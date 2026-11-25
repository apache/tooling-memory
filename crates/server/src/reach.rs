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
use crate::{lock, Error, Memory};
use dm_core::{Entry, RawReach, Reach, ReadFilter, TrustTier};
use dm_ledger::Task;
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

/// The task chain of a session: the tasks whose raw entries the session reads whatever
/// the reach of each entry is.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Chain {
    /// The held task, the task that it was filed from, the tasks filed from it and the
    /// tasks that it depends on.
    tasks: Vec<String>,
    /// Whether the held task is a task of a curator or of the auditor. Such a session
    /// reads all raw entries.
    reads_all: bool,
}

impl Chain {
    /// The chain of a held task before the ledger is asked: that task alone.
    pub(crate) fn of(held: &str) -> Chain {
        Chain {
            tasks: vec![held.to_string()],
            reads_all: false,
        }
    }

    /// Add a task, once.
    fn take(&mut self, id: &str) {
        if !self.tasks.iter().any(|t| t == id) {
            self.tasks.push(id.to_string());
        }
    }
}

/// The origins that name the task that another task was filed from: a subagent task, a
/// continuation and a retry. They are the origins that `Ledger::filed_from` looks for.
const FILED_FROM: [&str; 3] = ["task:", "continues:", "retries:"];

/// The task that `task` was filed from, when its origin names one.
fn filed_from(task: &Task) -> Option<&str> {
    let origin = task.spec.origin.as_deref()?;
    FILED_FROM.iter().find_map(|p| origin.strip_prefix(p))
}

/// Whether a task is a task of a curator or of the auditor, as its origin says.
pub(crate) fn curates(task: &Task) -> bool {
    task.spec
        .origin
        .as_deref()
        .is_some_and(|o| o.starts_with("curator:") || o.starts_with("auditor:"))
}

/// The word for a raw reach, as a root policy writes it.
pub(crate) fn raw_reach_word(reach: RawReach) -> &'static str {
    match reach {
        RawReach::Chain => "chain",
        RawReach::Fleet => "fleet",
        RawReach::All => "all",
    }
}

impl Memory {
    /// The read filter of the policy with the task chain of this moment laid over it. A
    /// session on a task of a curator or of the auditor reads all raw entries.
    pub(crate) fn over_chain(&self, mut f: ReadFilter) -> ReadFilter {
        let chain = lock(&self.chain);
        f.chain.clone_from(&chain.tasks);
        if chain.reads_all {
            f.raw_reach = RawReach::All;
        }
        f
    }

    /// Ask the ledger for the task chain of the held task: the task, the task that it
    /// was filed from, the tasks filed from it, and the tasks that it depends on. That is
    /// one step each way. A raw entry that names one of these tasks is served to this
    /// session whatever its reach is. A session whose held task is a task of a curator
    /// or of the auditor reads all raw entries.
    ///
    /// A held task that the ledger does not hold leaves the chain as that task alone. A
    /// held task that this session may not see gives an empty chain. This function takes
    /// the ledger: do not call it while the ledger is held.
    ///
    /// # Errors
    ///
    /// A ledger error, or a policy that does not give the read scope.
    pub async fn sync_chain(&self) -> Result<(), Error> {
        self.chain_of_held().await.map(|_| ())
    }

    /// [`Memory::sync_chain`], and the held task when the ledger holds it and this
    /// session may see it.
    async fn chain_of_held(&self) -> Result<Option<Task>, Error> {
        let Some(held) = self.held_task() else {
            *lock(&self.chain) = Chain::default();
            return Ok(None);
        };
        let handling = self.handling()?;
        let l = self.ledger.lock().await;
        let task = match l.get(&held).await {
            Ok(task) => task,
            Err(dm_ledger::Error::NotFound(_)) => {
                *lock(&self.chain) = Chain::of(&held);
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        };
        if !self.sees(&task, handling) {
            *lock(&self.chain) = Chain::default();
            return Ok(None);
        }
        let mut chain = Chain::of(&task.id);
        if let Some(parent) = filed_from(&task) {
            chain.take(parent);
        }
        for child in l.filed_from(&task.id).await? {
            chain.take(&child.id);
        }
        for needed in &task.spec.depends_on {
            chain.take(needed);
        }
        chain.reads_all = curates(&task);
        *lock(&self.chain) = chain;
        Ok(Some(task))
    }

    /// Hold a task, as a launcher does before the harness starts on it: the held task is
    /// set ([`Memory::set_held_task`]) and its task chain is read from the ledger
    /// ([`Memory::sync_chain`]). When the task is content from outside the fleet, this
    /// session has read outside content: the launcher puts the text of the task in the
    /// prompt of the harness.
    ///
    /// # Errors
    ///
    /// As [`Memory::sync_chain`].
    pub async fn hold_task(&self, id: &str) -> Result<(), Error> {
        self.set_held_task(id);
        if let Some(task) = self.chain_of_held().await? {
            if self.task_is_outside(&task) {
                lock(&self.session).read_outside();
            }
        }
        Ok(())
    }

    /// The tasks that this session filed from `parent` join the chain when `parent` is
    /// the held task. The ledger is not asked.
    pub(crate) fn chain_takes<'a>(&self, parent: &str, filed: impl Iterator<Item = &'a Task>) {
        if self.held_task().as_deref() != Some(parent) {
            return;
        }
        let mut chain = lock(&self.chain);
        filed.for_each(|t| chain.take(&t.id));
    }

    /// Whether this session may promote `entry` to curated. A raw entry with tap reach is
    /// content from authors outside the fleet, so only a session that reads all raw
    /// entries curates it: a person, or a session on a task of a curator or of the
    /// auditor. A member of the task chain reads the entry and does not curate it.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for such an entry and a session that does not read all raw
    /// entries, or a policy that does not give the read scope.
    pub(crate) fn may_curate(&self, entry: &Entry) -> Result<(), Error> {
        if entry.front.reach != Reach::Tap || self.session_filter()?.raw_reach == RawReach::All {
            return Ok(());
        }
        Err(Error::Denied(format!(
            "{} has tap reach: it came from outside the fleet, or its author had read content from outside the fleet. A curator or a person curates an entry with tap reach",
            entry.front.id
        )))
    }

    /// Whether this session may write an entry that replaces `old`. The entry is not
    /// written yet: `tier`, `reach` and `task` are what the session gives it.
    ///
    /// The read filter of the session, before its settings, must admit `old`, as an
    /// active entry or as one that the archive keeps. A write that replaces an entry takes that entry out of the
    /// active set, so a session that cannot read an entry cannot replace it.
    ///
    /// A raw entry with tap reach is served only to the chain of its task and to the
    /// readers of all raw entries. Such a write replaces only a raw entry with tap reach
    /// that names the same task: each session that reads `old` then reads the entry that
    /// replaces it. It replaces no other entry, or it would take that entry from its
    /// readers before a curator saw the write.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for an entry that the read filter does not admit, which is
    /// the error for an entry that does not exist. [`Error::Denied`] for a raw write
    /// with tap reach and an entry with other readers. An index or store error.
    pub(crate) async fn may_replace(
        &self,
        old: &str,
        tier: TrustTier,
        reach: Reach,
        task: Option<&str>,
    ) -> Result<(), Error> {
        // a read on behalf of a write: under the grants, the policy and the task chain.
        // The settings of the session do not narrow it
        let mut f = self.chained_filter()?;
        f.include_inactive = true;
        let meta = self.index.lock().await.meta(old, &f)?;
        let Some(meta) = meta else {
            return Err(Error::NotFound(old.to_string()));
        };
        if !raw_tap(tier, reach) {
            return Ok(());
        }
        let same_readers = self
            .store
            .get(&meta.hit.domain, old)
            .await?
            .is_some_and(|e| outside_content(&e) && e.front.task.as_deref() == task);
        if same_readers {
            return Ok(());
        }
        Err(Error::Denied(format!(
            "this write has tap reach: its session read content from outside the fleet. It does not replace {old}, which has other readers. Write the entry without `supersedes` and cite {old}: a curator or a person replaces {old}"
        )))
    }
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

    /// The task that the writer of [`tap_entry`] holds. The ledger does not hold it.
    const PLANTED: &str = "task-planted-1";

    /// A raw entry with tap reach in `dom-me` that names [`PLANTED`]. The session that
    /// writes it is given the record directly, so that a test of one way to read outside
    /// content does not rest on another way.
    async fn tap_entry(root: &Path) -> String {
        let planted = agent(root, "agent:planted");
        planted.set_held_task(PLANTED);
        lock(&planted.session).read_outside();
        let w = wrote(&planted, EntryType::Fact).await;
        assert_eq!(w.reach, Reach::Tap);
        w.id
    }

    /// An agent session in the task chain of [`tap_entry`].
    fn in_chain(root: &Path, name: &str) -> Memory {
        let m = agent(root, name);
        m.set_held_task(PLANTED);
        m
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
        let a = in_chain(dir.path(), "agent:searches");
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
        let a = in_chain(dir.path(), "agent:lists");
        assert_eq!(a.list(None, None, 10).await.unwrap().len(), 1);
        assert!(a.read_outside());
        assert_eq!(wrote(&a, EntryType::Fact).await.reach, Reach::Tap);
    }

    #[tokio::test]
    async fn a_write_after_a_get_served_a_raw_entry_with_tap_reach_has_tap_reach() {
        let dir = tempfile::tempdir().unwrap();
        drop(owner(dir.path()));
        let id = tap_entry(dir.path()).await;
        let a = in_chain(dir.path(), "agent:gets");
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
        let a = in_chain(root, "agent:reader");
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

    // ---- the reader's side: the task chain ----

    /// What a session is served of one entry by `search`, `list` and `get`. The three
    /// agree.
    async fn served(m: &Memory, query: &str, id: &str) -> bool {
        let found = |hits: Vec<dm_index::Hit>| hits.iter().any(|h| h.id == id);
        let search = found(m.search(query, None, 50).await.unwrap());
        let list = found(m.list(None, None, 50).await.unwrap());
        let get = match m.get(id).await {
            Ok(_) => true,
            Err(Error::NotFound(missing)) => {
                assert_eq!(missing, id);
                false
            }
            Err(e) => panic!("{e}"),
        };
        assert_eq!((search, list), (get, get), "search, list and get agree");
        get
    }

    /// [`served`] for an entry that [`wrote`] wrote.
    async fn serves(m: &Memory, id: &str) -> bool {
        served(m, "planted", id).await
    }

    /// An agent session that holds `task`, as a launcher starts it.
    async fn holder(root: &Path, name: &str, task: &str) -> Memory {
        let m = agent(root, name);
        m.hold_task(task).await.unwrap();
        m
    }

    /// Task T, which a tap filed and the owner admitted, and note N: a handoff that
    /// `agent:a` wrote while it held T. N is raw, has tap reach and names T.
    async fn noted(root: &Path) -> (Task, String) {
        let t = tap_task(root, "From the tracker").await;
        let a = holder(root, "agent:a", &t.id).await;
        let n = wrote(&a, EntryType::Handoff).await;
        assert_eq!(
            (n.reach, n.task.as_deref()),
            (Reach::Tap, Some(t.id.as_str()))
        );
        (t, n.id)
    }

    /// A task that the ledger of `o` takes as it is, in `dom-me`.
    async fn ledger_task(o: &Memory, title: &str, origin: Option<String>, needs: &[&str]) -> Task {
        let spec = TaskSpec {
            domain: "dom-me".into(),
            created_by: OWNER.into(),
            origin,
            depends_on: needs.iter().map(|id| (*id).to_string()).collect(),
            ..spec(title)
        };
        o.ledger.lock().await.create(spec).await.unwrap().task
    }

    /// Give the agents of a root one more key in their read filter.
    fn agents_read(root: &Path, pair: &str) {
        let text = dm_policy::decide::BUILTIN.replace(
            "read_filter := base_filter\n",
            &format!(
                "read_filter := object.union(base_filter, {{{pair}}}) if class == \"agent\"\n\nread_filter := base_filter if class != \"agent\"\n"
            ),
        );
        assert_ne!(text, dm_policy::decide::BUILTIN);
        std::fs::create_dir_all(root.join("policy")).unwrap();
        std::fs::write(root.join("policy/local.rego"), text).unwrap();
    }

    #[tokio::test]
    async fn a_session_that_holds_an_outside_task_writes_with_tap_reach() {
        let dir = tempfile::tempdir().unwrap();
        let o = owner(dir.path());
        let own = o.file_own(spec("Own work")).await.unwrap().task;
        let plain = holder(dir.path(), "agent:plain", &own.id).await;
        assert!(!plain.read_outside(), "a task of the fleet");
        let t = tap_task(dir.path(), "From the tracker").await;
        let a = agent(dir.path(), "agent:a");
        let before = a.label();
        a.hold_task(&t.id).await.unwrap();
        assert!(a.read_outside(), "the launcher puts the task in the prompt");
        assert_eq!(a.label(), before, "the label is as it was");
        let w = wrote(&a, EntryType::Fact).await;
        assert_eq!(
            (w.reach, w.task.as_deref()),
            (Reach::Tap, Some(t.id.as_str()))
        );
    }

    #[tokio::test]
    async fn a_raw_entry_with_tap_reach_is_served_to_a_second_session_that_holds_its_task() {
        let dir = tempfile::tempdir().unwrap();
        drop(owner(dir.path()));
        let (t, n) = noted(dir.path()).await;
        let b = holder(dir.path(), "agent:b", &t.id).await;
        assert!(serves(&b, &n).await);
        assert!(b.t0(None).await.unwrap().contains(&n), "in its T0");
        // a view of the session reads with the chain of the session
        assert!(serves(&b.for_request().unwrap(), &n).await);
    }

    #[tokio::test]
    async fn a_raw_entry_with_tap_reach_is_not_served_outside_its_task_chain() {
        let dir = tempfile::tempdir().unwrap();
        let o = owner(dir.path());
        let (_, n) = noted(dir.path()).await;
        let none = agent(dir.path(), "agent:none");
        assert!(!serves(&none, &n).await, "an agent with no task");
        assert!(!none.t0(None).await.unwrap().contains(&n));
        let other = o.file_own(spec("Other work")).await.unwrap().task;
        let b = holder(dir.path(), "agent:b", &other.id).await;
        assert!(!serves(&b, &n).await, "an agent on an unrelated task");
        assert!(!b.t0(None).await.unwrap().contains(&n));
        // the answer for it is the answer for an entry that does not exist
        let missing = n.replace("mem-", "mem-no-");
        for id in [&n, &missing] {
            let refused = b.get(id).await.unwrap_err();
            assert!(
                matches!(&refused, Error::NotFound(named) if named == id),
                "{refused}"
            );
        }
        assert!(!b.read_outside(), "nothing was served");
    }

    #[tokio::test]
    async fn the_chain_is_one_step_around_the_held_task() {
        let dir = tempfile::tempdir().unwrap();
        let o = owner(dir.path());
        let (t, n) = noted(dir.path()).await;
        let child = ledger_task(&o, "Child", Some(format!("task:{}", t.id)), &[]).await;
        let around = [
            child.clone(),
            ledger_task(&o, "Resume", Some(dm_ledger::continues(&t.id)), &[]).await,
            ledger_task(&o, "Retry", Some(format!("retries:{}", t.id)), &[]).await,
            ledger_task(&o, "Needs it", None, &[&t.id]).await,
        ];
        for task in &around {
            let reader = holder(dir.path(), "agent:near", &task.id).await;
            assert!(serves(&reader, &n).await, "{}", task.spec.title);
        }
        let grandchild =
            ledger_task(&o, "Grandchild", Some(format!("task:{}", child.id)), &[]).await;
        let far = holder(dir.path(), "agent:far", &grandchild.id).await;
        assert!(!serves(&far, &n).await, "two steps from the task");
        // the other way: the session on the task reads what the child's session wrote
        let c = holder(dir.path(), "agent:child", &child.id).await;
        assert!(serves(&c, &n).await);
        let m = wrote(&c, EntryType::Handoff).await;
        assert_eq!(
            (m.reach, m.task.as_deref()),
            (Reach::Tap, Some(child.id.as_str()))
        );
        let parent = holder(dir.path(), "agent:parent", &t.id).await;
        assert!(serves(&parent, &m.id).await);
        assert!(serves(&far, &m.id).await, "a child of that task reads it");
    }

    #[tokio::test]
    async fn a_task_that_needs_the_held_task_is_not_in_the_chain() {
        let dir = tempfile::tempdir().unwrap();
        let o = owner(dir.path());
        let (t, _) = noted(dir.path()).await;
        let needs = ledger_task(&o, "Needs it", None, &[&t.id]).await;
        let later = holder(dir.path(), "agent:later", &needs.id).await;
        lock(&later.session).read_outside();
        let m = wrote(&later, EntryType::Handoff).await;
        assert_eq!(m.reach, Reach::Tap);
        let on_t = holder(dir.path(), "agent:b", &t.id).await;
        assert!(!serves(&on_t, &m.id).await);
    }

    #[tokio::test]
    async fn a_held_task_that_the_ledger_does_not_hold_is_a_chain_of_itself() {
        let dir = tempfile::tempdir().unwrap();
        drop(owner(dir.path()));
        let n = tap_entry(dir.path()).await;
        let a = holder(dir.path(), "agent:a", PLANTED).await;
        assert!(serves(&a, &n).await);
        a.sync_chain().await.unwrap();
        assert!(serves(&a, &n).await, "asked again");
    }

    #[tokio::test]
    async fn after_a_fan_out_the_session_reads_what_a_child_wrote() {
        let dir = tempfile::tempdir().unwrap();
        drop(owner(dir.path()));
        let t = tap_task(dir.path(), "From the tracker").await;
        let p = holder(dir.path(), "agent:p", &t.id).await;
        let view = p.for_request().unwrap();
        let f = p
            .fan_out(&t.id, &[("Child".into(), "Do the child.".into())])
            .await
            .unwrap();
        let (child, resume) = (&f.children[0], f.continuation.as_ref().unwrap());
        let mut notes = Vec::new();
        for (name, task) in [("agent:c", child), ("agent:r", resume)] {
            let m = wrote(
                &holder(dir.path(), name, &task.id).await,
                EntryType::Handoff,
            )
            .await;
            assert_eq!(m.reach, Reach::Tap);
            notes.push(m.id);
        }
        for n in &notes {
            assert!(serves(&p, n).await, "without holding the task again");
            assert!(serves(&view, n).await, "a view made before the fan-out");
        }
        // a fan-out from a task that the session does not hold adds nothing
        let q = agent(dir.path(), "agent:q");
        let g = q
            .fan_out(&child.id, &[("Deeper".into(), "Go on.".into())])
            .await
            .unwrap();
        let deep = holder(dir.path(), "agent:d", &g.children[0].id).await;
        let m = wrote(&deep, EntryType::Handoff).await;
        assert_eq!(m.reach, Reach::Tap);
        assert!(!serves(&q, &m.id).await);
    }

    #[tokio::test]
    async fn the_owner_and_a_session_on_a_curator_s_task_read_every_raw_entry() {
        let dir = tempfile::tempdir().unwrap();
        let o = owner(dir.path());
        let (_, n) = noted(dir.path()).await;
        assert!(serves(&o, &n).await, "the owner");
        assert!(o.t0(None).await.unwrap().contains(&n));
        for origin in ["curator:survey", "auditor:sample"] {
            let duty = ledger_task(&o, "A duty", Some(origin.into()), &[]).await;
            let s = holder(dir.path(), "agent:duty", &duty.id).await;
            assert!(serves(&s, &n).await, "{origin}");
            // a view keeps the filter of the policy, and not the reach of the task that
            // the session held when the view was made
            let view = s.for_request().unwrap();
            assert!(serves(&view, &n).await, "{origin}");
            let plain = o.file_own(spec("Plain work")).await.unwrap().task;
            s.hold_task(&plain.id).await.unwrap();
            assert!(!serves(&s, &n).await, "on another task after {origin}");
            assert!(!serves(&view, &n).await, "the view, after {origin}");
        }
    }

    #[tokio::test]
    async fn a_tap_is_handed_a_handoff_with_tap_reach_to_post_back() {
        let dir = tempfile::tempdir().unwrap();
        let o = owner(dir.path());
        let filed = TaskSpec {
            origin: Some("github:acme/x#1".into()),
            upstream_event: Some("open:2026-10-01T09:00:00Z".into()),
            ..spec("From upstream")
        };
        let task = o.create_task(filed).await.unwrap().task;
        o.admit(&task.id).await.unwrap();
        let w = holder(dir.path(), "agent:w", &task.id).await;
        lock(&w.session).read_outside();
        w.claim(&task.id, Duration::minutes(5)).await.unwrap();
        let h = wrote(&w, EntryType::Handoff).await;
        assert_eq!(h.reach, Reach::Tap);
        w.complete(&task.id, None, &h.id).await.unwrap();
        let tap = agent(dir.path(), "tap:comments");
        let due = tap.unposted("dom-me", Label::Restricted).await.unwrap();
        assert_eq!(due.len(), 1, "the tap reads what it posts back");
        assert_eq!(
            (due[0].0.id.as_str(), &due[0].1.front.id),
            (task.id.as_str(), &h.id)
        );
        assert_eq!(due[0].1.front.reach, Reach::Tap);
        // the same question from an agent outside the chain hands out nothing
        let outside = agent(dir.path(), "agent:other");
        let none = outside.unposted("dom-me", Label::Restricted).await.unwrap();
        assert!(none.is_empty(), "{none:?}");
    }

    /// The handoff that a release names is read on behalf of the release, with the
    /// task chain: the session that wrote a handoff with tap reach releases its task
    /// with it, and a session outside the chain is told that there is no such entry.
    #[tokio::test]
    async fn a_session_releases_its_task_with_its_handoff_that_has_tap_reach() {
        let dir = tempfile::tempdir().unwrap();
        let o = owner(dir.path());
        let t = tap_task(dir.path(), "From the tracker").await;
        let a = holder(dir.path(), "agent:a", &t.id).await;
        a.claim(&t.id, Duration::minutes(5)).await.unwrap();
        let h = wrote(&a, EntryType::Handoff).await;
        assert_eq!((h.reach, h.task.as_deref()), (Reach::Tap, Some(&*t.id)));
        // a session on an unrelated task does not release its task with that handoff
        let other = o.file_own(spec("Other work")).await.unwrap().task;
        let b = holder(dir.path(), "agent:b", &other.id).await;
        b.claim(&other.id, Duration::minutes(5)).await.unwrap();
        let refused = b.release(&other.id, None, Some(&h.id)).await;
        assert!(matches!(refused, Err(Error::NotFound(_))), "{refused:?}");
        let released = a.release(&t.id, None, Some(&h.id)).await.unwrap();
        assert_eq!(released.handoff.as_deref(), Some(h.id.as_str()));
    }

    #[tokio::test]
    async fn once_curated_the_entry_is_served_to_an_agent_with_no_task() {
        let dir = tempfile::tempdir().unwrap();
        let o = owner(dir.path());
        let (_, n) = noted(dir.path()).await;
        assert!(!serves(&agent(dir.path(), "agent:before"), &n).await);
        o.promote(&n, TrustTier::Curated, "Checked against the runbook.")
            .await
            .unwrap();
        let after = agent(dir.path(), "agent:after");
        assert!(serves(&after, &n).await);
        assert!(!after.read_outside(), "a curated entry");
    }

    #[tokio::test]
    async fn a_member_of_the_chain_does_not_curate_an_entry_with_tap_reach() {
        let dir = tempfile::tempdir().unwrap();
        let o = owner(dir.path());
        let (t, n) = noted(dir.path()).await;
        let b = holder(dir.path(), "agent:b", &t.id).await;
        let refused = b
            .promote(&n, TrustTier::Curated, "It reads well.")
            .await
            .unwrap_err();
        assert!(
            matches!(&refused, Error::Denied(why) if why.contains("A curator or a person curates")),
            "{refused}"
        );
        assert_eq!(o.get(&n).await.unwrap().front.trust_tier, TrustTier::Raw);
        // a proposal for verified is a task for a person, and it is filed
        let proposed = b
            .promote(&n, TrustTier::Verified, "It reads well.")
            .await
            .unwrap();
        assert!(matches!(proposed, crate::Promotion::Proposed { .. }));
        // a session on a curator's task curates it, and so does the owner
        let other = wrote(
            &holder(dir.path(), "agent:a2", &t.id).await,
            EntryType::Handoff,
        )
        .await;
        let duty = ledger_task(&o, "Survey", Some("curator:survey".into()), &[]).await;
        let curator = holder(dir.path(), "agent:curator", &duty.id).await;
        for (m, id) in [(&curator, &other.id), (&o, &n)] {
            let done = m.promote(id, TrustTier::Curated, "Checked.").await.unwrap();
            assert!(matches!(done, crate::Promotion::Promoted { .. }));
        }
    }

    #[tokio::test]
    async fn an_agent_curates_an_entry_of_the_fleet_as_before() {
        let dir = tempfile::tempdir().unwrap();
        let o = owner(dir.path());
        let fleet = wrote(&o, EntryType::Fact).await;
        let b = agent(dir.path(), "agent:b");
        let done = b
            .promote(&fleet.id, TrustTier::Curated, "Checked.")
            .await
            .unwrap();
        assert!(matches!(done, crate::Promotion::Promoted { .. }));
    }

    #[tokio::test]
    async fn a_root_policy_narrows_an_agent_to_the_raw_entries_of_its_chain() {
        let dir = tempfile::tempdir().unwrap();
        let o = owner(dir.path());
        let (t, n) = noted(dir.path()).await;
        let fleet = wrote(&o, EntryType::Fact).await;
        assert_eq!(fleet.reach, Reach::Fleet);
        let before = holder(dir.path(), "agent:b", &t.id).await;
        assert!(serves(&before, &fleet.id).await);
        agents_read(dir.path(), "\"raw_reach\": \"chain\"");
        let b = holder(dir.path(), "agent:b", &t.id).await;
        assert!(!serves(&b, &fleet.id).await, "a raw entry of the fleet");
        assert!(serves(&b, &n).await, "a raw entry of its chain");
        assert!(serves(&o, &fleet.id).await, "the owner reads as before");
        let log = std::fs::read_to_string(dir.path().join("state/decisions.jsonl")).unwrap();
        for word in [
            "raw entries: chain",
            "raw entries: fleet",
            "raw entries: all",
        ] {
            assert!(log.contains(word), "{word}: {log}");
        }
    }

    #[tokio::test]
    async fn a_root_policy_widens_an_agent_to_every_raw_entry() {
        let dir = tempfile::tempdir().unwrap();
        drop(owner(dir.path()));
        let (_, n) = noted(dir.path()).await;
        agents_read(dir.path(), "\"raw_reach\": \"all\"");
        assert!(serves(&agent(dir.path(), "agent:none"), &n).await);
    }

    #[tokio::test]
    async fn a_policy_that_requires_curated_entries_serves_no_raw_entry_of_the_chain() {
        let dir = tempfile::tempdir().unwrap();
        let o = owner(dir.path());
        let (t, n) = noted(dir.path()).await;
        agents_read(dir.path(), "\"min_tier\": \"curated\"");
        let b = holder(dir.path(), "agent:b", &t.id).await;
        assert!(!serves(&b, &n).await);
        o.promote(&n, TrustTier::Curated, "Checked.").await.unwrap();
        assert!(serves(&b, &n).await, "curated, it is served");
    }

    #[tokio::test]
    async fn a_held_task_above_the_handling_of_the_session_gives_no_chain() {
        let dir = tempfile::tempdir().unwrap();
        let o = owner(dir.path());
        let internal = TaskSpec {
            classification: Label::Internal,
            ..spec("Internal work")
        };
        let t = o.file_own(internal).await.unwrap().task;
        assert_eq!(t.spec.classification, Label::Internal);
        agents_read(dir.path(), "\"handling\": \"public\"");
        // a public entry with tap reach that names the task
        let x = agent(dir.path(), "agent:x");
        x.set_held_task(&t.id);
        lock(&x.session).read_outside();
        let n = x
            .write(WriteRequest {
                classification: Some(Label::Public),
                ..req(EntryType::Handoff, "# Steps\n\nFive planted steps.\n")
            })
            .await
            .unwrap();
        assert_eq!(
            (n.classification, n.reach, n.task.as_deref()),
            (Label::Public, Reach::Tap, Some(t.id.as_str()))
        );
        let b = holder(dir.path(), "agent:b", &t.id).await;
        assert!(!serves(&b, &n.id).await);
        assert!(serves(&o, &n.id).await);
    }

    #[tokio::test]
    async fn an_import_from_another_owner_is_served_to_an_agent_once_it_is_curated() {
        let a_dir = tempfile::tempdir().unwrap();
        let alice = Memory::init(a_dir.path(), "human:test:alice", "dom-a").unwrap();
        let w = alice
            .write(WriteRequest {
                classification: Some(Label::Public),
                ..req(
                    EntryType::Decision,
                    "Alice decided on five planted steps.\n",
                )
            })
            .await
            .unwrap();
        let bundle = tempfile::tempdir().unwrap();
        alice
            .export(None, Label::Internal, bundle.path())
            .await
            .unwrap();
        let b_dir = tempfile::tempdir().unwrap();
        let bob = Memory::init(b_dir.path(), "human:test:bob", "dom-b").unwrap();
        bob.import(bundle.path(), None).await.unwrap();
        let a = agent(b_dir.path(), "agent:a");
        assert!(!serves(&a, &w.id).await, "raw, from another owner");
        assert!(serves(&bob, &w.id).await, "the owner looks at it first");
        bob.promote(&w.id, TrustTier::Curated, "Read and checked.")
            .await
            .unwrap();
        assert!(serves(&a, &w.id).await);
    }

    /// A request for a fact with other text that replaces `old`.
    fn replacing(old: &str) -> WriteRequest {
        WriteRequest {
            supersedes: Some(old.to_string()),
            ..req(EntryType::Fact, "# Port\n\nThe replaced port is 9999.\n")
        }
    }

    /// An agent session on a task that the owner filed from `parent`. The task is not
    /// outside content, and `parent` is in its chain.
    async fn child_of(o: &Memory, root: &Path, name: &str, parent: &Task) -> Memory {
        let origin = Some(format!("task:{}", parent.id));
        let child = ledger_task(o, "Child", origin, &[]).await;
        let m = holder(root, name, &child.id).await;
        assert!(!m.read_outside(), "a task of the owner");
        m
    }

    #[tokio::test]
    async fn a_write_does_not_replace_an_entry_that_the_read_filter_withholds() {
        let dir = tempfile::tempdir().unwrap();
        let o = owner(dir.path());
        let (t, n) = noted(dir.path()).await;
        let none = agent(dir.path(), "agent:none");
        // the answer for it is the answer for an entry that does not exist
        let missing = n.replace("mem-", "mem-no-");
        for id in [&n, &missing] {
            let refused = none.write(replacing(id)).await.unwrap_err();
            assert!(
                matches!(&refused, Error::NotFound(named) if named == id),
                "{refused}"
            );
        }
        assert!(o.get(&n).await.unwrap().is_active(), "it is as it was");
        let b = holder(dir.path(), "agent:b", &t.id).await;
        assert!(serves(&b, &n).await, "its chain reads it as before");
        assert!(
            none.list(None, None, 50).await.unwrap().is_empty(),
            "the record took no entry of the refused writes"
        );
        // a session that reads the entry replaces it
        let w = b.write(replacing(&n)).await.unwrap();
        assert_eq!(
            (w.reach, w.task.as_deref()),
            (Reach::Tap, Some(t.id.as_str()))
        );
        let moved = holder(dir.path(), "agent:b2", &t.id)
            .await
            .get(&n)
            .await
            .unwrap_err();
        assert!(
            matches!(&moved, Error::Moved { to, .. } if *to == w.id),
            "{moved}"
        );
    }

    #[tokio::test]
    async fn a_raw_write_with_tap_reach_replaces_only_an_entry_with_the_same_readers() {
        let dir = tempfile::tempdir().unwrap();
        let o = owner(dir.path());
        let fleet = wrote(&o, EntryType::Fact).await;
        let (t, n) = noted(dir.path()).await;
        let marked = holder(dir.path(), "agent:marked", &t.id).await;
        assert!(marked.read_outside());
        // an entry that the fleet reads
        let refused = marked.write(replacing(&fleet.id)).await.unwrap_err();
        assert!(
            matches!(&refused, Error::Denied(why) if why.contains("has tap reach") && why.contains(&fleet.id)),
            "{refused}"
        );
        let plain = agent(dir.path(), "agent:plain");
        assert!(
            serves(&plain, &fleet.id).await,
            "the fleet reads it as before"
        );
        // a raw entry with tap reach that names another task of the chain
        let c = child_of(&o, dir.path(), "agent:c", &t).await;
        c.get(&n).await.unwrap();
        assert!(c.read_outside());
        let refused = c.write(replacing(&n)).await.unwrap_err();
        assert!(
            matches!(&refused, Error::Denied(why) if why.contains("has tap reach")),
            "{refused}"
        );
        assert!(serves(&marked, &n).await, "its chain reads it as before");
        // the same text without `supersedes` is taken
        let mut alone = replacing(&n);
        alone.supersedes = None;
        assert_eq!(c.write(alone).await.unwrap().reach, Reach::Tap);
        // a session on a task of a curator writes at curated, and the fleet reads that
        let duty = ledger_task(&o, "Survey", Some("curator:survey".into()), &[]).await;
        let curator = holder(dir.path(), "agent:curator", &duty.id).await;
        curator.get(&n).await.unwrap();
        assert!(curator.read_outside());
        let w = curator.write(replacing(&fleet.id)).await.unwrap();
        assert_eq!(w.reach, Reach::Tap);
        let read = agent(dir.path(), "agent:after").get(&w.id).await.unwrap();
        assert_eq!(read.front.trust_tier, TrustTier::Curated);
    }

    #[tokio::test]
    async fn the_summaries_page_does_not_list_a_raw_entry_with_tap_reach() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let view = dir.path().join("view");
        let o = owner(&root);
        let own = o
            .write(req(
                EntryType::Fact,
                "# Port\n\nThe gateway port is 8443.\n",
            ))
            .await
            .unwrap();
        let (t, n) = noted(&root).await;
        let c = child_of(&o, &root, "agent:c", &t).await;
        let done = c.project_into(&view).await.unwrap();
        assert_eq!(done.listed, 2, "each entry is a file");
        let dom = view.join("dom-me");
        let page = || std::fs::read_to_string(dom.join(crate::projection::SUMMARIES)).unwrap();
        let first = page();
        assert!(first.starts_with("# dom-me: 1 entries\n"), "{first}");
        assert!(first.contains(&own.id), "{first}");
        assert!(
            !first.contains(&n) && !first.contains("Steps") && !first.contains("planted"),
            "{first}"
        );
        assert!(first.contains("does not list the raw entries"), "{first}");
        assert!(!c.read_outside(), "the render is not a read");
        assert_eq!(wrote(&c, EntryType::Fact).await.reach, Reach::Fleet);
        // the file is there, and the hook turns the open of it into a get
        assert!(dom.join(crate::projection::entry_file(&n)).is_file());
        c.get(&n).await.unwrap();
        assert!(c.read_outside());
        // once curated, the entry has a line
        o.promote(&n, TrustTier::Curated, "Checked against the runbook.")
            .await
            .unwrap();
        c.project_into(&view).await.unwrap();
        let second = page();
        assert!(second.contains(&format!("[Steps]({n}.md)")), "{second}");
    }

    #[tokio::test]
    async fn a_write_after_a_recall_linked_a_raw_entry_with_tap_reach_has_tap_reach() {
        use crate::recall::{Budget, Depth, Via};
        let dir = tempfile::tempdir().unwrap();
        let o = owner(dir.path());
        let (t, n) = noted(dir.path()).await;
        let citing = o
            .write(WriteRequest {
                cites: vec![n.clone()],
                ..req(EntryType::Fact, "# Port\n\nThe gateway port is 8443.\n")
            })
            .await
            .unwrap();
        assert_eq!(citing.reach, Reach::Fleet);
        let c = child_of(&o, dir.path(), "agent:c", &t).await;
        let pack = c
            .pack(
                &["gateway port".to_string()],
                None,
                Depth::Sections,
                Budget::default(),
            )
            .await
            .unwrap();
        let via: Vec<(&str, Via)> = pack.pieces.iter().map(|p| (p.id.as_str(), p.via)).collect();
        assert_eq!(
            via,
            [(citing.id.as_str(), Via::Query), (n.as_str(), Via::Linked)],
            "the note is served only as a linked entry"
        );
        assert!(c.read_outside(), "its summary is outside content");
        assert_eq!(wrote(&c, EntryType::Fact).await.reach, Reach::Tap);
    }

    #[tokio::test]
    async fn a_write_after_a_recall_served_the_body_of_a_raw_entry_with_tap_reach_has_tap_reach() {
        use crate::recall::{Budget, Depth};
        let dir = tempfile::tempdir().unwrap();
        let o = owner(dir.path());
        let w = o
            .write(req(
                EntryType::Fact,
                "# Port\n\nThe gateway port is 8443.\n",
            ))
            .await
            .unwrap();
        // the record is ahead of the index: its version has tap reach, and the row of
        // the index says that the entry has the reach of the fleet
        let mut ahead = o.store.get("dom-me", &w.id).await.unwrap().unwrap();
        ahead.front.reach = Reach::Tap;
        o.store.put(&ahead).await.unwrap();
        let plain = agent(dir.path(), "agent:plain");
        let queries = ["gateway port".to_string()];
        let titles = plain
            .pack(&queries, None, Depth::Summaries, Budget::default())
            .await
            .unwrap();
        assert_eq!(titles.pieces.len(), 1);
        assert!(!plain.read_outside(), "the row has the reach of the fleet");
        let pack = plain
            .pack(&queries, None, Depth::Sections, Budget::default())
            .await
            .unwrap();
        assert!(pack.pieces[0].body.is_some(), "the body was served");
        assert!(plain.read_outside(), "the body is outside content");
        assert_eq!(wrote(&plain, EntryType::Fact).await.reach, Reach::Tap);
    }

    #[tokio::test]
    async fn an_agent_with_no_task_reads_the_raw_fleet_and_chain_entries_of_others() {
        let dir = tempfile::tempdir().unwrap();
        let o = owner(dir.path());
        let fleet = wrote(&agent(dir.path(), "agent:w1"), EntryType::Fact).await;
        let other = o.file_own(spec("Other work")).await.unwrap().task;
        let w2 = holder(dir.path(), "agent:w2", &other.id).await;
        let chain = wrote(&w2, EntryType::Handoff).await;
        assert_eq!(
            (fleet.reach, chain.reach, chain.task.as_deref()),
            (Reach::Fleet, Reach::Chain, Some(other.id.as_str()))
        );
        let none = agent(dir.path(), "agent:none");
        assert!(serves(&none, &fleet.id).await);
        assert!(serves(&none, &chain.id).await);
        assert!(!none.read_outside());
    }
}
