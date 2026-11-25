//! An outage: what a session does while reads and changes cannot be checked.
//!
//! The owner declares an outage ([`Memory::declare_outage`]) and ends it
//! ([`Memory::end_outage`]). The state is one file, [`OUTAGE_FILE`], under the root. While
//! the file is there, each read and each change waits: it returns [`Error::Outage`], and
//! nothing reaches the record, the log or the ledger. A session is not stopped, and
//! [`Memory::fresh`] does not look at the file, so a session keeps its model for local
//! analysis.
//!
//! Three things do not wait:
//!
//! - [`Memory::t0`] returns the T0 that the session loaded before the outage, with one
//!   line that says so. A session that holds a task gets it only until the last lease
//!   that the ledger confirmed.
//! - A write of a handoff is put on a queue, [`QUEUE_FILE`].
//! - A heartbeat is put on the same queue. It is not confirmed.
//!
//! At the end of the outage the queue is replayed in order. A heartbeat goes to the
//! ordinary heartbeat of the ledger, so the clock of the ledger decides: a heartbeat for
//! a lease that ran out in the outage is refused and does not give the claim back. A
//! handoff goes through the ordinary write as its author, so the gates screen it. When
//! the claim of its task is over, the handoff is left on the task as a late handoff.

use crate::{io, lock, reads, Error, Memory, WriteRequest};
use dm_core::{EntryType, Label};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};

/// The file that declares an outage, under the root. While it is there, reads and
/// changes wait.
pub const OUTAGE_FILE: &str = "state/outage.json";
/// The handoffs and the heartbeats that wait for the end of the outage, under the root:
/// one JSON object on each line, in the order they came.
pub const QUEUE_FILE: &str = "state/outage/queue.jsonl";
/// The queue while [`Memory::end_outage`] replays it.
const REPLAY_FILE: &str = "state/outage/queue.replaying.jsonl";

/// A declared outage.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outage {
    /// When it was declared.
    #[serde(with = "time::serde::rfc3339")]
    pub since: OffsetDateTime,
    /// Why, in the words of the person who declared it.
    pub reason: String,
    /// Who declared it.
    pub declared_by: String,
}

/// What the end of an outage did with the queue.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Replayed {
    /// The ids of the handoffs that the record took, in the order of the queue.
    pub handoffs: Vec<String>,
    /// How many queued heartbeats the ledger applied.
    pub heartbeats_applied: usize,
    /// How many queued heartbeats the ledger refused: the lease ran out in the outage,
    /// or the claim is over.
    pub heartbeats_refused: usize,
    /// One line for each queued line that failed, with the reason. A line that failed
    /// does not stop the lines after it.
    pub failed: Vec<String>,
}

/// The outage as one session sees it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct OutageStatus {
    /// The outage that is declared now. None when there is no outage.
    pub outage: Option<Outage>,
    /// The domains whose T0 this session loaded: what it works from in an outage.
    pub t0_loaded: Vec<String>,
    /// The end of the last lease that the ledger confirmed to this session.
    #[serde(with = "time::serde::rfc3339::option")]
    pub confirmed_lease: Option<OffsetDateTime>,
}

/// What a session keeps for an outage. The views of one session share it.
#[derive(Debug, Default)]
pub(crate) struct Offline {
    /// The T0 that the session last loaded for each domain, and when.
    loaded: BTreeMap<String, (String, OffsetDateTime)>,
    /// The end of the last lease that the ledger confirmed to this session.
    pub(crate) lease: Option<OffsetDateTime>,
}

/// One line of the queue.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Queued {
    /// A handoff that a session wrote in the outage.
    Handoff {
        /// When it was queued.
        #[serde(with = "time::serde::rfc3339")]
        at: OffsetDateTime,
        /// What the session asked to write.
        request: Box<WriteRequest>,
        /// The author of the session.
        author: String,
        /// The id of the session.
        session: String,
        /// The task that the session held.
        task: Option<String>,
        /// The generation of the claim that the session last had on that task.
        generation: Option<u64>,
        /// The label of the session: the entry is classified no lower.
        label: Label,
        /// Whether the session had read content from outside the fleet.
        outside: bool,
    },
    /// A heartbeat that a session sent in the outage.
    Heartbeat {
        /// When it was queued.
        #[serde(with = "time::serde::rfc3339")]
        at: OffsetDateTime,
        /// The task.
        task: String,
        /// The holder that sent it.
        holder: String,
        /// The generation of the claim it was sent under.
        generation: u64,
        /// The lease asked, in milliseconds.
        lease_ms: i64,
    },
}

fn rfc3339(at: OffsetDateTime) -> String {
    at.format(&Rfc3339).unwrap_or_default()
}

/// A T0 with the line that says it is a copy, under the first heading.
fn with_outage_line(text: &str, outage: &Outage, loaded: OffsetDateTime) -> String {
    let note = format!(
        "> Outage since {}: this is the T0 that this session loaded at {}. Reads and changes wait.",
        rfc3339(outage.since),
        rfc3339(loaded)
    );
    match text.split_once('\n') {
        Some((heading, rest)) => format!("{heading}\n\n{note}\n{rest}"),
        None => format!("{text}\n\n{note}\n"),
    }
}

/// Whether the ledger refused a heartbeat, and did not fail.
fn refused(e: &dm_ledger::Error) -> bool {
    matches!(
        e,
        dm_ledger::Error::NotFound(_)
            | dm_ledger::Error::NotHolder { .. }
            | dm_ledger::Error::Fenced { .. }
            | dm_ledger::Error::WrongState { .. }
    )
}

impl Memory {
    /// The outage that is declared now. None when there is no outage. A file that
    /// cannot be read, or that does not parse, is an outage: its reason says so.
    #[must_use]
    pub fn outage(&self) -> Option<Outage> {
        let path = self.root.join(OUTAGE_FILE);
        let unreadable = |why: String| {
            let since = std::fs::metadata(&path)
                .and_then(|m| m.modified())
                .map_or_else(|_| OffsetDateTime::now_utc(), OffsetDateTime::from);
            Some(Outage {
                since,
                reason: format!("{OUTAGE_FILE} cannot be read ({why})"),
                declared_by: String::new(),
            })
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str(&text) {
                Ok(outage) => Some(outage),
                Err(e) => unreadable(e.to_string()),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => unreadable(e.to_string()),
        }
    }

    /// The outage as this session sees it: the declared outage, the domains whose T0
    /// the session loaded, and the last lease that the ledger confirmed to it. Any
    /// session may ask, in an outage too: nothing of the record is read.
    #[must_use]
    pub fn outage_status(&self) -> OutageStatus {
        let offline = lock(&self.offline);
        OutageStatus {
            outage: self.outage(),
            t0_loaded: offline.loaded.keys().cloned().collect(),
            confirmed_lease: offline.lease,
        }
    }

    /// The guard of each read and each change: an error while an outage is declared.
    /// The filter of each read calls it, and so does each decision of the policy. A
    /// change that passes neither of those two calls it first.
    ///
    /// # Errors
    ///
    /// [`Error::Outage`] while [`OUTAGE_FILE`] is there.
    pub(crate) fn serving(&self) -> Result<(), Error> {
        match self.outage() {
            None => Ok(()),
            Some(o) => Err(Error::Outage(format!(
                "an outage is declared since {} ({}). Reads and changes wait until it ends. \
                 Work from the T0 that this session loaded, and write a handoff: it is queued",
                rfc3339(o.since),
                o.reason
            ))),
        }
    }

    fn owner_alone(&self, what: &str) -> Result<(), Error> {
        if self.author == self.grants.owner {
            Ok(())
        } else {
            Err(Error::Denied(format!(
                "only the owner {} {what}",
                self.grants.owner
            )))
        }
    }

    /// Declare an outage: from now on each read and each change waits. An outage that
    /// is declared already stays as it is, and is returned.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for a session that is not the owner; [`Error::Stale`] when the
    /// grants changed; an I/O error.
    pub fn declare_outage(&self, reason: &str) -> Result<Outage, Error> {
        self.owner_alone("declares an outage")?;
        self.fresh()?;
        if let Some(declared) = self.outage() {
            return Ok(declared);
        }
        let outage = Outage {
            since: OffsetDateTime::now_utc(),
            reason: reason.to_string(),
            declared_by: self.author.clone(),
        };
        let path = self.root.join(OUTAGE_FILE);
        let text = serde_json::to_string(&outage).unwrap_or_default();
        std::fs::write(&path, text).map_err(io(&path))?;
        Ok(outage)
    }

    /// End the outage, and replay the queue in order. The outage file is removed first,
    /// so the replay goes through the ordinary write and the ordinary heartbeat. With no
    /// outage and no queue, nothing is done.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for a session that is not the owner; [`Error::Stale`] when the
    /// grants changed; an I/O error on the outage file or the queue. A queued line that
    /// fails is no error: [`Replayed::failed`] names it.
    pub async fn end_outage(&self) -> Result<Replayed, Error> {
        self.owner_alone("ends an outage")?;
        self.fresh()?;
        let path = self.root.join(OUTAGE_FILE);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io(&path)(e)),
        }
        let mut done = Replayed::default();
        for (n, line) in self.take_queue()?.lines().enumerate() {
            match serde_json::from_str::<Queued>(line) {
                Ok(queued) => self.replay(queued, &mut done).await,
                Err(e) => done
                    .failed
                    .push(format!("line {}: it does not parse ({e})", n + 1)),
            }
        }
        let replayed = self.root.join(REPLAY_FILE);
        if replayed.exists() {
            std::fs::remove_file(&replayed).map_err(io(&replayed))?;
        }
        Ok(done)
    }

    /// The text of the queue, moved aside so a line that comes later is not lost. A
    /// replay that stopped before its end left its file: its lines come first.
    fn take_queue(&self) -> Result<String, Error> {
        let (queue, replaying) = (self.root.join(QUEUE_FILE), self.root.join(REPLAY_FILE));
        let read = |path: &PathBuf| match std::fs::read_to_string(path) {
            Ok(text) => Ok(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(io(path)(e)),
        };
        let text = read(&replaying)? + &read(&queue)?;
        if text.is_empty() {
            return Ok(text);
        }
        std::fs::write(&replaying, &text).map_err(io(&replaying))?;
        if queue.exists() {
            std::fs::remove_file(&queue).map_err(io(&queue))?;
        }
        Ok(text)
    }

    async fn replay(&self, queued: Queued, done: &mut Replayed) {
        match queued {
            Queued::Heartbeat {
                task,
                holder,
                generation,
                lease_ms,
                ..
            } => {
                let by = dm_ledger::Held {
                    holder: &holder,
                    generation,
                };
                let lease = Duration::milliseconds(lease_ms);
                // the clock of the ledger decides whether the lease is still there
                match self.ledger.lock().await.heartbeat(&task, by, lease).await {
                    Ok(_) => done.heartbeats_applied += 1,
                    Err(e) if refused(&e) => done.heartbeats_refused += 1,
                    Err(e) => done
                        .failed
                        .push(format!("heartbeat of {holder} on {task}: {e}")),
                }
            }
            Queued::Handoff {
                request,
                author,
                session,
                task,
                generation,
                label,
                outside,
                ..
            } => {
                let from = Sender {
                    author,
                    session,
                    task,
                    generation,
                    label,
                    outside,
                };
                self.replay_handoff(*request, &from, done).await;
            }
        }
    }

    /// Write a queued handoff as its author, and leave it on the task of the session
    /// when the claim of that task is over.
    async fn replay_handoff(&self, request: WriteRequest, from: &Sender, done: &mut Replayed) {
        let written = match self.session_of(from).await {
            Ok(view) => view.write(request).await,
            Err(e) => Err(e),
        };
        let id = match written {
            Ok(w) => w.id,
            Err(e) => {
                done.failed.push(format!(
                    "handoff of {} in session {}: {e}",
                    from.author, from.session
                ));
                return;
            }
        };
        done.handoffs.push(id.clone());
        let (Some(task), Some(generation)) = (&from.task, from.generation) else {
            return;
        };
        let by = dm_ledger::Held {
            holder: &from.author,
            generation,
        };
        let left = self.ledger.lock().await.handoff_after(task, by, &id).await;
        match left {
            // a claim that is in force is completed or released by its holder, and a
            // closed task takes no handoff: the entry is in the record either way
            Ok(_) | Err(dm_ledger::Error::WrongState { .. }) => {}
            Err(e) => done.failed.push(format!(
                "handoff {id} is written, and is not on {task}: {e}"
            )),
        }
    }

    /// A session of this root that writes as the sender of a queued handoff did: its
    /// author, its session id, its held task, and no lower a label.
    async fn session_of(&self, from: &Sender) -> Result<Memory, Error> {
        let mut view = Memory::open(&self.root)?;
        view.session_id.clone_from(&from.session);
        view.reads = Arc::new(reads::ReadLog::new(&self.root, &from.session));
        let view = view.with_author(from.author.clone());
        {
            let mut s = lock(&view.session);
            s.observe(from.label);
            if from.outside {
                s.read_outside();
            }
        }
        if let Some(task) = &from.task {
            view.hold_task(task).await?;
        }
        Ok(view)
    }

    /// Keep the T0 that this session loaded for a domain: what it works from in an
    /// outage.
    pub(crate) fn keep_t0(&self, domain: &str, text: &str) {
        lock(&self.offline).loaded.insert(
            domain.to_string(),
            (text.to_string(), OffsetDateTime::now_utc()),
        );
    }

    /// The ledger confirmed a lease until `until`: a claim, a heartbeat, or the held
    /// task as the ledger has it.
    pub(crate) fn confirm_lease(&self, until: Option<OffsetDateTime>) {
        if until.is_some() {
            lock(&self.offline).lease = until;
        }
    }

    /// Whether this session holds a task: one a launcher set, or one it claimed.
    fn holds_a_task(&self) -> bool {
        self.held_task().is_some() || !lock(&self.claims).is_empty()
    }

    /// The T0 in an outage, at the time `now`: the copy that this session loaded for the
    /// domain, with the line that says so.
    ///
    /// # Errors
    ///
    /// [`Error::Outage`] for a session that loaded no T0 for the domain, and for a
    /// session that holds a task when `now` is after the last lease that the ledger
    /// confirmed; [`Error::Stale`] when the grants changed.
    pub(crate) fn t0_in_outage(
        &self,
        domain: &str,
        outage: &Outage,
        now: OffsetDateTime,
    ) -> Result<String, Error> {
        self.fresh()?;
        let offline = lock(&self.offline);
        let since = rfc3339(outage.since);
        let Some((text, loaded)) = offline.loaded.get(domain) else {
            return Err(Error::Outage(format!(
                "an outage is declared since {since}, and this session loaded no T0 for \
                 {domain} before it. The session waits"
            )));
        };
        if self.holds_a_task() && offline.lease.is_none_or(|until| now > until) {
            let until = offline.lease.map_or_else(
                || "no lease was confirmed".to_string(),
                |t| format!("the last confirmed lease ended at {}", rfc3339(t)),
            );
            return Err(Error::Outage(format!(
                "an outage is declared since {since}, and {until}. The session stops its \
                 work on the task and writes its handoff: the handoff is queued"
            )));
        }
        Ok(with_outage_line(text, outage, *loaded))
    }

    /// A write in an outage. A handoff is put on the queue. A write of another type is
    /// refused, and nothing is queued. The error is the answer in each case.
    pub(crate) fn write_in_outage(&self, req: WriteRequest, outage: &Outage) -> Error {
        let since = rfc3339(outage.since);
        if req.entry_type != EntryType::Handoff {
            return Error::Outage(format!(
                "an outage is declared since {since}. A write of type {} waits until it \
                 ends, and nothing is queued. Only a handoff is queued",
                req.entry_type
            ));
        }
        match self.queue_handoff(req) {
            Ok(()) => Error::Outage(format!(
                "an outage is declared since {since}. The handoff is queued. It lands in \
                 the record when the outage ends, if the gates allow it"
            )),
            Err(e) => e,
        }
    }

    fn queue_handoff(&self, req: WriteRequest) -> Result<(), Error> {
        self.fresh()?;
        self.resolve_domain(req.domain.as_deref())?;
        if req.body.trim().is_empty() {
            return Err(Error::Denied("an entry needs a body".into()));
        }
        let task = self.held_task();
        let generation = task
            .as_ref()
            .and_then(|t| lock(&self.claims).get(t).copied());
        self.queue(&Queued::Handoff {
            at: OffsetDateTime::now_utc(),
            request: Box::new(req),
            author: self.author.clone(),
            session: self.session_id.clone(),
            task,
            generation,
            label: self.label(),
            outside: self.read_outside(),
        })
    }

    /// A heartbeat in an outage: it is put on the queue, and it confirms no lease. The
    /// error is the answer.
    pub(crate) fn heartbeat_in_outage(
        &self,
        id: &str,
        generation: Option<u64>,
        lease: Duration,
        outage: &Outage,
    ) -> Error {
        let queued = self.fresh().and_then(|()| {
            self.queue(&Queued::Heartbeat {
                at: OffsetDateTime::now_utc(),
                task: id.to_string(),
                holder: self.author.clone(),
                generation: self.claim_of(id, generation)?,
                lease_ms: i64::try_from(lease.whole_milliseconds()).unwrap_or(i64::MAX),
            })
        });
        match queued {
            Ok(()) => Error::Outage(format!(
                "an outage is declared since {}. The heartbeat on {id} is queued and is \
                 not confirmed: the lease ends when the ledger last said. The ledger \
                 decides at the end of the outage",
                rfc3339(outage.since)
            )),
            Err(e) => e,
        }
    }

    fn queue(&self, line: &Queued) -> Result<(), Error> {
        let path = self.root.join(QUEUE_FILE);
        let text = serde_json::to_string(line).unwrap_or_default();
        reads::try_append_line(&path, &text).map_err(io(&path))
    }
}

/// Who queued a handoff, and under what.
struct Sender {
    author: String,
    session: String,
    task: Option<String>,
    generation: Option<u64>,
    label: Label,
    outside: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log;
    use crate::recall::Budget;
    use dm_core::erasure::Reason;
    use dm_ledger::{LateHandoff, OutcomeKind, Status, TaskSpec};
    use std::path::Path;

    const OWNER: &str = "human:test:me";
    const DOMAIN: &str = "dom-me";
    const AGENT: &str = "agent:x";
    const FACT: &str = "# Retries\n\nThe relay retries a failed send three times.\n";
    const HANDOFF: &str = "# Handoff\n\nThe relay is half done. The tests of the retry are next.\n";

    fn init(dir: &Path) -> Memory {
        Memory::init(dir, OWNER, DOMAIN).unwrap()
    }

    fn agent(dir: &Path) -> Memory {
        Memory::open(dir).unwrap().with_author(AGENT)
    }

    fn entry(kind: EntryType, body: &str) -> WriteRequest {
        WriteRequest {
            entry_type: kind,
            body: body.into(),
            ..WriteRequest::default()
        }
    }

    fn spec(title: &str) -> TaskSpec {
        TaskSpec {
            domain: DOMAIN.into(),
            title: title.into(),
            body: "Do it.".into(),
            ..TaskSpec::default()
        }
    }

    /// An admitted task that the owner filed.
    async fn task(owner: &Memory, title: &str) -> String {
        let filed = owner.file_own(spec(title)).await.unwrap();
        assert_eq!(filed.task.status, Status::Open);
        filed.task.id
    }

    /// The text of the refusal, for an action that waited.
    fn waits<T>(r: Result<T, Error>) -> String {
        match r {
            Err(Error::Outage(why)) => why,
            Err(other) => panic!("the action did not wait: {other}"),
            Ok(_) => panic!("the action did not wait: it was done"),
        }
    }

    #[tokio::test]
    async fn the_status_names_the_outage_the_loaded_t0_and_the_confirmed_lease() {
        let dir = tempfile::tempdir().unwrap();
        let owner = init(dir.path());
        let id = task(&owner, "Relay").await;
        let worker = agent(dir.path());
        let before = worker.outage_status();
        assert_eq!(before.outage, None);
        assert!(before.t0_loaded.is_empty());
        assert_eq!(before.confirmed_lease, None);
        worker.t0(None).await.unwrap();
        let claim = worker.claim(&id, Duration::minutes(30)).await.unwrap();
        owner.declare_outage("the ledger is moved").unwrap();
        let status = worker.outage_status();
        assert_eq!(status.outage, owner.outage());
        assert_eq!(status.t0_loaded, vec![DOMAIN.to_string()]);
        assert_eq!(status.confirmed_lease, Some(claim.lease_until));
        // a session that loaded nothing says so, and the status itself does not wait
        let late = agent(dir.path()).outage_status();
        assert!(late.outage.is_some() && late.t0_loaded.is_empty());
        let text = serde_json::to_value(&status).unwrap();
        assert_eq!(text["outage"]["reason"], "the ledger is moved");
        assert!(text["confirmed_lease"].is_string(), "{text}");
    }

    /// How many events the ledger holds for the domain.
    async fn events(m: &Memory) -> usize {
        let l = m.ledger.lock().await;
        l.events_after(DOMAIN, 0).await.unwrap().len()
    }

    /// Each file below the record, with its bytes.
    fn record(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        let mut out = BTreeMap::new();
        let mut dirs = vec![root.join("record")];
        while let Some(dir) = dirs.pop() {
            for item in std::fs::read_dir(&dir).unwrap() {
                let path = item.unwrap().path();
                if path.is_dir() {
                    dirs.push(path);
                } else {
                    out.insert(path.clone(), std::fs::read(&path).unwrap());
                }
            }
        }
        out
    }

    fn log_len(root: &Path) -> usize {
        log::read(root, DOMAIN, 0).unwrap().len()
    }

    #[tokio::test]
    async fn each_read_waits_in_an_outage_and_is_served_after_it() {
        let dir = tempfile::tempdir().unwrap();
        let owner = init(dir.path());
        let fact = owner.write(entry(EntryType::Fact, FACT)).await.unwrap();
        let a = agent(dir.path());
        assert_eq!(a.search("retries", None, 5).await.unwrap().len(), 1);
        owner.declare_outage("the ledger does not answer").unwrap();
        let why = waits(a.search("retries", None, 5).await);
        assert!(why.contains("the ledger does not answer"), "{why}");
        waits(a.get(&fact.id).await);
        waits(a.list(None, None, 5).await);
        waits(
            a.recall("how many retries", None, None, Budget::default())
                .await,
        );
        // the owner waits as an agent does
        waits(owner.search("retries", None, 5).await);
        waits(owner.get(&fact.id).await);
        owner.end_outage().await.unwrap();
        assert_eq!(a.get(&fact.id).await.unwrap().front.id, fact.id);
    }

    #[tokio::test]
    async fn t0_in_an_outage_is_the_copy_that_the_session_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let owner = init(dir.path());
        owner.write(entry(EntryType::Fact, FACT)).await.unwrap();
        let a = agent(dir.path());
        let loaded = a.t0(None).await.unwrap();
        let none_loaded = agent(dir.path());
        let outage = owner.declare_outage("the index is down").unwrap();
        let text = a.t0(None).await.unwrap();
        let line = text.lines().nth(2).unwrap();
        let start = format!(
            "> Outage since {}: this is the T0 that this session loaded at ",
            rfc3339(outage.since)
        );
        assert!(line.starts_with(&start), "{line}");
        assert!(line.ends_with(". Reads and changes wait."), "{line}");
        // the line is under the first heading, and the rest is the text loaded
        assert!(text.starts_with(loaded.lines().next().unwrap()));
        assert_eq!(text.replacen(&format!("\n{line}\n"), "", 1), loaded);
        // a view of the session has the copy of the session
        assert_eq!(a.for_request().unwrap().t0(None).await.unwrap(), text);
        let why = waits(none_loaded.t0(None).await);
        assert!(why.contains("loaded no T0"), "{why}");
    }

    #[tokio::test]
    async fn a_session_with_a_task_gets_t0_until_its_last_confirmed_lease() {
        let dir = tempfile::tempdir().unwrap();
        let owner = init(dir.path());
        let id = task(&owner, "Build the relay").await;
        let a = agent(dir.path());
        let claim = a.claim(&id, Duration::minutes(10)).await.unwrap();
        a.t0(None).await.unwrap();
        let outage = owner.declare_outage("the ledger does not answer").unwrap();
        let at = |time| a.t0_in_outage(DOMAIN, &outage, time);
        assert!(at(claim.lease_until - Duration::seconds(1)).is_ok());
        assert!(at(claim.lease_until).is_ok());
        let after = claim.lease_until + Duration::seconds(1);
        let why = waits(at(after));
        assert!(why.contains("the last confirmed lease ended"), "{why}");
        // a heartbeat that is queued confirms no lease
        waits(a.heartbeat(&id, None, Duration::hours(5)).await);
        waits(at(after));
        // the session's own clock is inside the lease now
        assert!(a.t0(None).await.is_ok());
    }

    #[tokio::test]
    async fn each_change_of_the_ledger_waits_and_the_ledger_takes_no_event() {
        let dir = tempfile::tempdir().unwrap();
        let owner = init(dir.path());
        let held = task(&owner, "Held").await;
        let open = task(&owner, "Open").await;
        let a = agent(dir.path());
        let proposed = a.create_task(spec("Proposed")).await.unwrap().task.id;
        a.claim(&held, Duration::minutes(10)).await.unwrap();
        let handoff = a.write(entry(EntryType::Handoff, HANDOFF)).await.unwrap();
        owner.declare_outage("the ledger does not answer").unwrap();
        let before = events(&owner).await;
        let lease = Duration::minutes(10);
        waits(a.claim_next(None, lease).await);
        waits(a.claim(&open, lease).await);
        waits(a.complete(&held, None, &handoff.id).await);
        waits(a.release(&held, None, None).await);
        waits(a.create_task(spec("New")).await);
        waits(owner.file_own(spec("Own")).await);
        waits(owner.admit(&proposed).await);
        waits(owner.invalidate(&open, "obsolete", None).await);
        waits(
            a.fan_out(&held, &[("Part".into(), "Do a part.".into())])
                .await,
        );
        waits(owner.retry_task(&open).await);
        waits(
            owner
                .record_outcome(&held, OutcomeKind::Accepted, "taken")
                .await,
        );
        assert_eq!(events(&owner).await, before);
        let l = owner.ledger.lock().await;
        assert_eq!(l.get(&held).await.unwrap().status, Status::Claimed);
        assert_eq!(l.get(&open).await.unwrap().status, Status::Open);
    }

    #[tokio::test]
    async fn erase_export_and_a_bundle_wait_and_the_record_is_as_it_was() {
        let dir = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let owner = init(dir.path());
        let fact = owner.write(entry(EntryType::Fact, FACT)).await.unwrap();
        owner.declare_outage("the log does not answer").unwrap();
        let before = record(dir.path());
        waits(owner.erase(&fact.id, Reason::Mistaken).await);
        let bundle = out.path().join("bundle");
        waits(owner.export(None, Label::Internal, &bundle).await);
        let recovery = out.path().join("recovery");
        waits(owner.recovery_bundle(&recovery).await);
        waits(owner.import(&bundle, None).await);
        waits(owner.release_quarantined("no-such-entry", true).await);
        assert_eq!(record(dir.path()), before);
        assert!(!bundle.exists() && !recovery.exists());
    }

    #[tokio::test]
    async fn a_fact_written_in_an_outage_is_refused_and_is_not_queued() {
        let dir = tempfile::tempdir().unwrap();
        let owner = init(dir.path());
        let a = agent(dir.path());
        owner.declare_outage("the log does not answer").unwrap();
        let before = (record(dir.path()), log_len(dir.path()));
        let why = waits(a.write(entry(EntryType::Fact, FACT)).await);
        assert!(why.contains("nothing is queued"), "{why}");
        assert!(!dir.path().join(QUEUE_FILE).exists());
        assert_eq!((record(dir.path()), log_len(dir.path())), before);
    }

    #[tokio::test]
    async fn a_handoff_written_in_an_outage_lands_at_the_end_with_its_author() {
        let dir = tempfile::tempdir().unwrap();
        let owner = init(dir.path());
        let a = agent(dir.path());
        owner.declare_outage("the log does not answer").unwrap();
        let before = (record(dir.path()), log_len(dir.path()));
        let why = waits(a.write(entry(EntryType::Handoff, HANDOFF)).await);
        assert!(why.contains("The handoff is queued"), "{why}");
        // not in the record and not in the log
        assert_eq!((record(dir.path()), log_len(dir.path())), before);
        let done = owner.end_outage().await.unwrap();
        assert_eq!(done.failed, Vec::<String>::new());
        assert_eq!(done.handoffs.len(), 1);
        let landed = owner.get(&done.handoffs[0]).await.unwrap();
        assert_eq!(landed.front.author, AGENT);
        assert_eq!(landed.front.entry_type, EntryType::Handoff);
        assert_eq!(landed.body, HANDOFF);
        // the log has the write under the session that queued it
        let session = landed.front.session.unwrap();
        assert!(session.starts_with(a.session_id()), "{session}");
        // the queue is gone: a second end replays nothing
        assert!(!dir.path().join(QUEUE_FILE).exists());
        assert_eq!(owner.end_outage().await.unwrap(), Replayed::default());
    }

    #[tokio::test]
    async fn a_heartbeat_queued_inside_the_lease_is_applied_at_the_end() {
        let dir = tempfile::tempdir().unwrap();
        let owner = init(dir.path());
        let id = task(&owner, "Build the relay").await;
        let a = agent(dir.path());
        let claim = a.claim(&id, Duration::minutes(10)).await.unwrap();
        owner.declare_outage("the ledger does not answer").unwrap();
        let before = events(&owner).await;
        let why = waits(a.heartbeat(&id, None, Duration::hours(2)).await);
        assert!(why.contains("queued and is not confirmed"), "{why}");
        assert_eq!(events(&owner).await, before);
        let done = owner.end_outage().await.unwrap();
        assert_eq!((done.heartbeats_applied, done.heartbeats_refused), (1, 0));
        assert_eq!(done.failed, Vec::<String>::new());
        let now = owner.get_task(&id).await.unwrap();
        assert_eq!(now.holder.as_deref(), Some(AGENT));
        assert!(now.lease_until.unwrap() > claim.lease_until + Duration::hours(1));
    }

    #[tokio::test]
    async fn a_heartbeat_for_a_lease_that_ran_out_does_not_give_the_claim_back() {
        let dir = tempfile::tempdir().unwrap();
        let owner = init(dir.path());
        let id = task(&owner, "Build the relay").await;
        let a = agent(dir.path());
        let claim = a.claim(&id, Duration::minutes(10)).await.unwrap();
        a.set_held_task(&id);
        owner.declare_outage("the ledger does not answer").unwrap();
        waits(a.heartbeat(&id, None, Duration::hours(2)).await);
        waits(a.write(entry(EntryType::Handoff, HANDOFF)).await);
        // the outage ends an hour later by the clock of the ledger
        let later = OffsetDateTime::now_utc() + Duration::hours(1);
        owner.ledger.lock().await.set_clock(move || later);
        let done = owner.end_outage().await.unwrap();
        assert_eq!((done.heartbeats_applied, done.heartbeats_refused), (0, 1));
        assert_eq!(done.failed, Vec::<String>::new());
        assert_eq!(done.handoffs.len(), 1);
        // the task is in the pool: another session claims it
        let b = Memory::open(dir.path()).unwrap().with_author("agent:y");
        b.ledger.lock().await.set_clock(move || later);
        let again = b.claim(&id, Duration::minutes(10)).await.unwrap();
        assert_eq!(again.task.holder.as_deref(), Some("agent:y"));
        assert_eq!(again.generation, claim.generation + 1);
        // the handoff of the lost claim is on the task as a late handoff
        let (late, _) = owner.task_record(&id).await.unwrap();
        let left = LateHandoff {
            holder: AGENT.into(),
            generation: claim.generation,
            handoff: done.handoffs[0].clone(),
        };
        assert_eq!(late, vec![left]);
        assert_eq!(
            owner.get(&done.handoffs[0]).await.unwrap().front.task,
            Some(id)
        );
    }

    #[tokio::test]
    async fn only_the_owner_declares_and_ends_an_outage() {
        let dir = tempfile::tempdir().unwrap();
        let owner = init(dir.path());
        let a = agent(dir.path());
        let refused = a.declare_outage("an agent says so").unwrap_err();
        assert!(matches!(refused, Error::Denied(_)), "{refused}");
        assert_eq!(owner.outage(), None);
        let declared = owner.declare_outage("the ledger does not answer").unwrap();
        assert_eq!(declared.declared_by, OWNER);
        // a second declaration keeps the first
        assert_eq!(owner.declare_outage("again").unwrap(), declared);
        let refused = a.end_outage().await.unwrap_err();
        assert!(matches!(refused, Error::Denied(_)), "{refused}");
        assert_eq!(a.outage(), Some(declared));
        owner.end_outage().await.unwrap();
        assert_eq!(a.outage(), None);
    }

    #[tokio::test]
    async fn an_outage_does_not_stop_the_session_or_its_model() {
        let dir = tempfile::tempdir().unwrap();
        let owner = init(dir.path());
        let a = agent(dir.path());
        owner.declare_outage("the ledger does not answer").unwrap();
        // the egress asks this before each model call
        a.fresh().unwrap();
        assert!(a.check_spend("anthropic", 0, 0).unwrap().allow);
        // a view is made, and each read of the view waits
        let view = a.for_request().unwrap();
        waits(view.search("retries", None, 5).await);
    }

    #[tokio::test]
    async fn an_outage_file_that_does_not_parse_is_an_outage() {
        let dir = tempfile::tempdir().unwrap();
        let owner = init(dir.path());
        std::fs::write(dir.path().join(OUTAGE_FILE), "not json").unwrap();
        let outage = owner.outage().unwrap();
        assert!(outage.reason.contains("cannot be read"), "{outage:?}");
        waits(owner.search("retries", None, 5).await);
        owner.end_outage().await.unwrap();
        assert_eq!(owner.outage(), None);
        owner.search("retries", None, 5).await.unwrap();
    }

    #[tokio::test]
    async fn a_queued_line_that_fails_is_reported_and_the_rest_is_replayed() {
        let dir = tempfile::tempdir().unwrap();
        let owner = init(dir.path());
        let a = agent(dir.path());
        owner.declare_outage("the log does not answer").unwrap();
        let queue = dir.path().join(QUEUE_FILE);
        reads::try_append_line(&queue, "{\"kind\":\"nothing\"}").unwrap();
        waits(a.write(entry(EntryType::Handoff, HANDOFF)).await);
        let done = owner.end_outage().await.unwrap();
        assert_eq!(done.handoffs.len(), 1);
        assert_eq!(done.failed.len(), 1);
        assert!(done.failed[0].starts_with("line 1: "), "{:?}", done.failed);
    }
}
