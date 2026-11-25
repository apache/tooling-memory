//! Replay state: what a decision context and a tape header record so that a later
//! reader can say what the session could see, and what of that has moved since (the
//! paper's section 11 and the replay-state rules of its Appendix E).
//!
//! A [`ReplayManifest`] names the state by reference: per granted domain the offset of
//! its event log and how many active entries the index held, the version of the policy,
//! the digest of the grants, the session's settings, the ranker, and the digest of the T0
//! last served. The manifest on a decision context also lists the entries served since
//! the context before it. No body and no body digest is stored: the event log names the
//! version an entry had at an offset.
//!
//! A [`ReplayState`] compares a tape's starting manifest with the state now. A re-run is
//! answered from its tape while its calls match, so a record that moved changes nothing
//! for a run that keeps to the tape. A run that leaves the tape reads the record as it
//! is now; when the record moved, or the tape lost calls to an erasure, that run is
//! incomplete, and it is marked so. Nothing here puts current content in the place of
//! what was recorded.

use crate::arms::Settings;
use crate::tape::{Tape, TapeCall};
use crate::{log, Error, Memory};
use dm_core::{Label, ReadFilter};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// What a session could see at one moment, by reference.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReplayManifest {
    /// Per granted domain: the offset of its event log, which is the sequence number
    /// of the last event that names a version of an entry.
    pub log: BTreeMap<String, u64>,
    /// Per granted domain: how many active entries the index held, at every label.
    pub index: BTreeMap<String, u64>,
    /// The version of the policy the session had loaded.
    pub policy: String,
    /// The digest of the grants file the session was opened from.
    pub grants: String,
    /// The retrieval configuration: the session's settings.
    pub settings: Settings,
    /// The ranker and the summaries, which are the binary's: its name and version.
    pub ranker: String,
    /// The digest of the T0 last served to the session; none when none was.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub t0: Option<String>,
    /// On a decision context: the entries served since the context before it, each
    /// once, in the order they were first served.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub returned: Vec<String>,
}

/// One part of the state that is not what a manifest recorded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Moved {
    /// Which part: `log <domain>`, `policy`, `grants`, `settings` or `ranker`.
    pub what: String,
    /// What the manifest recorded.
    pub then: String,
    /// What it is now.
    pub now: String,
}

/// Whether a re-run that leaves the tape reads the state the session read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Availability {
    /// The starting state was recorded, nothing of it moved, and the tape has every
    /// call.
    Complete,
    /// The starting state was not recorded, or part of it moved, or an erasure removed
    /// calls from the tape.
    Incomplete,
}

/// The replay state of one tape.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayState {
    /// The session whose tape it is.
    pub session: String,
    /// The manifest the tape began with; none on a tape kept before manifests were.
    pub manifest: Option<ReplayManifest>,
    /// What moved since the manifest.
    pub moved: Vec<Moved>,
    /// The call numbers below the tape's highest that the tape does not hold.
    pub missing_calls: Vec<usize>,
    /// Whether the replay is complete.
    pub availability: Availability,
    /// Why it is incomplete, one sentence for each reason; empty when it is complete.
    pub why: Vec<String>,
}

/// The name of the ranker in a manifest. The ranker and the summaries are code of this
/// crate, so its version names them.
#[must_use]
pub fn ranker() -> String {
    format!("amem {}", env!("CARGO_PKG_VERSION"))
}

fn differs(out: &mut Vec<Moved>, what: &str, then: &str, now: &str) {
    if then != now {
        out.push(Moved {
            what: what.to_string(),
            then: then.to_string(),
            now: now.to_string(),
        });
    }
}

impl ReplayManifest {
    /// What of this manifest is different in `now`: the log offset of each domain the
    /// manifest names, the policy, the grants, the settings and the ranker. The index
    /// count follows the log, and the T0 and the returned entries are what a session
    /// was served, so those three are not compared.
    #[must_use]
    pub fn moved_since(&self, now: &ReplayManifest) -> Vec<Moved> {
        let mut out = Vec::new();
        for (domain, then) in &self.log {
            let at = now
                .log
                .get(domain)
                .map_or_else(|| "no log".to_string(), u64::to_string);
            differs(&mut out, &format!("log {domain}"), &then.to_string(), &at);
        }
        differs(&mut out, "policy", &self.policy, &now.policy);
        differs(&mut out, "grants", &self.grants, &now.grants);
        let settings = |s: &Settings| serde_json::to_string(s).unwrap_or_default();
        differs(
            &mut out,
            "settings",
            &settings(&self.settings),
            &settings(&now.settings),
        );
        differs(&mut out, "ranker", &self.ranker, &now.ranker);
        out
    }
}

/// The call numbers below the highest that `calls` does not hold. A session numbers its
/// calls from one without a gap, so a gap is a call that an erasure removed.
fn missing(calls: &[TapeCall]) -> Vec<usize> {
    let have: BTreeSet<usize> = calls.iter().map(|c| c.n).collect();
    let top = have.last().copied().unwrap_or(0);
    (1..top).filter(|n| !have.contains(n)).collect()
}

/// The replay state of `tape` against the state `now`.
#[must_use]
pub fn state_of(tape: &Tape, now: &ReplayManifest) -> ReplayState {
    let manifest = tape.header.start.manifest.clone();
    let moved = manifest
        .as_ref()
        .map(|m| m.moved_since(now))
        .unwrap_or_default();
    let missing_calls = missing(&tape.calls);
    let mut why = Vec::new();
    if manifest.is_none() {
        why.push(
            "The tape has no replay manifest: its starting state was not recorded.".to_string(),
        );
    }
    for m in &moved {
        why.push(format!(
            "The {} moved: it was {} and it is {} now.",
            m.what, m.then, m.now
        ));
    }
    if !missing_calls.is_empty() {
        let numbers: Vec<String> = missing_calls.iter().map(usize::to_string).collect();
        why.push(format!(
            "An erasure removed calls from the tape: {}.",
            numbers.join(", ")
        ));
    }
    let availability = if why.is_empty() {
        Availability::Complete
    } else {
        Availability::Incomplete
    };
    ReplayState {
        session: tape.header.session.clone(),
        manifest,
        moved,
        missing_calls,
        availability,
        why,
    }
}

impl Memory {
    /// The replay manifest of this session now. It reads the log offsets, the index
    /// counts and what the session holds; it reads no entry, logs no read and leaves the
    /// session's label where it is. The counts are of every label, so the manifest is
    /// kept only where the owner alone reads it: on a decision context and on a tape.
    ///
    /// # Errors
    ///
    /// The file system, or an index error.
    pub async fn replay_manifest(&self) -> Result<ReplayManifest, Error> {
        let mut offsets = BTreeMap::new();
        let mut counts = BTreeMap::new();
        {
            let ix = self.index.lock().await;
            for domain in self.domains() {
                offsets.insert(domain.clone(), log::offset(&self.root, &domain)?);
                let all = ReadFilter::for_domains([domain.as_str()], Label::Restricted);
                counts.insert(domain, ix.count(&all)?);
            }
        }
        Ok(ReplayManifest {
            log: offsets,
            index: counts,
            policy: self.policy().version().to_string(),
            grants: self.grants_stamp.clone(),
            settings: self.settings(),
            ranker: ranker(),
            t0: self.reads.last_t0(),
            returned: Vec::new(),
        })
    }

    /// The replay state of a session's tape: the manifest it began with, what of that
    /// state is different for this session now, and the calls an erasure removed. The
    /// settings compared are this session's, so a caller that re-runs the tape under
    /// the tape's settings takes those settings first. The owner's alone, as the tape
    /// is.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for anyone but the owner, [`Error::NotFound`] for a session
    /// with no tape, [`Error::Stale`] when this session's grants changed, or a store or
    /// index error.
    pub async fn replay_state(&self, session: &str) -> Result<ReplayState, Error> {
        let tape = self.tape(session).await?;
        self.fresh()?;
        Ok(state_of(&tape, &self.replay_manifest().await?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arms::Arm;
    use crate::tape::{TapeHeader, TapeStart};
    use crate::{reads, WriteRequest};
    use serde_json::json;

    fn req(body: &str) -> WriteRequest {
        WriteRequest {
            body: body.into(),
            ..WriteRequest::default()
        }
    }

    fn manifest() -> ReplayManifest {
        ReplayManifest {
            log: BTreeMap::from([("dom-a".to_string(), 4), ("dom-b".to_string(), 0)]),
            index: BTreeMap::from([("dom-a".to_string(), 3)]),
            policy: "policy-1".into(),
            grants: "grants-1".into(),
            settings: Settings::default(),
            ranker: "amem 0.1.0".into(),
            t0: Some("t0-1".into()),
            returned: vec!["mem-a-1".into()],
        }
    }

    fn call(n: usize) -> TapeCall {
        TapeCall {
            n,
            at: "2026-11-01T10:00:00Z".into(),
            tool: "t0".into(),
            arguments: json!({}),
            occurrence: n,
            label: Label::Public,
            result: json!({}),
            is_error: false,
        }
    }

    fn tape(manifest: Option<ReplayManifest>, calls: Vec<TapeCall>) -> Tape {
        Tape {
            header: TapeHeader {
                session: "sess-a".into(),
                domain: "dom-a".into(),
                started_at: "2026-11-01T10:00:00Z".into(),
                start: TapeStart {
                    manifest,
                    ..TapeStart::default()
                },
                pinned: false,
            },
            calls,
        }
    }

    #[test]
    fn replay_state_the_offset_of_a_log_is_the_number_of_its_last_version_event() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(log::offset(dir.path(), "dom-a").unwrap(), 0, "no log yet");
        let put = |id: &str| log::Change::Put {
            id: id.into(),
            digest: "d".into(),
            active: true,
        };
        log::append(dir.path(), "dom-a", "me", put("a")).unwrap();
        log::append(dir.path(), "dom-a", "me", put("b")).unwrap();
        log::append(dir.path(), "dom-b", "me", put("c")).unwrap();
        assert_eq!(log::offset(dir.path(), "dom-a").unwrap(), 2);
        assert_eq!(log::offset(dir.path(), "dom-b").unwrap(), 1, "per domain");
        // an event that names no version leaves the offset where it is
        let read = log::Change::Read {
            ids: vec!["a".into()],
            survey: false,
        };
        log::append(dir.path(), "dom-a", "me", read).unwrap();
        assert_eq!(log::offset(dir.path(), "dom-a").unwrap(), 2, "after a read");
        let erased = log::Change::Erased { id: "a".into() };
        log::append(dir.path(), "dom-a", "me", erased).unwrap();
        assert_eq!(log::offset(dir.path(), "dom-a").unwrap(), 4, "an erasure");
        assert!(
            log::offset(dir.path(), "../x").is_err(),
            "a name, not a path"
        );
    }

    #[test]
    fn replay_state_moved_since_names_each_part_that_differs() {
        let then = manifest();
        assert!(then.moved_since(&then).is_empty());
        // what a session was served, and the index count, are not compared
        let served = ReplayManifest {
            index: BTreeMap::new(),
            t0: None,
            returned: Vec::new(),
            ..manifest()
        };
        assert!(then.moved_since(&served).is_empty());
        let mut now = manifest();
        now.log.insert("dom-a".into(), 6);
        assert_eq!(
            then.moved_since(&now),
            [Moved {
                what: "log dom-a".into(),
                then: "4".into(),
                now: "6".into()
            }]
        );
        let what = |now: &ReplayManifest| -> Vec<String> {
            then.moved_since(now).into_iter().map(|m| m.what).collect()
        };
        let mut now = manifest();
        now.log.remove("dom-b");
        assert_eq!(what(&now), ["log dom-b"], "a domain with no log now");
        let now = ReplayManifest {
            policy: "policy-2".into(),
            ..manifest()
        };
        assert_eq!(what(&now), ["policy"]);
        let now = ReplayManifest {
            grants: "grants-2".into(),
            ..manifest()
        };
        assert_eq!(what(&now), ["grants"]);
        let now = ReplayManifest {
            settings: Arm::RawOnly.settings(),
            ..manifest()
        };
        assert_eq!(what(&now), ["settings"]);
        let now = ReplayManifest {
            ranker: "amem 0.2.0".into(),
            ..manifest()
        };
        assert_eq!(what(&now), ["ranker"]);
    }

    #[tokio::test]
    async fn replay_state_a_manifest_names_the_state_and_reads_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let owner = Memory::init(dir.path(), "human:test:me", "dom-me").unwrap();
        owner.write(req("The first fact.\n")).await.unwrap();
        owner.write(req("The second fact.\n")).await.unwrap();
        // a session that reads public entries only, in the raw-only arm
        let agent = Memory::open(dir.path())
            .unwrap()
            .with_author("agent:a")
            .with_handling_cap(Label::Public);
        agent.set_settings(Arm::RawOnly.settings()).unwrap();
        assert!(agent.list(None, None, 10).await.unwrap().is_empty());
        let reads_before = reads::all(dir.path()).unwrap().len();
        let label_before = agent.label();
        let m = agent.replay_manifest().await.unwrap();
        assert_eq!(
            reads::all(dir.path()).unwrap().len(),
            reads_before,
            "no read is logged"
        );
        assert_eq!(agent.label(), label_before, "the label stays");
        // each write is two events, the offer and the version: the second version is 4
        assert_eq!(m.log, BTreeMap::from([("dom-me".to_string(), 4)]));
        assert_eq!(
            m.index,
            BTreeMap::from([("dom-me".to_string(), 2)]),
            "every label is counted, also above the session's handling"
        );
        assert_eq!(m.policy, agent.policy().version());
        let grants = std::fs::read_to_string(dir.path().join(crate::GRANTS_FILE)).unwrap();
        assert_eq!(m.grants, dm_core::digest(&grants));
        assert_eq!(m.settings, Arm::RawOnly.settings());
        assert_eq!(m.ranker, format!("amem {}", env!("CARGO_PKG_VERSION")));
        assert_eq!(m.t0, None, "no T0 was served");
        assert!(m.returned.is_empty());
        let t0 = agent.t0(None).await.unwrap();
        let m = agent.replay_manifest().await.unwrap();
        assert_eq!(m.t0, Some(dm_core::digest(&t0)));
        // a read by the owner goes into the log and leaves the offset where it is
        let before = log::read(dir.path(), "dom-me", 0).unwrap().len();
        assert_eq!(owner.list(None, None, 10).await.unwrap().len(), 2);
        assert!(log::read(dir.path(), "dom-me", 0).unwrap().len() > before);
        let after = agent.replay_manifest().await.unwrap();
        assert_eq!(after.log, m.log, "a read moves no offset");
        // a session that may read the entries and has read none: its label stays too
        let fresh = Memory::open(dir.path()).unwrap().with_author("agent:b");
        fresh.replay_manifest().await.unwrap();
        assert_eq!(fresh.label(), Label::Public, "the label stays");
    }

    #[tokio::test]
    async fn replay_state_names_what_moved_since_the_tape_began() {
        let dir = tempfile::tempdir().unwrap();
        let owner = Memory::init(dir.path(), "human:test:me", "dom-me").unwrap();
        owner.write(req("The first fact.\n")).await.unwrap();
        let agent = Memory::open(dir.path()).unwrap().with_author("agent:a");
        agent.tape_start(TapeStart::default()).await.unwrap();
        let session = agent.session_id().to_string();
        let state = owner.replay_state(&session).await.unwrap();
        assert_eq!(state.session, session);
        assert_eq!(
            state.manifest,
            Some(agent.replay_manifest().await.unwrap()),
            "the manifest the tape began with"
        );
        assert!(state.moved.is_empty(), "{:?}", state.moved);
        assert!(state.missing_calls.is_empty());
        assert_eq!(state.availability, Availability::Complete);
        assert!(state.why.is_empty(), "{:?}", state.why);
        // the record moves
        owner.write(req("The second fact.\n")).await.unwrap();
        let state = owner.replay_state(&session).await.unwrap();
        assert_eq!(
            state.moved,
            [Moved {
                what: "log dom-me".into(),
                then: "2".into(),
                now: "4".into()
            }]
        );
        assert_eq!(state.availability, Availability::Incomplete);
        assert_eq!(
            state.why,
            ["The log dom-me moved: it was 2 and it is 4 now."]
        );
        // a caller under other settings than the tape's is told so
        let other = Memory::open(dir.path()).unwrap();
        other.set_settings(Arm::NoMemory.settings()).unwrap();
        let moved: Vec<String> = other
            .replay_state(&session)
            .await
            .unwrap()
            .moved
            .into_iter()
            .map(|m| m.what)
            .collect();
        assert_eq!(moved, ["log dom-me", "settings"]);
    }

    #[tokio::test]
    async fn replay_state_a_tape_with_no_manifest_or_with_calls_erased_is_incomplete() {
        let now = manifest();
        // a tape kept before manifests were
        let state = state_of(&tape(None, vec![call(1), call(2)]), &now);
        assert_eq!(state.availability, Availability::Incomplete);
        assert_eq!(state.manifest, None);
        assert_eq!(
            state.why,
            ["The tape has no replay manifest: its starting state was not recorded."]
        );
        // calls two and three are not there, below the highest
        let state = state_of(&tape(Some(manifest()), vec![call(4), call(1)]), &now);
        assert_eq!(state.missing_calls, [2, 3]);
        assert_eq!(state.availability, Availability::Incomplete);
        assert_eq!(state.why, ["An erasure removed calls from the tape: 2, 3."]);
        // every call there: complete
        let state = state_of(&tape(Some(manifest()), vec![call(1), call(2)]), &now);
        assert_eq!(state.availability, Availability::Complete);
        assert!(state.missing_calls.is_empty() && state.why.is_empty());
        // through an erasure
        let dir = tempfile::tempdir().unwrap();
        let m = Memory::init(dir.path(), "human:test:me", "dom-me").unwrap();
        let body = "The deploy key rotates every Friday at noon, and nobody writes it down.";
        m.tape_call("t0", &json!({}), &json!({"text": "T0"}), false)
            .await
            .unwrap();
        m.tape_call(
            "get",
            &json!({"id": "mem-1"}),
            &json!({"body": body}),
            false,
        )
        .await
        .unwrap();
        m.tape_call("list", &json!({}), &json!({"hits": []}), false)
            .await
            .unwrap();
        let session = m.session_id().to_string();
        assert_eq!(
            m.replay_state(&session).await.unwrap().availability,
            Availability::Complete
        );
        m.erase_from_tapes(body, "mem-nothere").await.unwrap();
        let state = m.replay_state(&session).await.unwrap();
        assert_eq!(state.missing_calls, [2]);
        assert_eq!(state.availability, Availability::Incomplete);
    }

    #[tokio::test]
    async fn replay_state_is_the_owners_alone() {
        let dir = tempfile::tempdir().unwrap();
        let owner = Memory::init(dir.path(), "human:test:me", "dom-me").unwrap();
        let agent = Memory::open(dir.path()).unwrap().with_author("agent:a");
        agent.tape_start(TapeStart::default()).await.unwrap();
        let session = agent.session_id().to_string();
        assert!(owner.replay_state(&session).await.is_ok());
        let refused = agent.replay_state(&session).await.unwrap_err();
        assert!(matches!(refused, Error::Denied(_)), "{refused}");
        assert!(matches!(
            owner.replay_state("sess-nothere").await,
            Err(Error::NotFound(_))
        ));
    }
}
