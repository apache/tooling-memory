//! Tapes: what each memory tool call of a session returned, kept so the session can be
//! re-enacted (the paper's live re-execution).
//!
//! A re-enactment runs a harness again on a session's original prompt, at the original
//! commit, to compare harness versions or models on the same task. It is not run against
//! a copy of the root: search ranks against the whole corpus, the ledger moves on, and a
//! copy does not scale. It is run against the tape. Each tool call the re-run makes that
//! matches the tape, by tool, arguments and occurrence, gets the recorded result and
//! writes nothing; the first call that does not match is the first divergence, and is
//! marked. After it, a read is served from the record as it is now, a write is captured
//! for the report and not applied, and a call that would reach outside is refused.
//! Nothing is substituted into the harness's model calls; the tape answers only at the
//! memory server, which the harness reaches as it always does.
//!
//! A tape is the owner's alone: it holds every result a session was served, bodies
//! included. It lives beside the session's domain's entries, never indexed or searched,
//! on the transcripts' clock unless pinned for a benchmark, and erasing an entry erases
//! the tape calls that carry it, as it erases decision contexts. A re-run keeps no tape
//! and no decision contexts of its own.

use crate::{lock, Error, Memory};
use dm_core::Label;
use dm_store::Area;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Mutex;
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};

/// The header's file name in a session's tape directory.
const HEADER: &str = "header.json";

/// What a session was started on: what a re-run needs to start the same way.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TapeStart {
    /// The harness, by its binary name.
    pub harness: String,
    /// The model, when the launch named one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The prompt of a headless run; none for an interactive one, which cannot be
    /// re-enacted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// The working tree's commit when the session started, when it was a git checkout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// Whether the working tree had changes not in that commit.
    #[serde(default)]
    pub dirty: bool,
}

/// A tape's header.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TapeHeader {
    /// The session.
    pub session: String,
    /// Its domain, where the tape is kept.
    pub domain: String,
    /// When the tape began.
    pub started_at: String,
    /// How the session was started; empty when it began without being told.
    #[serde(flatten)]
    pub start: TapeStart,
    /// Kept past the transcripts' clock, for a benchmark.
    #[serde(default)]
    pub pinned: bool,
}

/// One tool call, as the memory server answered it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TapeCall {
    /// Its place in the session, from one.
    pub n: usize,
    /// When.
    pub at: String,
    /// The tool.
    pub tool: String,
    /// The arguments, as the harness sent them.
    pub arguments: Value,
    /// Which call of this tool with these arguments it was, from one.
    pub occurrence: usize,
    /// The session's label after the call.
    pub label: Label,
    /// The result in full: a `CallToolResult`, or `{"error": ...}` for a refusal.
    pub result: Value,
    /// Whether the result was an error.
    pub is_error: bool,
}

/// A session's tape.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Tape {
    /// Its header.
    pub header: TapeHeader,
    /// Its calls, in order.
    pub calls: Vec<TapeCall>,
}

/// A tape in a listing: the header and how many calls.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Listed {
    /// The header.
    #[serde(flatten)]
    pub header: TapeHeader,
    /// How many calls.
    pub calls: usize,
}

/// The arguments as one canonical string: keys sorted at every depth, so the same
/// arguments in another order are the same call.
#[must_use]
pub fn canonical(v: &Value) -> String {
    fn sorted(v: &Value) -> Value {
        match v {
            Value::Object(o) => {
                let m: BTreeMap<&String, Value> = o.iter().map(|(k, v)| (k, sorted(v))).collect();
                Value::Object(m.into_iter().map(|(k, v)| (k.clone(), v)).collect())
            }
            Value::Array(a) => Value::Array(a.iter().map(sorted).collect()),
            other => other.clone(),
        }
    }
    sorted(v).to_string()
}

fn key(tool: &str, arguments: &Value) -> String {
    format!("{tool}\n{}", canonical(arguments))
}

fn call_name(n: usize) -> String {
    format!("{n:06}.json")
}

fn now() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default()
}

fn bad(e: &serde_json::Error) -> Error {
    Error::Transcript(format!("tape: {e}"))
}

/// What the recording side keeps between calls.
#[derive(Default)]
pub(crate) struct Recording {
    /// How many calls so far.
    n: usize,
    /// Calls so far, by tool and arguments.
    seen: BTreeMap<String, usize>,
    /// The domain the header went to, once it was written.
    domain: Option<String>,
}

/// What a tool is, for a call after the first divergence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// It reads the record: served as the record is now.
    Read,
    /// It reaches outside: refused.
    Outside,
    /// It writes the record or moves the ledger: captured, not applied.
    Mutation,
}

/// How a re-run's call is to be answered.
#[derive(Clone, Debug, PartialEq)]
pub enum Answer {
    /// From the tape: the recorded result, and the label the session had after it.
    Recorded {
        /// The result as recorded.
        result: Value,
        /// Whether it was an error.
        is_error: bool,
        /// The label after it.
        label: Label,
    },
    /// By the server, as the record is now.
    Live,
    /// Captured for the report, not applied.
    Captured,
    /// Refused, with why.
    Refused(String),
}

/// One call of a re-run, as the report shows it.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Step {
    /// Its place in the re-run, from one.
    pub n: usize,
    /// The tool.
    pub tool: String,
    /// The arguments.
    pub arguments: Value,
    /// The tape call it matched, by number.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched: Option<usize>,
    /// `tape`, `live`, `captured` or `refused`.
    pub served: String,
}

/// What a re-run did against its tape.
#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct Played {
    /// The session re-enacted.
    pub session: String,
    /// Calls on the tape.
    pub tape_calls: usize,
    /// The re-run's calls, in order.
    pub steps: Vec<Step>,
    /// Calls before the first that did not match: all of them when none diverged.
    pub steps_to_divergence: usize,
    /// Whether any call diverged.
    pub diverged: bool,
    /// The tools of the tape's calls, in order.
    pub tape_tools: Vec<String>,
}

/// A tape being played to a re-run.
pub struct Playback {
    session: String,
    calls: Vec<TapeCall>,
    /// Tape calls by tool and arguments, in order, by index.
    by_key: BTreeMap<String, Vec<usize>>,
    state: Mutex<PlayState>,
}

#[derive(Default)]
struct PlayState {
    used: BTreeMap<String, usize>,
    steps: Vec<Step>,
    diverged_at: Option<usize>,
}

impl Playback {
    /// A playback of `tape`.
    #[must_use]
    pub fn new(tape: Tape) -> Playback {
        let mut by_key: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (i, c) in tape.calls.iter().enumerate() {
            by_key
                .entry(key(&c.tool, &c.arguments))
                .or_default()
                .push(i);
        }
        Playback {
            session: tape.header.session,
            calls: tape.calls,
            by_key,
            state: Mutex::new(PlayState::default()),
        }
    }

    /// How to answer a call of `tool` with `arguments`, a tool of `kind`: from the tape
    /// when this is the next occurrence of the same call there, otherwise by kind, and
    /// the first such call is the first divergence.
    pub fn answer(&self, tool: &str, arguments: &Value, kind: Kind) -> Answer {
        let k = key(tool, arguments);
        let mut st = lock(&self.state);
        let used = st.used.get(&k).copied().unwrap_or(0);
        let hit = self.by_key.get(&k).and_then(|v| v.get(used)).copied();
        let n = st.steps.len() + 1;
        let (answer, matched, served) = match hit {
            Some(i) => {
                let c = &self.calls[i];
                st.used.insert(k, used + 1);
                let a = Answer::Recorded {
                    result: c.result.clone(),
                    is_error: c.is_error,
                    label: c.label,
                };
                (a, Some(c.n), "tape")
            }
            None => {
                if st.diverged_at.is_none() {
                    st.diverged_at = Some(n);
                }
                match kind {
                    Kind::Read => (Answer::Live, None, "live"),
                    Kind::Mutation => (Answer::Captured, None, "captured"),
                    Kind::Outside => (
                        Answer::Refused(format!(
                            "re-enactment of {}: {tool} is not on the tape, and a re-run makes no new calls outside",
                            self.session
                        )),
                        None,
                        "refused",
                    ),
                }
            }
        };
        st.steps.push(Step {
            n,
            tool: tool.to_string(),
            arguments: arguments.clone(),
            matched,
            served: served.to_string(),
        });
        answer
    }

    /// What the re-run did so far.
    #[must_use]
    pub fn played(&self) -> Played {
        let st = lock(&self.state);
        Played {
            session: self.session.clone(),
            tape_calls: self.calls.len(),
            steps: st.steps.clone(),
            steps_to_divergence: st.diverged_at.map_or(st.steps.len(), |n| n - 1),
            diverged: st.diverged_at.is_some(),
            tape_tools: self.calls.iter().map(|c| c.tool.clone()).collect(),
        }
    }
}

impl Memory {
    /// The domain this session's records go to: the domain of the task it holds, else
    /// the only granted one; none when there is neither.
    pub(crate) async fn recording_domain(&self) -> Option<String> {
        match self.held.get() {
            Some(id) => self
                .ledger
                .lock()
                .await
                .get(id)
                .await
                .ok()
                .map(|t| t.spec.domain),
            None => self.resolve_domain(None).ok(),
        }
    }

    /// Re-enact against `playback`: from here on this session's tool calls are answered
    /// from it, and it keeps no tape and no decision contexts of its own.
    pub fn set_playback(&self, playback: std::sync::Arc<Playback>) {
        let _ = self.playback.set(playback);
    }

    /// The playback this session re-enacts, when it does.
    pub fn playback(&self) -> Option<&std::sync::Arc<Playback>> {
        self.playback.get()
    }

    /// Say how this session was started, at the head of its tape.
    ///
    /// # Errors
    ///
    /// A store error.
    pub async fn tape_start(&self, start: TapeStart) -> Result<(), Error> {
        if self.playback.get().is_some() {
            return Ok(());
        }
        let Some(domain) = self.recording_domain().await else {
            return Ok(());
        };
        self.put_header(&TapeHeader {
            session: self.session_id.clone(),
            domain: domain.clone(),
            started_at: now(),
            start,
            pinned: false,
        })
        .await?;
        lock(&self.recording).domain = Some(domain);
        Ok(())
    }

    async fn put_header(&self, h: &TapeHeader) -> Result<(), Error> {
        let bytes = serde_json::to_vec_pretty(h).map_err(|e| bad(&e))?;
        self.store
            .put_object(&h.domain, Area::Tapes, &[&h.session, HEADER], bytes)
            .await?;
        Ok(())
    }

    /// Keep one tool call and its result on this session's tape. A re-run keeps none.
    ///
    /// # Errors
    ///
    /// A store error.
    pub async fn tape_call(
        &self,
        tool: &str,
        arguments: &Value,
        result: &Value,
        is_error: bool,
    ) -> Result<(), Error> {
        if self.playback.get().is_some() {
            return Ok(());
        }
        let known = lock(&self.recording).domain.clone();
        let domain = match known {
            Some(d) => d,
            None => {
                let Some(d) = self.recording_domain().await else {
                    return Ok(());
                };
                // a session that began without saying how: a header all the same
                self.put_header(&TapeHeader {
                    session: self.session_id.clone(),
                    domain: d.clone(),
                    started_at: now(),
                    start: TapeStart::default(),
                    pinned: false,
                })
                .await?;
                lock(&self.recording).domain = Some(d.clone());
                d
            }
        };
        let (n, occurrence) = {
            let mut r = lock(&self.recording);
            r.n += 1;
            let n = r.n;
            let seen = r.seen.entry(key(tool, arguments)).or_default();
            *seen += 1;
            (n, *seen)
        };
        let c = TapeCall {
            n,
            at: now(),
            tool: tool.to_string(),
            arguments: arguments.clone(),
            occurrence,
            label: self.label(),
            result: result.clone(),
            is_error,
        };
        let bytes = serde_json::to_vec(&c).map_err(|e| bad(&e))?;
        self.store
            .put_object(
                &domain,
                Area::Tapes,
                &[&self.session_id, &call_name(n)],
                bytes,
            )
            .await?;
        Ok(())
    }

    fn owner_only(&self, what: &str) -> Result<(), Error> {
        if self.author == self.grants.owner {
            Ok(())
        } else {
            Err(Error::Denied(format!(
                "{what} are the owner's: {}",
                self.grants.owner
            )))
        }
    }

    /// The sessions that kept a tape, by domain.
    ///
    /// # Errors
    ///
    /// A store error.
    pub async fn tape_sessions(&self) -> Result<Vec<(String, String)>, Error> {
        let mut out = Vec::new();
        for domain in self.domains() {
            for s in self.store.objects(&domain, Area::Tapes, &[], true).await? {
                out.push((domain.clone(), s));
            }
        }
        Ok(out)
    }

    async fn header_in(&self, domain: &str, session: &str) -> Result<Option<TapeHeader>, Error> {
        match self
            .store
            .get_object(domain, Area::Tapes, &[session, HEADER])
            .await?
        {
            Some(t) => Ok(Some(serde_json::from_str(&t).map_err(|e| bad(&e))?)),
            None => Ok(None),
        }
    }

    /// Every tape the record keeps, the owner's view: how each session was started,
    /// how many calls, and whether it is pinned.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for anyone but the owner, or a store error.
    pub async fn tapes(&self) -> Result<Vec<Listed>, Error> {
        self.owner_only("tapes")?;
        let mut out = Vec::new();
        for (domain, session) in self.tape_sessions().await? {
            let Some(header) = self.header_in(&domain, &session).await? else {
                continue;
            };
            let calls = self
                .store
                .objects(&domain, Area::Tapes, &[&session], false)
                .await?
                .iter()
                .filter(|n| n.as_str() != HEADER)
                .count();
            out.push(Listed { header, calls });
        }
        out.sort_by(|a, b| b.header.started_at.cmp(&a.header.started_at));
        Ok(out)
    }

    /// A session's tape, in order. The owner's alone.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for anyone but the owner, [`Error::NotFound`] for a session
    /// with no tape, or a store error.
    pub async fn tape(&self, session: &str) -> Result<Tape, Error> {
        self.owner_only("tapes")?;
        for domain in self.domains() {
            let Some(header) = self.header_in(&domain, session).await? else {
                continue;
            };
            let mut calls = Vec::new();
            for n in self
                .store
                .objects(&domain, Area::Tapes, &[session], false)
                .await?
            {
                if n == HEADER {
                    continue;
                }
                if let Some(text) = self
                    .store
                    .get_object(&domain, Area::Tapes, &[session, &n])
                    .await?
                {
                    calls.push(serde_json::from_str::<TapeCall>(&text).map_err(|e| bad(&e))?);
                }
            }
            calls.sort_by_key(|c| c.n);
            return Ok(Tape { header, calls });
        }
        Err(Error::NotFound(format!("no tape for {session}")))
    }

    /// Pin a tape past the transcripts' clock, for a benchmark, or let it go again.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for anyone but the owner, [`Error::NotFound`], or a store
    /// error.
    pub async fn pin_tape(&self, session: &str, pinned: bool) -> Result<TapeHeader, Error> {
        self.owner_only("tapes")?;
        for domain in self.domains() {
            if let Some(mut h) = self.header_in(&domain, session).await? {
                h.pinned = pinned;
                self.put_header(&h).await?;
                return Ok(h);
            }
        }
        Err(Error::NotFound(format!("no tape for {session}")))
    }

    /// Drop the tapes whose last call is older than their domain's transcript clock,
    /// unless pinned. Returns the sessions swept.
    ///
    /// # Errors
    ///
    /// A store error.
    pub async fn sweep_tapes(&self, at: OffsetDateTime) -> Result<Vec<String>, Error> {
        let mut swept = Vec::new();
        for (domain, session) in self.tape_sessions().await? {
            let Some(h) = self.header_in(&domain, &session).await? else {
                continue;
            };
            if h.pinned {
                continue;
            }
            let days = self
                .grants
                .domains
                .get(&domain)
                .map_or(dm_policy::DEFAULT_TRANSCRIPT_RETENTION_DAYS, |g| {
                    g.transcript_retention_days()
                });
            let names = self
                .store
                .objects(&domain, Area::Tapes, &[&session], false)
                .await?;
            let mut last = OffsetDateTime::parse(&h.started_at, &Rfc3339).ok();
            if let Some(n) = names.iter().rfind(|n| n.as_str() != HEADER) {
                if let Some(text) = self
                    .store
                    .get_object(&domain, Area::Tapes, &[&session, n])
                    .await?
                {
                    let c: Option<TapeCall> = serde_json::from_str(&text).ok();
                    last = c
                        .and_then(|c| OffsetDateTime::parse(&c.at, &Rfc3339).ok())
                        .or(last);
                }
            }
            if last.is_some_and(|l| at >= l + Duration::days(i64::from(days))) {
                for n in &names {
                    self.store
                        .remove_object(&domain, Area::Tapes, &[&session, n])
                        .await?;
                }
                swept.push(session);
            }
        }
        Ok(swept)
    }

    /// Erase every tape call that carries `body`, as an erased entry's text must not
    /// outlive it in what a session was served; returns the calls removed, as
    /// `<session>/<n>`. Matched as decision contexts are.
    ///
    /// # Errors
    ///
    /// A store error.
    pub async fn erase_from_tapes(&self, body: &str) -> Result<Vec<String>, Error> {
        let probe: String = body.trim().chars().take(160).collect();
        if probe.chars().count() < 24 {
            return Ok(Vec::new());
        }
        let escaped = serde_json::to_string(&probe).unwrap_or_default();
        let escaped = escaped.trim_matches('"').to_string();
        // the result's text is itself JSON inside a JSON string: escaped twice
        let twice = serde_json::to_string(&escaped).unwrap_or_default();
        let twice = twice.trim_matches('"').to_string();
        let mut removed = Vec::new();
        for (domain, session) in self.tape_sessions().await? {
            for n in self
                .store
                .objects(&domain, Area::Tapes, &[&session], false)
                .await?
            {
                if n == HEADER {
                    continue;
                }
                let Some(text) = self
                    .store
                    .get_object(&domain, Area::Tapes, &[&session, &n])
                    .await?
                else {
                    continue;
                };
                if text.contains(&probe) || text.contains(&escaped) || text.contains(&twice) {
                    self.store
                        .remove_object(&domain, Area::Tapes, &[&session, &n])
                        .await?;
                    removed.push(format!("{session}/{n}"));
                }
            }
        }
        Ok(removed)
    }

    /// Whether this session keeps decision contexts: a re-run keeps none.
    pub(crate) fn keeps_contexts(&self) -> bool {
        self.playback.get().is_none()
    }

    /// Raise this session's label as a recorded call raised the original's.
    pub fn observe_recorded(&self, label: Label) -> bool {
        lock(&self.session).observe(label)
    }

    /// Calls kept on this session's tape so far.
    #[must_use]
    pub fn taped(&self) -> usize {
        lock(&self.recording).n
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(n: usize, tool: &str, arguments: Value, occurrence: usize) -> TapeCall {
        TapeCall {
            n,
            at: now(),
            tool: tool.into(),
            arguments,
            occurrence,
            label: Label::Internal,
            result: json!({ "content": [], "n": n }),
            is_error: false,
        }
    }

    fn tape(calls: Vec<TapeCall>) -> Tape {
        Tape {
            header: TapeHeader {
                session: "sess-a".into(),
                domain: "dom-me".into(),
                started_at: now(),
                start: TapeStart::default(),
                pinned: false,
            },
            calls,
        }
    }

    #[test]
    fn the_same_arguments_in_another_order_are_the_same_call() {
        assert_eq!(
            canonical(&json!({"b": 1, "a": {"d": [2, {"y": 1, "x": 0}], "c": 3}})),
            canonical(&json!({"a": {"c": 3, "d": [2, {"x": 0, "y": 1}]}, "b": 1}))
        );
        assert_ne!(
            canonical(&json!({"a": [1, 2]})),
            canonical(&json!({"a": [2, 1]}))
        );
    }

    #[test]
    fn each_occurrence_is_answered_once_and_the_first_miss_is_the_divergence() {
        let p = Playback::new(tape(vec![
            call(1, "search", json!({"query": "import"}), 1),
            call(2, "get", json!({"id": "mem-1"}), 1),
            call(3, "search", json!({"query": "import"}), 2),
        ]));
        let n = |a: &Answer| match a {
            Answer::Recorded { result, .. } => result["n"].as_u64(),
            _ => None,
        };
        let q = json!({"query": "import"});
        assert_eq!(n(&p.answer("search", &q, Kind::Read)), Some(1));
        assert_eq!(
            n(&p.answer("search", &q, Kind::Read)),
            Some(3),
            "the second"
        );
        assert_eq!(p.answer("search", &q, Kind::Read), Answer::Live, "no third");
        assert_eq!(
            n(&p.answer("get", &json!({"id": "mem-1"}), Kind::Read)),
            Some(2)
        );
        assert_eq!(
            p.answer("write", &json!({"body": "x"}), Kind::Mutation),
            Answer::Captured
        );
        assert!(matches!(
            p.answer("fetch", &json!({"url": "u"}), Kind::Outside),
            Answer::Refused(_)
        ));
        let played = p.played();
        assert_eq!((played.steps_to_divergence, played.diverged), (2, true));
        assert_eq!(
            played.steps[3].matched,
            Some(2),
            "matched after the divergence"
        );
        assert_eq!(played.tape_tools, ["search", "get", "search"]);
    }

    #[tokio::test]
    async fn a_tape_is_swept_on_the_clock_unless_pinned_and_erasure_reaches_it() {
        let dir = tempfile::tempdir().unwrap();
        let m = Memory::init(dir.path(), "human:test:me", "dom-me").unwrap();
        let body = "The deploy key rotates every Friday at noon, and nobody writes it down.";
        m.tape_call(
            "get",
            &json!({"id": "mem-1"}),
            &json!({"body": body}),
            false,
        )
        .await
        .unwrap();
        m.tape_call("t0", &json!({}), &json!({"text": "T0"}), false)
            .await
            .unwrap();
        let session = m.session_id().to_string();
        let listed = m.tapes().await.unwrap();
        assert_eq!((listed.len(), listed[0].calls), (1, 2));
        assert!(
            listed[0].header.start.prompt.is_none(),
            "begun without saying how"
        );
        // erasure takes the call that carried the body, and only it
        assert_eq!(
            m.erase_from_tapes(body).await.unwrap(),
            [format!("{session}/000001.json")]
        );
        assert_eq!(m.tape(&session).await.unwrap().calls.len(), 1);
        // the clock: thirty days by default; pinned, kept
        let later = OffsetDateTime::now_utc() + Duration::days(31);
        m.pin_tape(&session, true).await.unwrap();
        assert!(m.sweep_tapes(later).await.unwrap().is_empty());
        m.pin_tape(&session, false).await.unwrap();
        assert!(m
            .sweep_tapes(OffsetDateTime::now_utc())
            .await
            .unwrap()
            .is_empty());
        assert_eq!(m.sweep_tapes(later).await.unwrap(), vec![session.clone()]);
        assert!(m.tape(&session).await.is_err());
    }

    #[tokio::test]
    async fn a_re_run_keeps_no_tape_and_no_contexts() {
        let dir = tempfile::tempdir().unwrap();
        let m = Memory::init(dir.path(), "human:test:me", "dom-me").unwrap();
        m.set_playback(std::sync::Arc::new(Playback::new(tape(Vec::new()))));
        m.tape_start(TapeStart::default()).await.unwrap();
        m.tape_call("t0", &json!({}), &json!({}), false)
            .await
            .unwrap();
        m.keep_context("anthropic", "/v1/messages", b"{}", b"{}")
            .await
            .unwrap();
        assert!(m.tape_sessions().await.unwrap().is_empty());
        assert!(m.context_sessions().await.unwrap().is_empty());
    }
}
