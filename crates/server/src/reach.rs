//! Raw reach on the writer's side: what counts as content from authors outside the
//! fleet, and which sessions read it.
//!
//! A session reads outside content when it is served an answer of a tool, a raw entry
//! with tap reach, or a task that a tap or such a session filed. From then on each event
//! that the session appends says so, and each entry that it writes has tap reach
//! ([`Memory::read_outside`]). The record blocks no other action of the session.
//! [`Memory::outside_readers`] gives those sessions from the log, with what each wrote
//! and sent after the read.

use crate::log::Change;
use crate::{Error, Memory};
use dm_core::{Entry, Reach, TrustTier};
use serde::Serialize;

/// Whether an entry at this tier and with this reach is content from authors outside
/// the fleet. An entry at curated or verified is not, whatever its reach.
pub(crate) fn raw_tap(tier: TrustTier, reach: Reach) -> bool {
    tier == TrustTier::Raw && reach == Reach::Tap
}

/// Whether this version of an entry is content from authors outside the fleet.
pub(crate) fn outside_content(entry: &Entry) -> bool {
    raw_tap(entry.front.trust_tier, entry.front.reach)
}

/// A session that read content from authors outside the fleet, as one log shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct OutsideReader {
    /// The session.
    pub session: String,
    /// Its author.
    pub by: String,
    /// The sequence number of its first event after the read.
    pub since: u64,
    /// The entries that it offered after the read, in the order of the log.
    pub writes: Vec<String>,
    /// What it sent after the read, in the order of the log.
    pub sends: Vec<OutsideSend>,
}

/// One send of a session that had read outside content.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct OutsideSend {
    /// The sequence number of the `send` event.
    pub seq: u64,
    /// When.
    pub at: String,
    /// Where it went.
    pub to: String,
    /// The digest of what went.
    pub digest: String,
}

impl Memory {
    /// The sessions that the log of a domain shows as readers of outside content, in the
    /// order of the first event of each after the read. Each has the entries that it
    /// offered and the sends that it made after the read. The log is read as
    /// [`Memory::log_synced`] reads it: for the security role only, without the events
    /// above the handling of this session, and the label of this session rises.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for an author outside the security role or an ungranted domain;
    /// a ledger error, or the file system.
    pub async fn outside_readers(&self, domain: Option<&str>) -> Result<Vec<OutsideReader>, Error> {
        let events = self.log_synced(domain, 0).await?;
        // sequence numbers rise through the log, so the first event of each session
        // puts the sessions in order
        let mut readers: Vec<OutsideReader> = Vec::new();
        for e in events.into_iter().filter(|e| e.outside) {
            let Some(session) = e.session else {
                continue;
            };
            let at = match readers.iter().position(|r| r.session == session) {
                Some(at) => at,
                None => {
                    readers.push(OutsideReader {
                        session,
                        by: e.by,
                        since: e.seq,
                        writes: Vec::new(),
                        sends: Vec::new(),
                    });
                    readers.len() - 1
                }
            };
            match e.change {
                Change::Write { id, .. } => readers[at].writes.push(id),
                Change::Send { to, digest } => readers[at].sends.push(OutsideSend {
                    seq: e.seq,
                    at: e.at,
                    to,
                    digest,
                }),
                _ => {}
            }
        }
        Ok(readers)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::{read, Event};
    use crate::sources::{Answer, Answered, Tools};
    use crate::{lock, WriteRequest, Written};
    use dm_core::{EntryType, Label};
    use dm_ledger::{Status, Task, TaskSpec};
    use serde_json::{json, Value};
    use std::path::Path;
    use std::sync::Arc;
    use time::Duration;

    const OWNER: &str = "human:test:me";

    fn owner(root: &Path) -> Memory {
        Memory::init(root, OWNER, "dom-me").unwrap()
    }

    fn agent(root: &Path, name: &str) -> Memory {
        Memory::open(root).unwrap().with_author(name)
    }

    fn req(kind: EntryType, body: &str) -> WriteRequest {
        WriteRequest {
            entry_type: kind,
            body: body.into(),
            ..WriteRequest::default()
        }
    }

    async fn wrote(m: &Memory, kind: EntryType) -> Written {
        m.write(req(kind, "# Steps\n\nFive planted steps.\n"))
            .await
            .unwrap()
    }

    fn spec(title: &str) -> TaskSpec {
        TaskSpec {
            title: title.into(),
            body: "Do it.\n".into(),
            ..TaskSpec::default()
        }
    }

    /// A raw entry with tap reach in `dom-me`. The session that writes it is given the
    /// record directly, so that a test of one way to read outside content does not rest
    /// on another way.
    async fn tap_entry(root: &Path) -> String {
        let planted = agent(root, "agent:planted");
        lock(&planted.session).read_outside();
        let w = wrote(&planted, EntryType::Fact).await;
        assert_eq!(w.reach, Reach::Tap);
        w.id
    }

    /// A task that a tap filed, admitted by a session of the owner.
    async fn tap_task(root: &Path, title: &str) -> Task {
        let filed = agent(root, "tap:issues")
            .create_task(spec(title))
            .await
            .unwrap()
            .task;
        Memory::open(root).unwrap().admit(&filed.id).await.unwrap()
    }

    /// The lines of the log of `dom-me`, each as JSON and as the event it reads as.
    fn lines(root: &Path) -> Vec<(Value, Event)> {
        let text =
            std::fs::read_to_string(root.join(crate::log::DIR).join("dom-me.jsonl")).unwrap();
        text.lines()
            .map(|l| {
                (
                    serde_json::from_str(l).unwrap(),
                    serde_json::from_str(l).unwrap(),
                )
            })
            .collect()
    }

    /// The events of one session in the log of `dom-me`, each as its kind and whether
    /// its line has the `outside` key.
    fn marks(root: &Path, session: &str) -> Vec<(String, bool)> {
        lines(root)
            .into_iter()
            .filter(|(_, e)| e.session.as_deref() == Some(session))
            .map(|(v, e)| {
                assert_eq!(v.get("outside").is_some(), e.outside, "{v}");
                (v["kind"].as_str().unwrap().to_string(), e.outside)
            })
            .collect()
    }

    /// Whether an event is a read of `id` and of nothing else.
    fn reads_only(e: &Event, id: &str) -> bool {
        matches!(&e.change, Change::Read { ids } if ids.len() == 1 && ids[0] == id)
    }

    /// A tool that answers with one text.
    struct Answering;

    impl Tools for Answering {
        fn call(
            &self,
            _memory: Arc<Memory>,
            _tool: String,
            _config: String,
            _args: String,
        ) -> Answer<'static> {
            Box::pin(async {
                Ok(Answered {
                    answer: json!({"text": "The port is 8443."}).to_string(),
                    reached: Vec::new(),
                })
            })
        }
    }

    /// Grant the tool `lookup` in `dom-me`.
    fn grant_lookup(root: &Path) {
        let path = root.join(crate::GRANTS_FILE);
        let mut g = dm_policy::Grants::load(&path).unwrap();
        g.taps.insert(
            "lookup".into(),
            dm_policy::TapGrant {
                domain: "dom-me".into(),
                tool: true,
                ceiling: Label::Public,
                ..dm_policy::TapGrant::default()
            },
        );
        std::fs::write(&path, g.render().unwrap()).unwrap();
    }

    fn with_tool(root: &Path, name: &str) -> Arc<Memory> {
        let m = Arc::new(agent(root, name));
        m.set_tools(Arc::new(Answering));
        m
    }

    #[tokio::test]
    async fn a_write_with_nothing_read_from_outside_and_no_task_has_fleet_reach_and_no_task() {
        let dir = tempfile::tempdir().unwrap();
        drop(owner(dir.path()));
        let a = agent(dir.path(), "agent:plain");
        for kind in [EntryType::Fact, EntryType::Handoff, EntryType::Decision] {
            let w = wrote(&a, kind).await;
            assert_eq!((w.reach, &w.task), (Reach::Fleet, &None), "{kind:?}");
            let told = serde_json::to_value(&w).unwrap();
            assert!(told.get("reach").is_none() && told.get("task").is_none());
            let text = a.get(&w.id).await.unwrap().render();
            assert!(
                !text
                    .lines()
                    .any(|l| l.starts_with("reach:") || l.starts_with("task:")),
                "{text}"
            );
        }
        assert!(!a.read_outside());
    }

    #[tokio::test]
    async fn under_a_held_task_a_handoff_and_a_decision_have_chain_reach_and_a_fact_has_fleet() {
        let dir = tempfile::tempdir().unwrap();
        let o = owner(dir.path());
        let held = o.file_own(spec("Ship it")).await.unwrap().task;
        let a = agent(dir.path(), "agent:worker");
        a.set_held_task(&held.id);
        for (kind, reach) in [
            (EntryType::Handoff, Reach::Chain),
            (EntryType::Decision, Reach::Chain),
            (EntryType::Fact, Reach::Fleet),
        ] {
            let w = wrote(&a, kind).await;
            assert_eq!(w.reach, reach, "{kind:?}");
            assert_eq!(w.task.as_deref(), Some(held.id.as_str()), "{kind:?}");
            let stored = a.get(&w.id).await.unwrap().front;
            assert_eq!(stored.reach, reach, "{kind:?}");
            assert_eq!(stored.task.as_deref(), Some(held.id.as_str()), "{kind:?}");
        }
    }

    #[tokio::test]
    async fn a_write_after_a_tool_answer_has_tap_reach_and_the_answer_leaves_a_read_event() {
        let dir = tempfile::tempdir().unwrap();
        drop(owner(dir.path()));
        grant_lookup(dir.path());
        let a = with_tool(dir.path(), "agent:asks");
        let before = wrote(&a, EntryType::Fact).await;
        assert_eq!(before.reach, Reach::Fleet);
        let answer = a.use_tool("lookup", json!({"q": "port"})).await.unwrap();
        assert!(a.read_outside());
        let source = answer["cite_as"].as_str().unwrap().to_string();
        assert!(source.starts_with("source:"), "{answer}");
        let read_events: Vec<Event> = read(dir.path(), "dom-me", 0)
            .unwrap()
            .into_iter()
            .filter(|e| reads_only(e, &source))
            .collect();
        assert_eq!(read_events.len(), 1, "one read event names the source");
        assert_eq!(read_events[0].session.as_deref(), Some(a.session_id()));
        assert!(read_events[0].outside);
        assert_eq!(wrote(&a, EntryType::Fact).await.reach, Reach::Tap);
        // a held task does not give chain reach to a session that read outside content
        a.set_held_task("task-held-1");
        let handoff = wrote(&a, EntryType::Handoff).await;
        assert_eq!(handoff.reach, Reach::Tap);
        assert_eq!(handoff.task.as_deref(), Some("task-held-1"));
    }

    #[tokio::test]
    async fn a_write_after_a_tool_answer_from_a_kept_source_has_tap_reach() {
        let dir = tempfile::tempdir().unwrap();
        drop(owner(dir.path()));
        grant_lookup(dir.path());
        let first = with_tool(dir.path(), "agent:first");
        let fresh = first
            .use_tool("lookup", json!({"q": "port"}))
            .await
            .unwrap();
        assert_eq!(fresh["cached"], json!(false), "{fresh}");
        let second = with_tool(dir.path(), "agent:second");
        let kept = second
            .use_tool("lookup", json!({"q": "port"}))
            .await
            .unwrap();
        assert_eq!(kept["cached"], json!(true), "{kept}");
        assert!(second.read_outside());
        assert_eq!(wrote(&second, EntryType::Fact).await.reach, Reach::Tap);
        let source = kept["cite_as"].as_str().unwrap().to_string();
        let readers: Vec<String> = read(dir.path(), "dom-me", 0)
            .unwrap()
            .into_iter()
            .filter(|e| reads_only(e, &source))
            .filter_map(|e| e.session)
            .collect();
        assert_eq!(
            readers,
            [first.session_id(), second.session_id()],
            "each session that was served the answer has a read event that names the source"
        );
    }

    #[tokio::test]
    async fn a_tool_failure_that_is_told_to_the_agent_is_outside_content() {
        struct Failing;
        impl Tools for Failing {
            fn call(&self, _: Arc<Memory>, _: String, _: String, _: String) -> Answer<'static> {
                Box::pin(async { Err("upstream 503: try again later".to_string()) })
            }
        }
        let dir = tempfile::tempdir().unwrap();
        drop(owner(dir.path()));
        grant_lookup(dir.path());
        let a = Arc::new(agent(dir.path(), "agent:asks"));
        a.set_tools(Arc::new(Failing));
        assert!(a.use_tool("lookup", json!({"q": "port"})).await.is_err());
        assert!(a.read_outside());
        assert_eq!(wrote(&a, EntryType::Fact).await.reach, Reach::Tap);
    }

    #[tokio::test]
    async fn a_write_after_a_search_served_a_raw_entry_with_tap_reach_has_tap_reach() {
        let dir = tempfile::tempdir().unwrap();
        drop(owner(dir.path()));
        tap_entry(dir.path()).await;
        let a = agent(dir.path(), "agent:searches");
        let hits = a.search("planted", None, 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert!(a.read_outside());
        assert_eq!(wrote(&a, EntryType::Fact).await.reach, Reach::Tap);
    }

    #[tokio::test]
    async fn a_write_after_a_list_served_a_raw_entry_with_tap_reach_has_tap_reach() {
        let dir = tempfile::tempdir().unwrap();
        drop(owner(dir.path()));
        tap_entry(dir.path()).await;
        let a = agent(dir.path(), "agent:lists");
        assert_eq!(a.list(None, None, 10).await.unwrap().len(), 1);
        assert!(a.read_outside());
        assert_eq!(wrote(&a, EntryType::Fact).await.reach, Reach::Tap);
    }

    #[tokio::test]
    async fn a_write_after_a_get_served_a_raw_entry_with_tap_reach_has_tap_reach() {
        let dir = tempfile::tempdir().unwrap();
        drop(owner(dir.path()));
        let id = tap_entry(dir.path()).await;
        let a = agent(dir.path(), "agent:gets");
        a.get(&id).await.unwrap();
        assert!(a.read_outside());
        assert_eq!(wrote(&a, EntryType::Fact).await.reach, Reach::Tap);
    }

    #[tokio::test]
    async fn a_curated_entry_with_tap_reach_is_not_outside_content() {
        let dir = tempfile::tempdir().unwrap();
        let o = owner(dir.path());
        let id = tap_entry(dir.path()).await;
        o.promote(&id, TrustTier::Curated, "Checked against the runbook.")
            .await
            .unwrap();
        let a = agent(dir.path(), "agent:after");
        let hits = a.search("planted", None, 10).await.unwrap();
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(
            (hits[0].trust_tier, hits[0].reach),
            (TrustTier::Curated, Reach::Tap)
        );
        assert_eq!(a.list(None, None, 10).await.unwrap().len(), 1);
        a.get(&hits[0].id).await.unwrap();
        assert!(!a.read_outside());
        assert_eq!(wrote(&a, EntryType::Fact).await.reach, Reach::Fleet);
    }

    #[tokio::test]
    async fn a_task_that_a_tap_files_is_outside_and_a_task_of_the_owner_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let o = owner(dir.path());
        let from_tap = tap_task(dir.path(), "From the tracker").await;
        assert!(from_tap.spec.outside);
        let own = o.file_own(spec("Own work")).await.unwrap().task;
        assert!(!own.spec.outside);
        let a = agent(dir.path(), "agent:reads");
        a.get_task(&own.id).await.unwrap();
        assert!(!a.read_outside(), "a task of the fleet");
        assert_eq!(wrote(&a, EntryType::Fact).await.reach, Reach::Fleet);
    }

    #[tokio::test]
    async fn a_task_of_a_tap_that_does_not_say_it_is_outside_is_outside_content() {
        let dir = tempfile::tempdir().unwrap();
        let o = owner(dir.path());
        // as a ledger from before the field holds a task of a tap
        let old = TaskSpec {
            domain: "dom-me".into(),
            created_by: "tap:issues".into(),
            ..spec("Filed before")
        };
        let task = o.ledger.lock().await.create(old).await.unwrap().task;
        assert!(!task.spec.outside);
        let a = agent(dir.path(), "agent:gets");
        a.get_task(&task.id).await.unwrap();
        assert!(a.read_outside());
        let f = a
            .fan_out(&task.id, &[("Child".into(), "Do the child.".into())])
            .await
            .unwrap();
        assert!(f.children[0].spec.outside);
    }

    #[tokio::test]
    async fn a_write_after_get_task_gave_an_outside_task_has_tap_reach() {
        let dir = tempfile::tempdir().unwrap();
        drop(owner(dir.path()));
        let task = tap_task(dir.path(), "From the tracker").await;
        let a = agent(dir.path(), "agent:gets");
        a.get_task(&task.id).await.unwrap();
        assert!(a.read_outside());
        assert_eq!(wrote(&a, EntryType::Fact).await.reach, Reach::Tap);
    }

    #[tokio::test]
    async fn a_write_after_a_claim_of_an_outside_task_has_tap_reach() {
        let dir = tempfile::tempdir().unwrap();
        drop(owner(dir.path()));
        let named = tap_task(dir.path(), "Named").await;
        let a = agent(dir.path(), "agent:claims");
        a.claim(&named.id, Duration::minutes(30)).await.unwrap();
        assert!(a.read_outside());
        assert_eq!(wrote(&a, EntryType::Handoff).await.reach, Reach::Tap);
        tap_task(dir.path(), "Next in the queue").await;
        let b = agent(dir.path(), "agent:next");
        let claim = b.claim_next(None, Duration::minutes(30)).await.unwrap();
        assert!(claim.is_some());
        assert!(b.read_outside());
        assert_eq!(wrote(&b, EntryType::Handoff).await.reach, Reach::Tap);
    }

    #[tokio::test]
    async fn a_write_after_a_task_list_gave_an_outside_task_has_tap_reach() {
        let dir = tempfile::tempdir().unwrap();
        drop(owner(dir.path()));
        tap_task(dir.path(), "From the tracker").await;
        let a = agent(dir.path(), "agent:lists");
        assert_eq!(a.tasks(None, Status::Open).await.unwrap().len(), 1);
        assert!(a.read_outside());
        assert_eq!(wrote(&a, EntryType::Fact).await.reach, Reach::Tap);
    }

    #[tokio::test]
    async fn a_task_that_a_session_files_after_it_read_outside_content_is_outside() {
        let dir = tempfile::tempdir().unwrap();
        let o = owner(dir.path());
        let parent = o.file_own(spec("Ship it")).await.unwrap().task;
        let plain = agent(dir.path(), "agent:plain");
        let filed = plain.create_task(spec("Before")).await.unwrap().task;
        assert!(!filed.spec.outside);
        let a = agent(dir.path(), "agent:marked");
        lock(&a.session).read_outside();
        // a caller cannot clear it
        let cleared = TaskSpec {
            outside: false,
            ..spec("After")
        };
        assert!(a.create_task(cleared).await.unwrap().task.spec.outside);
        let f = a
            .fan_out(&parent.id, &[("Child".into(), "Do the child.".into())])
            .await
            .unwrap();
        assert!(f.children[0].spec.outside, "a child of the fan-out");
        assert!(f.continuation.unwrap().spec.outside, "the continuation");
        assert!(a.retry_task(&parent.id).await.unwrap().spec.outside);
    }

    #[tokio::test]
    async fn what_is_filed_from_an_outside_task_is_outside() {
        let dir = tempfile::tempdir().unwrap();
        drop(owner(dir.path()));
        let parent = tap_task(dir.path(), "From the tracker").await;
        // each session files before it is handed the task, so only the parent says it
        let o = Memory::open(dir.path()).unwrap();
        let f = o
            .fan_out(&parent.id, &[("Child".into(), "Do the child.".into())])
            .await
            .unwrap();
        assert!(f.children[0].spec.outside, "a child of the fan-out");
        assert!(f.continuation.unwrap().spec.outside, "the continuation");
        let again = Memory::open(dir.path()).unwrap();
        assert!(again.retry_task(&parent.id).await.unwrap().spec.outside);
    }

    #[tokio::test]
    async fn the_classification_of_a_write_is_as_it_was_before_the_read() {
        let dir = tempfile::tempdir().unwrap();
        drop(owner(dir.path()));
        let a = agent(dir.path(), "agent:marked");
        let before = wrote(&a, EntryType::Fact).await;
        lock(&a.session).read_outside();
        let after = wrote(&a, EntryType::Fact).await;
        assert_eq!(after.reach, Reach::Tap);
        assert_eq!(after.classification, before.classification);
        let asked = a
            .write(WriteRequest {
                classification: Some(Label::Restricted),
                ..req(EntryType::Fact, "A restricted fact.\n")
            })
            .await
            .unwrap();
        assert_eq!(asked.classification, Label::Restricted);
    }

    const REQUEST: &[u8] = br#"{"messages":[{"role":"user","content":"Which port?"}]}"#;

    /// One session reads a plain entry, then a raw entry with tap reach, then writes
    /// and sends; another session reads the plain entry and writes. Returns the two
    /// sessions' ids, the entry the first wrote, and the root's owner session.
    async fn two_sessions(root: &Path) -> (String, String, String) {
        let o = owner(root);
        let plain = wrote(&o, EntryType::Fact).await.id;
        let planted = tap_entry(root).await;
        let a = agent(root, "agent:reader");
        a.get(&plain).await.unwrap();
        a.get(&planted).await.unwrap();
        let w = wrote(&a, EntryType::Fact).await;
        a.keep_context("anthropic", "/v1/messages", REQUEST, b"{}")
            .await
            .unwrap();
        let b = agent(root, "agent:other");
        b.get(&plain).await.unwrap();
        wrote(&b, EntryType::Fact).await;
        b.keep_context("anthropic", "/v1/messages", REQUEST, b"{}")
            .await
            .unwrap();
        (a.session_id().to_string(), b.session_id().to_string(), w.id)
    }

    #[tokio::test]
    async fn each_event_of_a_session_after_it_read_outside_content_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b, _) = two_sessions(dir.path()).await;
        let mark = |kind: &str, outside: bool| (kind.to_string(), outside);
        assert_eq!(
            marks(dir.path(), &a),
            [
                mark("read", false),
                mark("read", true),
                mark("write", true),
                mark("send", true)
            ],
            "the read that served the entry, and each event after it"
        );
        assert_eq!(
            marks(dir.path(), &b),
            [
                mark("read", false),
                mark("write", false),
                mark("send", false)
            ],
            "a session that read nothing from outside"
        );
        // a put names no session, and never has the key
        for (line, event) in lines(dir.path()) {
            if event.session.is_none() {
                assert!(line.get("outside").is_none(), "{line}");
                assert!(!event.outside);
            }
        }
    }

    #[tokio::test]
    async fn outside_readers_gives_each_such_session_with_its_writes_and_its_sends() {
        let dir = tempfile::tempdir().unwrap();
        let (a, _, written) = two_sessions(dir.path()).await;
        let o = Memory::open(dir.path()).unwrap();
        let readers = o.outside_readers(None).await.unwrap();
        let events = read(dir.path(), "dom-me", 0).unwrap();
        let of_a: Vec<&Event> = events
            .iter()
            .filter(|e| e.session.as_deref() == Some(a.as_str()) && e.outside)
            .collect();
        let send = of_a.last().unwrap();
        let Change::Send { to, digest } = &send.change else {
            panic!("{send:?}")
        };
        // the session that planted the entry wrote it after it was marked
        assert_eq!(readers.len(), 2, "{readers:?}");
        assert_eq!(readers[0].by, "agent:planted");
        assert!(
            readers[0].since < readers[1].since,
            "in the order of the log"
        );
        assert_eq!(
            readers[1],
            OutsideReader {
                session: a.clone(),
                by: "agent:reader".into(),
                since: of_a[0].seq,
                writes: vec![written],
                sends: vec![OutsideSend {
                    seq: send.seq,
                    at: send.at.clone(),
                    to: to.clone(),
                    digest: digest.clone(),
                }],
            }
        );
        assert_eq!(to, "anthropic");
        let refused = agent(dir.path(), "agent:curious")
            .outside_readers(None)
            .await
            .unwrap_err();
        assert!(matches!(refused, Error::Denied(_)), "{refused}");
    }

    #[tokio::test]
    async fn an_import_from_another_owner_lands_with_tap_reach_and_the_owner_s_own_keeps_it() {
        let a_dir = tempfile::tempdir().unwrap();
        let alice = Memory::init(a_dir.path(), "human:test:alice", "dom-a").unwrap();
        let held = alice.file_own(spec("Decide")).await.unwrap().task;
        alice.set_held_task(&held.id);
        let w = alice
            .write(WriteRequest {
                classification: Some(Label::Public),
                ..req(EntryType::Decision, "Alice decided.\n")
            })
            .await
            .unwrap();
        assert_eq!(
            (w.reach, w.task.as_deref()),
            (Reach::Chain, Some(held.id.as_str()))
        );
        let bundle = tempfile::tempdir().unwrap();
        alice
            .export(None, Label::Internal, bundle.path())
            .await
            .unwrap();

        let b_dir = tempfile::tempdir().unwrap();
        let bob = Memory::init(b_dir.path(), "human:test:bob", "dom-b").unwrap();
        assert_eq!(
            bob.import(bundle.path(), None).await.unwrap().written.len(),
            1
        );
        let landed = bob.get(&w.id).await.unwrap().front;
        assert_eq!((landed.reach, landed.task), (Reach::Tap, None));
        assert!(
            bob.read_outside(),
            "the import is raw content of another owner"
        );

        let own_dir = tempfile::tempdir().unwrap();
        let again = Memory::init(own_dir.path(), "human:test:alice", "dom-a").unwrap();
        assert_eq!(
            again
                .import(bundle.path(), None)
                .await
                .unwrap()
                .written
                .len(),
            1
        );
        let kept = again.get(&w.id).await.unwrap().front;
        assert_eq!((kept.reach, kept.task), (Reach::Chain, Some(held.id)));
        assert!(!again.read_outside());
    }
}
