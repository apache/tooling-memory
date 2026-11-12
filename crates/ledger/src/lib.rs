//! The task ledger: tasks, leases, handoffs, origin dedup, and the record of why each
//! entry was promoted.
//!
//! The ledger is event-sourced. Every operation appends one event to an append-only
//! table and updates a projection of current task state in the same transaction. The
//! projection is a cache: [`Ledger::rebuild`] drops it and replays the events, and a test
//! shows the result identical. That is the paper's "projection plus lease cache,
//! rebuildable from the log", with a database standing in for the log.
//!
//! One type, two backends, chosen when the ledger is opened: SQLite, one file on a laptop
//! ([`Ledger::open`], [`Ledger::in_memory`]), and PostgreSQL, a server several processes
//! share ([`Ledger::connect`]). The rules, the events and the projection are the same on
//! both; only the SQL that holds them differs, and the same tests run against each.
//!
//! Three rules hold:
//!
//! - **A task is born proposed and becomes claimable only once admitted.** Anything may
//!   file a task; admission is a separate act.
//! - **One holder at a time.** A claim is one transaction over the ledger, so two callers
//!   racing for the same task cannot both win. On PostgreSQL the candidate rows are taken
//!   `FOR UPDATE SKIP LOCKED`, so that holds across connections too.
//! - **A lease that cannot be renewed is lost at its expiry**, for the holder and the
//!   ledger alike. An expired lease makes the task claimable again; the stale holder's
//!   heartbeat, release, and complete are refused.
//!
//! A promotion is recorded on the same log, as an event of its own under the key
//! `promotion:<entry>`, and projected into a table of its own. It is no task: nothing
//! claims it, lists it, or reads it as delivered work. It is kept once per version of the
//! entry, and a rebuild replays it with the rest.
//!
//! Time comes from a clock the ledger owns, so tests can move it.

mod postgres;
mod sqlite;
#[cfg(any(test, feature = "pg-test"))]
pub mod testing;

use dm_core::{Label, TrustTier};
use serde::{Deserialize, Serialize};
use std::path::Path;
use time::{format_description::well_known::Rfc3339, Duration, OffsetDateTime};

/// What can go wrong at the ledger.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// SQLite refused or failed.
    #[error("ledger: {0}")]
    Db(#[from] rusqlite::Error),
    /// PostgreSQL refused or failed, or could not be reached.
    #[error("ledger: {0}")]
    Postgres(#[from] tokio_postgres::Error),
    /// The runtime a PostgreSQL ledger needs could not be started.
    #[error("ledger: {0}")]
    Runtime(String),
    /// No task with that id.
    #[error("no task {0}")]
    NotFound(String),
    /// The task is not in a state where the operation applies.
    #[error("task {id} is {status:?}; cannot {op}")]
    WrongState {
        /// The task.
        id: String,
        /// Its current status.
        status: Status,
        /// What was attempted.
        op: &'static str,
    },
    /// The caller does not hold the task, or held it and the lease expired.
    #[error("task {id} is not held by {holder}")]
    NotHolder {
        /// The task.
        id: String,
        /// Who asked.
        holder: String,
    },
    /// The caller holds a claim of an earlier generation: the task was claimed again since.
    #[error("task {id} is at generation {current}; the claim of generation {given} is over")]
    Fenced {
        /// The task.
        id: String,
        /// The generation the caller carries.
        given: u64,
        /// The generation of the claim in force.
        current: u64,
    },
    /// Another connection holds the write-ahead log, so text just removed may still be
    /// in it. Run the operation again.
    #[error("ledger: another connection holds the log: {0}")]
    Held(String),
    /// A row holds something the ledger did not write. Rebuild it.
    #[error("ledger row for {id}: {what}")]
    BadRow {
        /// The task.
        id: String,
        /// What was wrong.
        what: String,
    },
}

/// Where a task is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Filed, not yet admitted. Not claimable.
    Proposed,
    /// Admitted by triage and waiting for a holder.
    Open,
    /// Held under a lease.
    Claimed,
    /// Not claimable until something outside the task is settled: a dependency that is
    /// not done, an event that has not come, or an effect nobody has reconciled.
    Blocked,
    /// Done, with a handoff.
    Done,
    /// Closed without being done: obsolete, invalid, or a duplicate.
    Invalidated,
}

impl Status {
    /// The status as the ledger stores it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Proposed => "proposed",
            Status::Open => "open",
            Status::Claimed => "claimed",
            Status::Done => "done",
            Status::Blocked => "blocked",
            Status::Invalidated => "invalidated",
        }
    }

    fn parse(id: &str, s: &str) -> Result<Status, Error> {
        Ok(match s {
            "proposed" => Status::Proposed,
            "open" => Status::Open,
            "claimed" => Status::Claimed,
            "done" => Status::Done,
            "blocked" => Status::Blocked,
            "invalidated" => Status::Invalidated,
            other => {
                return Err(Error::BadRow {
                    id: id.to_string(),
                    what: format!("status {other:?}"),
                })
            }
        })
    }

    /// Whether no further operation applies.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Status::Done | Status::Invalidated)
    }
}

/// What a caller files. Everything else on a [`Task`] the ledger fills in.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskSpec {
    /// The domain whose ledger partition holds it.
    pub domain: String,
    /// One line.
    pub title: String,
    /// What is to be done, in Markdown.
    pub body: String,
    /// Capabilities a session's manifest must carry to claim it, such as `tool:github`.
    #[serde(default)]
    pub requirements: Vec<String>,
    /// Tasks that must be completed first.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Where the task came from, when a tap or an import filed it.
    #[serde(default)]
    pub origin: Option<String>,
    /// The upstream event that produced it, for dedup with `origin`.
    #[serde(default)]
    pub upstream_event: Option<String>,
    /// Who filed it, system-qualified.
    pub created_by: String,
    /// How sensitive the task and its handoff are.
    #[serde(default)]
    pub classification: Label,
    /// Higher is claimed first.
    #[serde(default)]
    pub priority: i64,
    /// The task definition this task runs under, as `<name>@<version>`: the record in
    /// the platform's domain for its kind of work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<String>,
    /// The checks that say this task is done, its own or those its definition gave it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub acceptance: Vec<String>,
}

/// The fields of a task that the ledger keeps and never queries by, as one JSON column.
#[derive(Default, Serialize, Deserialize)]
struct Extra {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    definition: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    acceptance: Vec<String>,
}

impl TaskSpec {
    /// The `extra` column of this spec's row.
    pub(crate) fn extra(&self) -> String {
        let extra = Extra {
            definition: self.definition.clone(),
            acceptance: self.acceptance.clone(),
        };
        serde_json::to_string(&extra).unwrap_or_default()
    }
}

/// A task as the ledger holds it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    /// Stable, permanent, meaningless.
    pub id: String,
    /// What was filed.
    #[serde(flatten)]
    pub spec: TaskSpec,
    /// When it was filed.
    #[serde(with = "time::serde::rfc3339")]
    pub created: OffsetDateTime,
    /// Where it is.
    pub status: Status,
    /// Who holds it, while claimed.
    pub holder: Option<String>,
    /// When the current lease runs out, while claimed.
    #[serde(with = "time::serde::rfc3339::option")]
    pub lease_until: Option<OffsetDateTime>,
    /// The generation of its latest claim: how many times it has been claimed. Every
    /// claim gets the next one, and what a holder does under a claim carries it, so a
    /// holder of an earlier claim is told apart from the holder now, whatever its name.
    pub generation: u64,
    /// The handoff entry that closed it, or the last one a lost lease left behind.
    pub handoff: Option<String>,
    /// Why it was invalidated.
    pub reason: Option<String>,
    /// The task that replaced it, when invalidated as a duplicate.
    pub superseded_by: Option<String>,
    /// Where its handoff was posted back upstream, once a tap has done so.
    #[serde(default)]
    pub posted_to: Option<String>,
}

/// What an erasure did to an entry's promotion records and approval tasks.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Scrubbed {
    /// How many records of the entry hold no why now.
    pub records: usize,
    /// How many tasks filed from the entry's origin hold the redaction now.
    pub tasks: usize,
    /// Copies of the text the ledger could not remove, named so nobody believes it did.
    pub beyond_reach: Vec<String>,
}

/// What an erasure rewrites a task to when the task is about the erased entry: a title
/// and a body that name the entry by its id and nothing else of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Redaction {
    /// The title every task gets.
    pub title: String,
    /// The body every task gets.
    pub body: String,
    /// Why an open task is invalidated.
    pub reason: String,
}

/// The result of filing a task.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Created {
    /// The task, new or already there.
    pub task: Task,
    /// True when the same origin and upstream event had already been filed and that
    /// task is returned instead of a second one.
    pub deduplicated: bool,
}

/// A granted claim.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Claim {
    /// The task.
    pub task: Task,
    /// When to heartbeat by.
    pub lease_until: OffsetDateTime,
    /// The claim's generation: what the holder passes with each heartbeat, completion
    /// and release.
    pub generation: u64,
}

/// A holder and the generation of the claim it acts under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Held<'a> {
    /// Who.
    pub holder: &'a str,
    /// Under which claim.
    pub generation: u64,
}

/// A handoff a holder left after its claim was over: its lease ran out, and it still
/// had something to say to whoever holds the task next.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LateHandoff {
    /// Who left it.
    pub holder: String,
    /// The generation of the claim it had held.
    pub generation: u64,
    /// The handoff entry.
    pub handoff: String,
}

/// What a promotion records: which version of an entry was moved to which tier, by
/// whom, and why. A record of a change already judged, not work: it is no task, and no
/// query over tasks returns it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromotionSpec {
    /// The domain the entry lives in, whose ledger keeps the record.
    pub domain: String,
    /// The entry promoted.
    pub entry: String,
    /// The tier it was promoted to.
    pub tier: TrustTier,
    /// Who promoted it, system-qualified.
    pub by: String,
    /// Who wrote the entry.
    pub author: String,
    /// Why: what was checked, and against what.
    pub why: String,
    /// How sensitive the record is: at least the entry's classification.
    pub classification: Label,
    /// The digest of the version of the entry that was promoted.
    pub digest: String,
    /// The digest of the version the promotion writes, so a record whose version never
    /// reached the record can be told from one whose version did. Empty in a record from
    /// before it was kept.
    #[serde(default)]
    pub new_digest: String,
}

/// A promotion as the ledger holds it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromotionRecord {
    /// What was recorded.
    #[serde(flatten)]
    pub spec: PromotionSpec,
    /// The why was removed by an erasure of the entry: `why` is then empty, and the rest
    /// is kept as the least an audit needs.
    #[serde(default)]
    pub why_erased: bool,
    /// When.
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
}

type Clock = Box<dyn Fn() -> OffsetDateTime + Send + Sync>;

/// One appended event, as replay reads it.
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum Event {
    Created {
        id: String,
        spec: TaskSpec,
    },
    Admitted,
    /// Admitted with a dependency that is not done: not claimable until each is.
    Blocked {
        on: Vec<String>,
    },
    /// The last dependency a blocked task waited on is done.
    Unblocked {
        by: String,
    },
    /// A dependency was invalidated, so what this task rests on changed: back to triage.
    Retriaged {
        because: String,
    },
    Claimed {
        holder: String,
        #[serde(with = "time::serde::rfc3339")]
        until: OffsetDateTime,
    },
    /// A claim of a named task the ledger refused, and why. Changes nothing; it is the
    /// record that someone tried.
    ClaimRefused {
        holder: String,
        why: String,
    },
    /// A handoff from the holder of an earlier claim. Changes nothing of the task.
    LateHandoff {
        holder: String,
        generation: u64,
        handoff: String,
    },
    Heartbeat {
        #[serde(with = "time::serde::rfc3339")]
        until: OffsetDateTime,
    },
    Released,
    /// A handoff left on a task that is going back to the pool, so the next claimant
    /// starts where this one stopped. Appended before `Released`.
    HandoffLeft {
        handoff: String,
    },
    Completed {
        handoff: String,
    },
    Invalidated {
        reason: String,
        superseded_by: Option<String>,
    },
    /// The handoff of a completed task was posted back where the task came from.
    Posted {
        to: String,
    },
    /// The rubric ranked an open task again: its place in the queue, and why. The
    /// priority it was filed with stays in its `Created` event.
    Rescored {
        priority: i64,
        why: String,
    },
    /// An entry was promoted, and why. Appended under [`promotion_key`], never a task id.
    /// An erasure of the entry rewrites it with the why removed and `why_erased` set.
    Promoted {
        #[serde(flatten)]
        spec: PromotionSpec,
        #[serde(default, skip_serializing_if = "is_false")]
        why_erased: bool,
    },
}

impl Event {
    /// The `kind` column: what the event is, readable without the payload.
    fn kind(&self) -> &'static str {
        match self {
            Event::Created { .. } => "created",
            Event::Admitted => "admitted",
            Event::Blocked { .. } => "blocked",
            Event::Unblocked { .. } => "unblocked",
            Event::Retriaged { .. } => "retriaged",
            Event::Claimed { .. } => "claimed",
            Event::ClaimRefused { .. } => "claim_refused",
            Event::LateHandoff { .. } => "late_handoff",
            Event::Heartbeat { .. } => "heartbeat",
            Event::Released => "released",
            Event::HandoffLeft { .. } => "handoff_left",
            Event::Completed { .. } => "completed",
            Event::Invalidated { .. } => "invalidated",
            Event::Posted { .. } => "posted",
            Event::Rescored { .. } => "rescored",
            Event::Promoted { .. } => "promoted",
        }
    }

    /// The `payload` column: the event as JSON.
    fn payload(&self, id: &str) -> Result<String, Error> {
        serde_json::to_string(self).map_err(|e| Error::BadRow {
            id: id.to_string(),
            what: format!("event serialises: {e}"),
        })
    }

    fn parse(id: &str, payload: &str) -> Result<Event, Error> {
        serde_json::from_str(payload).map_err(|e| Error::BadRow {
            id: id.to_string(),
            what: format!("event payload: {e}"),
        })
    }
}

/// What a PostgreSQL ledger keeps of a why after the rows are rewritten: the server, not
/// the ledger, decides when old row versions and its log are gone.
const POSTGRES_RESIDUE: &str = "the ledger's PostgreSQL server: old row versions and its write-ahead log keep removed text until the server vacuums and recycles them";

/// A task event's payload with the redaction in place of its free text: a `created`
/// event's title and body, an `invalidated` event's reason, and the handoff of a
/// `handoff_left` or `completed` event, each text a person may have put the entry's text
/// in. `None` when it holds the redaction already, or carries no text.
fn redact_event(id: &str, payload: &str, redaction: &Redaction) -> Result<Option<String>, Error> {
    let redacted = match Event::parse(id, payload)? {
        Event::Created {
            id: new_id,
            mut spec,
        } if spec.title != redaction.title || spec.body != redaction.body => {
            spec.title.clone_from(&redaction.title);
            spec.body.clone_from(&redaction.body);
            Event::Created { id: new_id, spec }
        }
        Event::Invalidated {
            reason,
            superseded_by,
        } if reason != redaction.reason => Event::Invalidated {
            reason: redaction.reason.clone(),
            superseded_by,
        },
        Event::HandoffLeft { handoff } if handoff != redaction.reason => Event::HandoffLeft {
            handoff: redaction.reason.clone(),
        },
        Event::Completed { handoff } if handoff != redaction.reason => Event::Completed {
            handoff: redaction.reason.clone(),
        },
        Event::LateHandoff {
            holder,
            generation,
            handoff,
        } if handoff != redaction.reason => Event::LateHandoff {
            holder,
            generation,
            handoff: redaction.reason.clone(),
        },
        _ => return Ok(None),
    };
    Ok(Some(redacted.payload(id)?))
}

/// A `promoted` event's payload with its why removed, when the event is that domain's
/// and holds a why; `None` when there is nothing to rewrite.
fn scrub_payload(key: &str, domain: &str, payload: &str) -> Result<Option<String>, Error> {
    match Event::parse(key, payload)? {
        Event::Promoted {
            mut spec,
            why_erased,
        } if spec.domain == domain && !(why_erased && spec.why.is_empty()) => {
            spec.why.clear();
            let scrubbed = Event::Promoted {
                spec,
                why_erased: true,
            };
            Ok(Some(scrubbed.payload(key)?))
        }
        _ => Ok(None),
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn fmt_time(t: OffsetDateTime) -> String {
    t.format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

fn parse_time(id: &str, s: &str) -> Result<OffsetDateTime, Error> {
    OffsetDateTime::parse(s, &Rfc3339).map_err(|_| Error::BadRow {
        id: id.to_string(),
        what: format!("time {s:?}"),
    })
}

fn label_from_rank(id: &str, rank: i64) -> Result<Label, Error> {
    match rank {
        0 => Ok(Label::Public),
        1 => Ok(Label::Internal),
        2 => Ok(Label::Restricted),
        other => Err(Error::BadRow {
            id: id.to_string(),
            what: format!("label rank {other}"),
        }),
    }
}

/// The key a promotion's events are appended under. A task id starts `task-`, so the two
/// never meet.
fn promotion_key(entry: &str) -> String {
    format!("promotion:{entry}")
}

/// The columns of a promotion row, in the order both backends read them.
const PROMOTION_COLUMNS: &str =
    "domain, entry, tier, promoter, author, why, label_rank, digest, new_digest, at, why_erased";

/// A promotion row as the database returns it, before the ledger has checked it.
struct RawPromotion {
    domain: String,
    entry: String,
    tier: String,
    promoter: String,
    author: String,
    why: String,
    label_rank: i64,
    digest: String,
    new_digest: String,
    at: String,
    why_erased: i64,
}

impl RawPromotion {
    fn into_record(self) -> Result<PromotionRecord, Error> {
        let key = promotion_key(&self.entry);
        let tier = self.tier.parse::<TrustTier>().map_err(|_| Error::BadRow {
            id: key.clone(),
            what: format!("tier {:?}", self.tier),
        })?;
        Ok(PromotionRecord {
            spec: PromotionSpec {
                domain: self.domain,
                entry: self.entry,
                tier,
                by: self.promoter,
                author: self.author,
                why: self.why,
                classification: label_from_rank(&key, self.label_rank)?,
                digest: self.digest,
                new_digest: self.new_digest,
            },
            at: parse_time(&key, &self.at)?,
            why_erased: self.why_erased != 0,
        })
    }
}

/// The columns of a task row, in the order both backends read them.
const TASK_COLUMNS: &str =
    "id, domain, title, body, requirements, depends_on, origin, upstream_event,
    created_by, label_rank, priority, created, status, holder, lease_until, attempts, handoff,
    reason, superseded_by, posted_to, extra";

/// A task row as the database returns it, before the ledger has checked it.
struct RawTask {
    id: String,
    domain: String,
    title: String,
    body: String,
    requirements: String,
    depends_on: String,
    origin: Option<String>,
    upstream_event: Option<String>,
    created_by: String,
    label_rank: i64,
    priority: i64,
    created: String,
    status: String,
    holder: Option<String>,
    lease_until: Option<String>,
    attempts: i64,
    handoff: Option<String>,
    reason: Option<String>,
    superseded_by: Option<String>,
    posted_to: Option<String>,
    extra: Option<String>,
}

impl RawTask {
    fn into_task(self) -> Result<Task, Error> {
        let id = self.id;
        let json_list = |s: &str, what: &str| -> Result<Vec<String>, Error> {
            serde_json::from_str(s).map_err(|_| Error::BadRow {
                id: id.clone(),
                what: format!("{what} {s:?}"),
            })
        };
        let requirements = json_list(&self.requirements, "requirements")?;
        let depends_on = json_list(&self.depends_on, "depends_on")?;
        // a row from before the column has none, and reads as a task with nothing more
        let extra: Extra = match self.extra.as_deref() {
            None | Some("") => Extra::default(),
            Some(text) => serde_json::from_str(text).map_err(|_| Error::BadRow {
                id: id.clone(),
                what: format!("extra {text:?}"),
            })?,
        };
        Ok(Task {
            spec: TaskSpec {
                domain: self.domain,
                title: self.title,
                body: self.body,
                requirements,
                depends_on,
                origin: self.origin,
                upstream_event: self.upstream_event,
                created_by: self.created_by,
                classification: label_from_rank(&id, self.label_rank)?,
                priority: self.priority,
                definition: extra.definition,
                acceptance: extra.acceptance,
            },
            created: parse_time(&id, &self.created)?,
            status: Status::parse(&id, &self.status)?,
            holder: self.holder,
            lease_until: self.lease_until.map(|s| parse_time(&id, &s)).transpose()?,
            // the column keeps the name it was made with; it counts claims
            generation: u64::try_from(self.attempts).unwrap_or(u64::MAX),
            handoff: self.handoff,
            reason: self.reason,
            superseded_by: self.superseded_by,
            posted_to: self.posted_to,
            id,
        })
    }
}

/// The `origin` a continuation of `parent` carries: the task that resumes it once the
/// subagent tasks its run filed are done.
#[must_use]
pub fn continues(parent: &str) -> String {
    format!("continues:{parent}")
}

/// Whether a session may claim `task` now: it is admitted, or claimed under a lease
/// that has run out; the session's manifest carries every requirement; and the
/// session's handling covers the task's classification. Whether its dependencies are
/// complete is the backend's question, asked inside the transaction.
fn claimable(task: &Task, now: OffsetDateTime, manifest: &[String], handling: Label) -> bool {
    let lease_live = task.lease_until.is_some_and(|t| t >= now);
    // a blocked task is a candidate too: the backend asks whether its dependencies are
    // done, so one whose last dependency closed without the event reaching it is not lost
    let state = matches!(task.status, Status::Open | Status::Blocked)
        || (task.status == Status::Claimed && !lease_live);
    state
        && task.spec.classification <= handling
        && task.spec.requirements.iter().all(|r| manifest.contains(r))
}

/// Whether `by` acts under the claim in force: its generation first, since a claim made
/// The queue's order: the highest priority first, and of equal priorities the task
/// filed first. The times are compared as times; as text, one with fewer digits of a
/// second sorts after a later one with more.
pub(crate) fn in_queue_order(tasks: &mut [Task]) {
    tasks.sort_by(|a, b| {
        b.spec
            .priority
            .cmp(&a.spec.priority)
            .then(a.created.cmp(&b.created))
            .then_with(|| a.id.cmp(&b.id))
    });
}

/// since is the reason to stop whoever the caller is, then its name and the lease.
pub(crate) fn check_holder(task: &Task, by: Held<'_>, now: OffsetDateTime) -> Result<(), Error> {
    if by.generation != task.generation {
        return Err(Error::Fenced {
            id: task.id.clone(),
            given: by.generation,
            current: task.generation,
        });
    }
    let held = task.holder.as_deref() == Some(by.holder);
    let live = task.lease_until.is_some_and(|t| t >= now);
    if held && live {
        Ok(())
    } else {
        Err(Error::NotHolder {
            id: task.id.clone(),
            holder: by.holder.to_string(),
        })
    }
}

/// Where the tables live.
enum Backend {
    Sqlite(sqlite::Db),
    Postgres(postgres::Db),
}

/// Run one backend call: the SQLite arm is synchronous, the PostgreSQL arm awaits.
macro_rules! on_backend {
    ($backend:expr, $db:ident => $call:expr) => {
        match $backend {
            Backend::Sqlite($db) => $call,
            Backend::Postgres($db) => $call.await,
        }
    };
}

/// The ledger, over one SQLite file or one PostgreSQL connection.
pub struct Ledger {
    backend: Backend,
    clock: Clock,
    /// Held while a thread of our own drives the connection; dropping it ends the thread.
    keepalive: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Ledger {
    /// Open or create the SQLite ledger file.
    ///
    /// # Errors
    ///
    /// When the file cannot be opened or the schema cannot be created.
    pub fn open(path: &Path) -> Result<Ledger, Error> {
        Ok(Ledger::new(Backend::Sqlite(sqlite::Db::open(path)?)))
    }

    /// A SQLite ledger that lives only in memory.
    ///
    /// # Errors
    ///
    /// When SQLite cannot create the schema, which it can always do in memory.
    pub fn in_memory() -> Result<Ledger, Error> {
        Ok(Ledger::new(Backend::Sqlite(sqlite::Db::in_memory()?)))
    }

    /// Connect to a PostgreSQL ledger at `url` (`postgres://user:pass@host:port/db`)
    /// and create its tables if they are not there. The connection is driven by a task
    /// spawned on the current runtime. TLS is not spoken: the server is on loopback or
    /// a private network, as the sidecar's would be.
    ///
    /// # Errors
    ///
    /// When the server cannot be reached, refuses the credentials, or the schema cannot
    /// be created.
    pub async fn connect(url: &str) -> Result<Ledger, Error> {
        Ok(Ledger::new(Backend::Postgres(
            postgres::Db::connect(url).await?,
        )))
    }

    /// [`Ledger::connect`] from synchronous code, for a caller that opens the ledger
    /// before it has a runtime of its own, or on one it cannot block. On a
    /// multi-threaded runtime the connection is driven there; otherwise a thread of
    /// its own drives it for as long as the ledger lives.
    ///
    /// # Errors
    ///
    /// As [`Ledger::connect`], or [`Error::Runtime`] when no runtime can be had.
    pub fn connect_blocking(url: &str) -> Result<Ledger, Error> {
        use tokio::runtime::{Handle, RuntimeFlavor};
        if let Ok(handle) = Handle::try_current() {
            if handle.runtime_flavor() == RuntimeFlavor::MultiThread {
                return tokio::task::block_in_place(|| handle.block_on(Ledger::connect(url)));
            }
        }
        let url = url.to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("ledger-postgres".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = tx.send(Err(Error::Runtime(e.to_string())));
                        return;
                    }
                };
                rt.block_on(async move {
                    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
                    let ledger = postgres::Db::connect(&url).await.map(|db| {
                        let mut l = Ledger::new(Backend::Postgres(db));
                        l.keepalive = Some(done_tx);
                        l
                    });
                    let _ = tx.send(ledger);
                    // the connection task lives on this runtime; stay until the
                    // ledger is dropped, which closes the other end
                    let _ = done_rx.await;
                });
            })
            .map_err(|e| Error::Runtime(e.to_string()))?;
        rx.recv()
            .unwrap_or_else(|_| Err(Error::Runtime("connection thread ended".into())))
    }

    fn new(backend: Backend) -> Ledger {
        Ledger {
            backend,
            clock: Box::new(OffsetDateTime::now_utc),
            keepalive: None,
        }
    }

    /// Replace the clock. Tests use this to let leases expire without waiting.
    pub fn set_clock(&mut self, clock: impl Fn() -> OffsetDateTime + Send + Sync + 'static) {
        self.clock = Box::new(clock);
    }

    fn now(&self) -> OffsetDateTime {
        (self.clock)()
    }

    /// File a task. It is born proposed. If `origin` and `upstream_event` are both set and
    /// the domain already has a task with that pair, that task is returned and nothing is
    /// filed; another domain's task with the same pair is not this domain's.
    ///
    /// # Errors
    ///
    /// A database error.
    pub async fn create(&mut self, spec: TaskSpec) -> Result<Created, Error> {
        if let (Some(o), Some(u)) = (&spec.origin, &spec.upstream_event) {
            let existing = on_backend!(&self.backend, db => db.find_origin(&spec.domain, o, u))?;
            if let Some(id) = existing {
                return Ok(Created {
                    task: self.get(&id).await?,
                    deduplicated: true,
                });
            }
        }
        let id = dm_core::id::new_task_id(&spec.domain);
        let now = self.now();
        let event = Event::Created {
            id: id.clone(),
            spec,
        };
        self.append_at(&id, now, &event).await?;
        Ok(Created {
            task: self.get(&id).await?,
            deduplicated: false,
        })
    }

    /// Admit a proposed task, making it claimable.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`], or [`Error::WrongState`] if it is not proposed.
    pub async fn admit(&mut self, id: &str) -> Result<Task, Error> {
        let task = self.get(id).await?;
        if task.status != Status::Proposed {
            return Err(Error::WrongState {
                id: id.to_string(),
                status: task.status,
                op: "admit",
            });
        }
        // a task that rests on work nobody will do has nothing to wait for: it stays
        // proposed, for whoever filed it to change or withdraw
        for d in &task.spec.depends_on {
            if matches!(self.get(d).await, Ok(t) if t.status == Status::Invalidated) {
                return Err(Error::WrongState {
                    id: id.to_string(),
                    status: task.status,
                    op: "admit while a task it depends on is invalidated",
                });
            }
        }
        self.append(id, &Event::Admitted).await?;
        let waits_on = self.not_done(&task.spec.depends_on).await?;
        if !waits_on.is_empty() {
            self.append(id, &Event::Blocked { on: waits_on }).await?;
        }
        self.get(id).await
    }

    /// Which of `ids` are not done.
    async fn not_done(&self, ids: &[String]) -> Result<Vec<String>, Error> {
        let mut open = Vec::new();
        for d in ids {
            // a dependency this ledger does not hold is not done as far as it can tell
            match self.get(d).await {
                Ok(t) if t.status == Status::Done => {}
                Ok(_) | Err(Error::NotFound(_)) => open.push(d.clone()),
                Err(e) => return Err(e),
            }
        }
        Ok(open)
    }

    /// `id` is done: open each blocked task that waited on it and now waits on nothing.
    async fn unblock_dependents(&mut self, id: &str) -> Result<(), Error> {
        for t in self.dependents(id).await? {
            if t.status == Status::Blocked && self.not_done(&t.spec.depends_on).await?.is_empty() {
                let by = id.to_string();
                self.append(&t.id, &Event::Unblocked { by }).await?;
            }
        }
        Ok(())
    }

    /// Claim the highest-ranked claimable task in a domain whose requirements the
    /// manifest satisfies, whose classification `handling` covers, and whose
    /// dependencies are complete. Claimable means admitted, or claimed under a lease
    /// that has expired. One transaction, so one holder; a task above `handling` is
    /// passed over inside it, so it is never claimed and then given back.
    ///
    /// # Errors
    ///
    /// A database error.
    pub async fn claim_next(
        &mut self,
        domain: &str,
        holder: &str,
        lease: Duration,
        manifest: &[String],
        handling: Label,
    ) -> Result<Option<Claim>, Error> {
        let now = self.now();
        let until = now + lease;
        let claimed = on_backend!(&mut self.backend, db => db.claim_next(domain, now, holder, until, manifest, handling))?;
        if let Some(id) = &claimed {
            self.trace("claimed", id).await;
        }
        match claimed {
            Some(id) => {
                let task = self.get(&id).await?;
                Ok(Some(Claim {
                    generation: task.generation,
                    task,
                    lease_until: until,
                }))
            }
            None => Ok(None),
        }
    }

    /// Claim one named task, under the same rules as [`Ledger::claim_next`].
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`]; [`Error::WrongState`] when it is not claimable, including when
    /// another holder's lease is still live, a dependency is open, or the task is above
    /// `handling`.
    pub async fn claim(
        &mut self,
        id: &str,
        holder: &str,
        lease: Duration,
        manifest: &[String],
        handling: Label,
    ) -> Result<Claim, Error> {
        let now = self.now();
        let until = now + lease;
        let claimed = on_backend!(&mut self.backend, db => db.claim(id, now, holder, until, manifest, handling));
        if matches!(claimed, Err(Error::WrongState { .. })) {
            self.record_refusal(id, holder, now).await?;
        }
        claimed?;
        self.trace("claimed", id).await;
        let task = self.get(id).await?;
        Ok(Claim {
            generation: task.generation,
            task,
            lease_until: until,
        })
    }

    /// A claim that lost is part of the task's record, while the task can still be
    /// claimed by someone. The reason names the claim it lost to, so the same holder
    /// refused twice under one claim is on the record once, and again under the next.
    async fn record_refusal(
        &mut self,
        id: &str,
        holder: &str,
        now: OffsetDateTime,
    ) -> Result<(), Error> {
        let task = self.get(id).await?;
        if task.status.is_terminal() {
            return Ok(());
        }
        let lease_live = task.lease_until.is_some_and(|t| t >= now);
        let why = if task.status == Status::Claimed && lease_live {
            format!(
                "the lease of the claim of generation {} was live",
                task.generation
            )
        } else if task.status == Status::Proposed {
            "the task was not admitted".to_string()
        } else {
            // claimable by someone, and still not this session's to take
            format!(
                "after the claim of generation {}, the session did not meet the task's requirements or its handling, or a task it depends on was not done",
                task.generation
            )
        };
        let last = self.refused_claims(id).await?.pop();
        if last.as_ref() == Some(&(holder.to_string(), why.clone())) {
            return Ok(());
        }
        let refused = Event::ClaimRefused {
            holder: holder.to_string(),
            why,
        };
        self.append_at(id, now, &refused).await
    }

    /// Renew a lease. Refused when the caller does not hold the task or the lease has
    /// already expired, in which case the task is back in the pool and the caller must
    /// stop. Returns the task so a caller can see an invalidation.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`], [`Error::NotHolder`], or [`Error::WrongState`] when terminal.
    pub async fn heartbeat(
        &mut self,
        id: &str,
        by: Held<'_>,
        lease: Duration,
    ) -> Result<Task, Error> {
        let now = self.now();
        let task = self.get(id).await?;
        if task.status.is_terminal() {
            return Err(Error::WrongState {
                id: id.to_string(),
                status: task.status,
                op: "heartbeat",
            });
        }
        let renewed = Event::Heartbeat { until: now + lease };
        self.append_held(id, now, by, &renewed).await?;
        self.get(id).await
    }

    /// Close a task with the handoff entry that records what happened.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`], [`Error::NotHolder`], or [`Error::WrongState`].
    pub async fn complete(&mut self, id: &str, by: Held<'_>, handoff: &str) -> Result<Task, Error> {
        let now = self.now();
        let task = self.get(id).await?;
        if task.status != Status::Claimed {
            return Err(Error::WrongState {
                id: id.to_string(),
                status: task.status,
                op: "complete",
            });
        }
        let completed = Event::Completed {
            handoff: handoff.to_string(),
        };
        self.append_held(id, now, by, &completed).await?;
        self.unblock_dependents(id).await?;
        self.get(id).await
    }

    /// Close a task its continuation finished: the continuation `by` resumed it and is
    /// complete with `handoff`, so the task is too, with the same handoff. Only a task
    /// nobody holds: one released when its run delegated, or whose lease ran out. A
    /// holder with a live lease finishes it themselves.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`]; [`Error::WrongState`] when it is terminal, held under a live
    /// lease, or `by` is not a completed continuation of it.
    pub async fn complete_by_continuation(
        &mut self,
        id: &str,
        by: &str,
        handoff: &str,
    ) -> Result<Task, Error> {
        let now = self.now();
        let task = self.get(id).await?;
        let continuation = self.get(by).await?;
        let held = task.status == Status::Claimed && task.lease_until.is_some_and(|t| t >= now);
        let resumed_it = continuation.spec.origin.as_deref() == Some(continues(id).as_str())
            && continuation.status == Status::Done;
        if task.status.is_terminal() || held || !resumed_it {
            return Err(Error::WrongState {
                id: id.to_string(),
                status: task.status,
                op: "complete by continuation",
            });
        }
        self.append_at(
            id,
            now,
            &Event::Completed {
                handoff: handoff.to_string(),
            },
        )
        .await?;
        self.unblock_dependents(id).await?;
        self.get(id).await
    }

    /// Give a task back to the pool.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`], [`Error::NotHolder`], or [`Error::WrongState`].
    pub async fn release(
        &mut self,
        id: &str,
        by: Held<'_>,
        handoff: Option<&str>,
    ) -> Result<Task, Error> {
        let now = self.now();
        let task = self.get(id).await?;
        if task.status != Status::Claimed {
            return Err(Error::WrongState {
                id: id.to_string(),
                status: task.status,
                op: "release",
            });
        }
        if let Some(handoff) = handoff {
            let left = Event::HandoffLeft {
                handoff: handoff.to_string(),
            };
            self.append_held(id, now, by, &left).await?;
        }
        self.append_held(id, now, by, &Event::Released).await?;
        self.get(id).await
    }

    /// Leave a handoff on a task after the claim it was written under is over: the
    /// lease ran out, the task may be held again, and what the earlier holder learned is
    /// still worth the next one's reading. It changes nothing of the task. `by` names the
    /// earlier claim, and the ledger's own record of that claim must agree.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`]; [`Error::NotHolder`] when the ledger never gave `by.holder`
    /// the claim of that generation; [`Error::WrongState`] when that claim is still in
    /// force, since its holder completes or releases instead.
    pub async fn handoff_after(
        &mut self,
        id: &str,
        by: Held<'_>,
        handoff: &str,
    ) -> Result<Task, Error> {
        let now = self.now();
        let task = self.get(id).await?;
        let in_force = task.status == Status::Claimed
            && task.generation == by.generation
            && task.holder.as_deref() == Some(by.holder)
            && task.lease_until.is_some_and(|t| t >= now);
        if in_force || task.status.is_terminal() {
            return Err(Error::WrongState {
                id: id.to_string(),
                status: task.status,
                op: "leave a late handoff: the claim is in force, or the task is closed",
            });
        }
        let was = on_backend!(&self.backend, db => db.claimant(id, by.generation))?;
        if was.as_deref() != Some(by.holder) {
            return Err(Error::NotHolder {
                id: id.to_string(),
                holder: by.holder.to_string(),
            });
        }
        let said = LateHandoff {
            holder: by.holder.to_string(),
            generation: by.generation,
            handoff: handoff.to_string(),
        };
        // the same handoff left twice is on the record once
        if self.late_handoffs(id).await?.contains(&said) {
            return Ok(task);
        }
        let late = Event::LateHandoff {
            holder: said.holder,
            generation: said.generation,
            handoff: said.handoff,
        };
        self.append_at(id, now, &late).await?;
        self.get(id).await
    }

    /// The handoffs left on a task after the claims they were written under, oldest first.
    ///
    /// # Errors
    ///
    /// A database error, or an event the ledger did not write.
    pub async fn late_handoffs(&self, id: &str) -> Result<Vec<LateHandoff>, Error> {
        let payloads = on_backend!(&self.backend, db => db.payloads_of(id, "late_handoff"))?;
        let mut out = Vec::new();
        for p in payloads {
            if let Event::LateHandoff {
                holder,
                generation,
                handoff,
            } = Event::parse(id, &p)?
            {
                out.push(LateHandoff {
                    holder,
                    generation,
                    handoff,
                });
            }
        }
        Ok(out)
    }

    /// The claims of a named task that the ledger refused: who asked, and why not.
    ///
    /// # Errors
    ///
    /// A database error, or an event the ledger did not write.
    pub async fn refused_claims(&self, id: &str) -> Result<Vec<(String, String)>, Error> {
        let payloads = on_backend!(&self.backend, db => db.payloads_of(id, "claim_refused"))?;
        let mut out = Vec::new();
        for p in payloads {
            if let Event::ClaimRefused { holder, why } = Event::parse(id, &p)? {
                out.push((holder, why));
            }
        }
        Ok(out)
    }

    /// Close a task without doing it. A live holder learns of it at the next heartbeat.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`], or [`Error::WrongState`] when already terminal.
    pub async fn invalidate(
        &mut self,
        id: &str,
        reason: &str,
        superseded_by: Option<&str>,
    ) -> Result<Task, Error> {
        let task = self.get(id).await?;
        if task.status.is_terminal() {
            return Err(Error::WrongState {
                id: id.to_string(),
                status: task.status,
                op: "invalidate",
            });
        }
        self.append(
            id,
            &Event::Invalidated {
                reason: reason.to_string(),
                superseded_by: superseded_by.map(str::to_string),
            },
        )
        .await?;
        // what rested on it goes back to triage, unless someone holds it: a holder learns
        // at its next heartbeat that the task it works on changed under it
        self.retriage_dependents(id).await?;
        self.get(id).await
    }

    /// `id` was invalidated: each task that waits on it goes back to triage. That is a
    /// blocked task, and an open one too: a ledger from before tasks were blocked holds
    /// its waiting tasks as open. A task that is held is its holder's.
    async fn retriage_dependents(&mut self, id: &str) -> Result<(), Error> {
        for t in self.dependents(id).await? {
            if matches!(t.status, Status::Blocked | Status::Open) {
                let because = id.to_string();
                self.append(&t.id, &Event::Retriaged { because }).await?;
            }
        }
        Ok(())
    }

    /// One task.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`], or a row the ledger did not write.
    pub async fn get(&self, id: &str) -> Result<Task, Error> {
        on_backend!(&self.backend, db => db.get(id))
    }

    /// The domain's task for an `origin` and `upstream_event` pair, in any status: the
    /// one [`Ledger::create`] returns in place of filing the same pair again.
    ///
    /// # Errors
    ///
    /// A database error, or a row the ledger did not write.
    pub async fn by_origin(
        &self,
        domain: &str,
        origin: &str,
        upstream_event: &str,
    ) -> Result<Option<Task>, Error> {
        let found =
            on_backend!(&self.backend, db => db.find_origin(domain, origin, upstream_event))?;
        match found {
            Some(id) => Ok(Some(self.get(&id).await?)),
            None => Ok(None),
        }
    }

    /// The domain's tasks with an `origin`, of every upstream event and status, oldest
    /// first.
    ///
    /// # Errors
    ///
    /// A database error, or a row the ledger did not write.
    pub async fn with_origin(&self, domain: &str, origin: &str) -> Result<Vec<Task>, Error> {
        on_backend!(&self.backend, db => db.with_origin(domain, origin))
    }

    /// The tasks filed from `parent`: the subagent tasks a run of it asked for
    /// (`origin` `task:<parent>`), its continuations (`continues:<parent>`) and its
    /// retries (`retries:<parent>`), oldest first.
    ///
    /// # Errors
    ///
    /// A database error, or a row the ledger did not write.
    pub async fn filed_from(&self, parent: &str) -> Result<Vec<Task>, Error> {
        on_backend!(&self.backend, db => db.filed_from(parent))
    }

    /// Tasks in a domain with the given status, ranked as the queue ranks them.
    ///
    /// # Errors
    ///
    /// A database error, or a row the ledger did not write.
    pub async fn list(&self, domain: &str, status: Status) -> Result<Vec<Task>, Error> {
        on_backend!(&self.backend, db => db.list(domain, status))
    }

    /// Record that a completed task's handoff was posted back where the task came from,
    /// at `to`, so it is not posted twice.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`], or [`Error::WrongState`] when the task is not completed or
    /// was posted already.
    pub async fn post(&mut self, id: &str, to: &str) -> Result<Task, Error> {
        let task = self.get(id).await?;
        if task.status != Status::Done || task.posted_to.is_some() {
            return Err(Error::WrongState {
                id: id.to_string(),
                status: task.status,
                op: "post",
            });
        }
        self.append(id, &Event::Posted { to: to.to_string() })
            .await?;
        self.get(id).await
    }

    /// Rank an open task again: its place in the queue changes, the priority it was filed
    /// with does not, and a claimed task keeps its lease whatever the rubric says. No
    /// event when the rank is what it was.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`], or [`Error::WrongState`] when it is terminal.
    pub async fn rescore(&mut self, id: &str, priority: i64, why: &str) -> Result<Task, Error> {
        let task = self.get(id).await?;
        if task.status.is_terminal() {
            return Err(Error::WrongState {
                id: id.to_string(),
                status: task.status,
                op: "rescore",
            });
        }
        if task.spec.priority == priority {
            return Ok(task);
        }
        self.append(
            id,
            &Event::Rescored {
                priority,
                why: why.to_string(),
            },
        )
        .await?;
        self.get(id).await
    }

    /// The priority a task was filed with, before any rescore: from its `Created` event.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`], or a payload the ledger did not write.
    pub async fn filed_priority(&self, id: &str) -> Result<i64, Error> {
        let payload = on_backend!(&self.backend, db => db.created_payload(id))?;
        match Event::parse(id, &payload)? {
            Event::Created { spec, .. } => Ok(spec.priority),
            _ => Err(Error::BadRow {
                id: id.to_string(),
                what: "first event is not its creation".into(),
            }),
        }
    }

    /// Completed tasks in a domain that came from upstream and whose handoff has not
    /// been posted back, oldest first.
    ///
    /// # Errors
    ///
    /// A database error, or a row the ledger did not write.
    pub async fn unposted(&self, domain: &str) -> Result<Vec<Task>, Error> {
        on_backend!(&self.backend, db => db.unposted(domain))
    }

    /// Tasks that depend on this one.
    ///
    /// # Errors
    ///
    /// A database error, or a row the ledger did not write.
    pub async fn dependents(&self, id: &str) -> Result<Vec<Task>, Error> {
        on_backend!(&self.backend, db => db.dependents(id))
    }

    /// Record a promotion: one event, projected into the promotions and nowhere else.
    /// Once per promoter and version: a second record of the same entry, tier, digest
    /// and promoter appends nothing and returns false. Another version, or another
    /// promoter of the same version, is a record of its own, so a record whose promotion
    /// failed never stands in for a later one that did not.
    ///
    /// # Errors
    ///
    /// A database error.
    pub async fn record_promotion(&mut self, spec: &PromotionSpec) -> Result<bool, Error> {
        let now = self.now();
        on_backend!(&mut self.backend, db => db.record_promotion(now, spec, false))
    }

    /// Record a promotion another ledger kept, as it was there: its time, and whether its
    /// why was erased. An import of a bundle the same owner exported calls this, after
    /// moving the record to the entry as it landed here. Once, as
    /// [`Ledger::record_promotion`] is.
    ///
    /// # Errors
    ///
    /// A database error.
    pub async fn restore_promotion(&mut self, record: &PromotionRecord) -> Result<bool, Error> {
        on_backend!(&mut self.backend, db => db.record_promotion(record.at, &record.spec, record.why_erased))
    }

    /// The promotions of an entry in a domain, oldest first.
    ///
    /// # Errors
    ///
    /// A database error, or a row the ledger did not write.
    pub async fn promotions(
        &self,
        domain: &str,
        entry: &str,
    ) -> Result<Vec<PromotionRecord>, Error> {
        on_backend!(&self.backend, db => db.promotions(domain, entry))
    }

    /// Remove the why from every promotion record of an entry, for its erasure: the
    /// events that hold it are rewritten and the projection with them, so no replay
    /// brings it back. Who promoted which version to which tier, and when, is kept.
    /// Running it again changes nothing.
    ///
    /// # Errors
    ///
    /// A database error, or [`Error::Held`] when another connection holds the log the
    /// old text is in.
    pub async fn scrub_promotions(&mut self, domain: &str, entry: &str) -> Result<Scrubbed, Error> {
        let records = on_backend!(&mut self.backend, db => db.scrub_promotions(domain, entry))?;
        Ok(Scrubbed {
            records,
            tasks: 0,
            beyond_reach: self.beyond_reach(),
        })
    }

    /// Rewrite the tasks filed from `origin` in `domain` for an erasure of what they are
    /// about: each gets `redaction`'s title and body in its `created` event and in the
    /// projection, its reason for being invalidated and the handoffs left on it or that
    /// closed it become the redaction's reason, so no replay brings the old text back;
    /// each still open is invalidated. The task's id, origin, who filed it and its history
    /// stay. Returns how many tasks from that origin now hold the redaction. Running it
    /// again changes nothing.
    ///
    /// # Errors
    ///
    /// A database error, or [`Error::Held`] when another connection holds the log the
    /// old text is in.
    pub async fn scrub_tasks(
        &mut self,
        domain: &str,
        origin: &str,
        redaction: &Redaction,
    ) -> Result<usize, Error> {
        let now = self.now();
        let n =
            on_backend!(&mut self.backend, db => db.scrub_tasks(now, domain, origin, redaction))?;
        self.retriage_after_scrub(domain, origin).await?;
        Ok(n)
    }

    /// A scrub invalidates the open tasks filed from an origin: what was blocked on one
    /// of them goes back to triage, as after any invalidation.
    async fn retriage_after_scrub(&mut self, domain: &str, origin: &str) -> Result<(), Error> {
        for t in self.with_origin(domain, origin).await? {
            if t.status == Status::Invalidated {
                self.retriage_dependents(&t.id).await?;
            }
        }
        Ok(())
    }

    /// Both scrubs of an erasure, [`Ledger::scrub_promotions`] of `entry` and
    /// [`Ledger::scrub_tasks`] of `origin`, in one transaction, and on SQLite one rebuild
    /// of the file after them, also when nothing was left to rewrite: an earlier run that
    /// a reader held up left the old text in the write-ahead log.
    ///
    /// # Errors
    ///
    /// A database error, or [`Error::Held`] when another connection holds the log the
    /// old text is in.
    pub async fn scrub_entry(
        &mut self,
        domain: &str,
        entry: &str,
        origin: &str,
        redaction: &Redaction,
    ) -> Result<Scrubbed, Error> {
        let now = self.now();
        let (records, tasks) = on_backend!(
            &mut self.backend,
            db => db.scrub_entry(now, domain, entry, origin, redaction)
        )?;
        self.retriage_after_scrub(domain, origin).await?;
        Ok(Scrubbed {
            records,
            tasks,
            beyond_reach: self.beyond_reach(),
        })
    }

    /// What a scrub on this backend cannot remove.
    fn beyond_reach(&self) -> Vec<String> {
        match self.backend {
            Backend::Sqlite(_) => Vec::new(),
            Backend::Postgres(_) => vec![POSTGRES_RESIDUE.to_string()],
        }
    }

    /// How many events the ledger holds.
    ///
    /// # Errors
    ///
    /// A database error.
    pub async fn event_count(&self) -> Result<u64, Error> {
        on_backend!(&self.backend, db => db.event_count())
    }

    /// Drop the projection and replay every event into a fresh one. Returns the number
    /// of events replayed.
    ///
    /// # Errors
    ///
    /// A database error; the projection is left as it was.
    pub async fn rebuild(&mut self) -> Result<u64, Error> {
        on_backend!(&mut self.backend, db => db.rebuild())
    }

    async fn append(&mut self, id: &str, event: &Event) -> Result<(), Error> {
        let now = self.now();
        self.append_at(id, now, event).await
    }

    /// Append an event that only the holder under the claim in force may cause. The
    /// backend reads the task and appends in one transaction, so a claim made between
    /// the two cannot slip in.
    async fn append_held(
        &mut self,
        id: &str,
        at: OffsetDateTime,
        by: Held<'_>,
        event: &Event,
    ) -> Result<(), Error> {
        on_backend!(&mut self.backend, db => db.append_held(id, at, by, event))?;
        self.trace(event.kind(), id).await;
        Ok(())
    }

    /// Append one event and project it, in one transaction.
    async fn append_at(
        &mut self,
        id: &str,
        at: OffsetDateTime,
        event: &Event,
    ) -> Result<(), Error> {
        on_backend!(&mut self.backend, db => db.append(id, at, event))?;
        let task_id = match event {
            Event::Created { id: new_id, .. } => new_id.as_str(),
            _ => id,
        };
        self.trace(event.kind(), task_id).await;
        Ok(())
    }

    /// A `task.<kind>` span for a transition that took effect, with the task as it now
    /// stands. Only when a collector is configured, since it reads the task back.
    async fn trace(&self, kind: &str, id: &str) {
        if !dm_otel::enabled() {
            return;
        }
        let Ok(t) = self.get(id).await else {
            return;
        };
        let mut attrs: Vec<(&'static str, dm_otel::Value<'_>)> = vec![
            ("task.id", t.id.as_str().into()),
            ("memory.domain", t.spec.domain.as_str().into()),
            ("task.status", t.status.as_str().into()),
            ("task.priority", t.spec.priority.into()),
            (
                "task.generation",
                i64::try_from(t.generation).unwrap_or(i64::MAX).into(),
            ),
            ("task.created_by", t.spec.created_by.as_str().into()),
        ];
        if let Some(h) = &t.holder {
            attrs.push(("task.holder", h.as_str().into()));
        }
        if let Some(o) = &t.spec.origin {
            attrs.push(("task.origin", o.as_str().into()));
        }
        if let Some(r) = &t.reason {
            attrs.push(("task.reason", r.as_str().into()));
        }
        dm_otel::event(&format!("task.{kind}"), &attrs);
    }
}

#[cfg(test)]
mod tests {
    use super::testing::PgServer;
    use super::*;
    use std::sync::{Arc, Mutex};
    use time::macros::datetime;

    const T0: OffsetDateTime = datetime!(2026-10-01 10:00 UTC);

    /// Where a test's ledger lives, so it can be opened again.
    enum Site {
        Sqlite(tempfile::TempDir),
        Postgres(Box<PgServer>),
    }

    impl Site {
        async fn open(&self) -> Ledger {
            match self {
                Site::Sqlite(dir) => Ledger::open(&dir.path().join("ledger.db")).unwrap(),
                Site::Postgres(server) => Ledger::connect(server.url()).await.unwrap(),
            }
        }

        /// Every event's payload, as the log holds it.
        async fn payloads(&self) -> Vec<String> {
            match self {
                Site::Sqlite(dir) => {
                    let conn = rusqlite::Connection::open(dir.path().join("ledger.db")).unwrap();
                    let mut stmt = conn.prepare("SELECT payload FROM events").unwrap();
                    let rows = stmt.query_map([], |r| r.get(0)).unwrap();
                    rows.map(Result::unwrap).collect()
                }
                Site::Postgres(server) => {
                    let c = pg_client(server).await;
                    let mut out = Vec::new();
                    for row in c
                        .query("SELECT schema FROM public.ledger_domains", &[])
                        .await
                        .unwrap()
                    {
                        let s: String = row.get(0);
                        for p in c
                            .query(&format!("SELECT payload FROM {s}.events"), &[])
                            .await
                            .unwrap()
                        {
                            out.push(p.get(0));
                        }
                    }
                    out
                }
            }
        }

        /// A ledger on a clock the test moves.
        async fn ledger(&self) -> (Ledger, Arc<Mutex<OffsetDateTime>>) {
            let now = Arc::new(Mutex::new(T0));
            let mut l = self.open().await;
            let c = now.clone();
            l.set_clock(move || *c.lock().unwrap());
            (l, now)
        }
    }

    /// Each test body once per backend, as `sqlite::<name>` and `postgres::<name>`; the
    /// PostgreSQL one is skipped, saying so, where no server can be had.
    macro_rules! on_both {
        ($($name:ident),* $(,)?) => {
            mod sqlite {
                $(
                    #[tokio::test]
                    async fn $name() {
                        let b = super::Site::Sqlite(tempfile::tempdir().unwrap());
                        super::$name(&b).await;
                    }
                )*
            }
            mod postgres {
                $(
                    #[tokio::test]
                    async fn $name() {
                        let Some(server) = super::PgServer::start().await.unwrap() else {
                            return;
                        };
                        super::$name(&super::Site::Postgres(Box::new(server))).await;
                    }
                )*
            }
        };
    }

    on_both! {
        of_two_tasks_of_one_priority_the_one_filed_first_is_claimed_first,
        a_lease_that_ran_out_half_a_second_ago_is_lost_to_claim_next,
        a_dependent_in_another_domain_is_opened_and_retriaged_too,
        a_blocked_task_whose_dependencies_are_done_is_claimable_whatever_its_state_says,
        a_task_closed_by_its_continuation_opens_what_waited_on_it,
        a_late_handoff_needs_a_lost_lease_and_an_open_task_and_is_kept_once,
        a_refused_claim_is_recorded_once_and_not_on_a_closed_task,
        an_erasure_takes_a_late_handoff_too,
        a_task_is_not_admitted_onto_an_invalidated_dependency,
        an_open_task_of_an_older_ledger_is_retriaged_with_its_dependency,
        an_erasure_that_invalidates_a_task_sends_back_what_it_blocked,
        a_projection_with_a_state_name_of_an_older_ledger_is_rebuilt_at_open,
        a_claim_made_since_fences_the_holder_of_an_earlier_one_whatever_its_name,
        a_handoff_is_left_after_a_claim_only_by_who_held_it,
        a_refused_claim_of_a_named_task_is_on_its_record,
        the_last_dependency_done_opens_a_blocked_task,
        a_task_is_blocked_until_its_dependencies_are_done_and_retriaged_when_one_is_invalidated,
        a_task_is_proposed_until_admitted_and_then_claimable_once,
        heartbeat_renews_only_for_the_holder_and_complete_closes,
        an_unrenewed_lease_is_lost_at_expiry_for_both_sides,
        requirements_match_the_manifest_and_priority_orders_the_queue,
        a_task_goes_only_to_a_session_whose_handling_covers_it,
        dependencies_block_until_complete,
        release_returns_to_pool_and_invalidate_is_terminal,
        origin_and_upstream_event_dedup_a_second_filing,
        what_was_filed_from_a_task_is_found_by_its_origin,
        every_operation_appends_one_event_and_replay_rebuilds_the_projection,
        not_found_and_persistence,
        a_completed_task_from_upstream_is_posted_back_once,
        two_claimers_on_two_connections_get_two_tasks,
        a_domain_claims_only_its_own_queue,
        a_rescore_moves_the_queue_and_keeps_the_filed_priority,
        a_task_waits_on_its_continuation,
        a_task_closes_with_its_continuation,
        a_continuation_given_up_on_gives_its_task_back,
        a_promotion_is_recorded_once_per_version,
        a_promotion_record_is_no_task,
        replay_keeps_the_promotions,
        a_second_promoter_of_the_same_version_is_recorded,
        an_erasure_takes_the_why_and_a_replay_keeps_it_out,
        a_restored_record_keeps_its_time_and_its_erased_why,
        an_erasure_rewrites_the_tasks_filed_from_an_origin,
        an_erasure_takes_the_reason_and_the_handoff_of_the_tasks_too,
    }

    fn spec(title: &str) -> TaskSpec {
        TaskSpec {
            domain: "dom-a".into(),
            title: title.into(),
            body: "Do it.\n".into(),
            requirements: vec![],
            depends_on: vec![],
            origin: None,
            upstream_event: None,
            created_by: "human:test:me".into(),
            classification: Label::Public,
            priority: 0,
            ..TaskSpec::default()
        }
    }

    async fn admitted(l: &mut Ledger, s: TaskSpec) -> Task {
        let t = l.create(s).await.unwrap().task;
        l.admit(&t.id).await.unwrap()
    }

    impl Ledger {
        /// A heartbeat by `holder` under the claim in force.
        async fn heartbeat_as(
            &mut self,
            id: &str,
            holder: &str,
            lease: Duration,
        ) -> Result<Task, Error> {
            let by = self.by(id, holder).await;
            self.heartbeat(id, by, lease).await
        }

        /// A completion by `holder` under the claim in force.
        async fn complete_as(
            &mut self,
            id: &str,
            holder: &str,
            handoff: &str,
        ) -> Result<Task, Error> {
            let by = self.by(id, holder).await;
            self.complete(id, by, handoff).await
        }

        /// A release by `holder` under the claim in force.
        async fn release_as(
            &mut self,
            id: &str,
            holder: &str,
            handoff: Option<&str>,
        ) -> Result<Task, Error> {
            let by = self.by(id, holder).await;
            self.release(id, by, handoff).await
        }

        /// `holder` under the claim in force, for a test that is about something else.
        async fn by<'a>(&self, id: &str, holder: &'a str) -> Held<'a> {
            Held {
                holder,
                generation: self.get(id).await.unwrap().generation,
            }
        }
    }

    fn mins(n: i64) -> Duration {
        Duration::minutes(n)
    }

    /// `claim_next` in `dom-a` with an empty manifest and the highest handling.
    async fn next(l: &mut Ledger, holder: &str, lease: Duration) -> Option<Claim> {
        l.claim_next("dom-a", holder, lease, &[], Label::Restricted)
            .await
            .unwrap()
    }

    /// `claim` with an empty manifest and the highest handling.
    async fn claim(
        l: &mut Ledger,
        id: &str,
        holder: &str,
        lease: Duration,
    ) -> Result<Claim, Error> {
        l.claim(id, holder, lease, &[], Label::Restricted).await
    }

    fn is_wrong_state<T>(r: &Result<T, Error>) -> bool {
        matches!(r, Err(Error::WrongState { .. }))
    }

    fn is_not_holder<T>(r: &Result<T, Error>) -> bool {
        matches!(r, Err(Error::NotHolder { .. }))
    }

    async fn get_all(l: &Ledger, ids: &[&str]) -> Vec<Task> {
        let mut out = Vec::new();
        for id in ids {
            out.push(l.get(id).await.unwrap());
        }
        out
    }

    async fn a_task_is_proposed_until_admitted_and_then_claimable_once(b: &Site) {
        let (mut l, _) = b.ledger().await;
        let t = l.create(spec("Fix the flaky test")).await.unwrap().task;
        assert_eq!(t.status, Status::Proposed);
        assert!(next(&mut l, "s1", mins(30)).await.is_none());
        l.admit(&t.id).await.unwrap();
        assert!(is_wrong_state(&l.admit(&t.id).await));
        claimable_once(&mut l, &t).await;
    }

    async fn claimable_once(l: &mut Ledger, t: &Task) {
        let c = next(l, "s1", mins(30)).await.unwrap();
        assert_eq!(c.task.id, t.id);
        assert_eq!(c.task.status, Status::Claimed);
        assert_eq!(c.task.holder.as_deref(), Some("s1"));
        assert_eq!(c.lease_until, T0 + mins(30));
        assert_eq!(c.task.generation, 1);
        assert!(next(l, "s2", mins(30)).await.is_none(), "one holder");
        assert!(is_wrong_state(&claim(l, &t.id, "s2", mins(30)).await));
    }

    async fn heartbeat_renews_only_for_the_holder_and_complete_closes(b: &Site) {
        let (mut l, now) = b.ledger().await;
        let t = admitted(&mut l, spec("Work")).await;
        claim(&mut l, &t.id, "s1", mins(30)).await.unwrap();
        *now.lock().unwrap() = T0 + mins(20);
        let t = l.heartbeat_as(&t.id, "s1", mins(30)).await.unwrap();
        assert_eq!(t.lease_until, Some(T0 + mins(50)));
        assert!(is_not_holder(&l.heartbeat_as(&t.id, "s2", mins(30)).await));
        assert!(is_not_holder(&l.complete_as(&t.id, "s2", "mem-a-h").await));
        complete_closes(&mut l, &t).await;
    }

    async fn complete_closes(l: &mut Ledger, t: &Task) {
        let done = l.complete_as(&t.id, "s1", "mem-a-handoff").await.unwrap();
        assert_eq!(done.status, Status::Done);
        assert_eq!(done.handoff.as_deref(), Some("mem-a-handoff"));
        assert!(done.lease_until.is_none());
        assert!(is_wrong_state(&l.heartbeat_as(&t.id, "s1", mins(30)).await));
        assert!(is_wrong_state(&l.complete_as(&t.id, "s1", "again").await));
        assert_eq!(l.list("dom-a", Status::Done).await.unwrap().len(), 1);
    }

    fn in_domain(domain: &str, title: &str, after: &[&Task]) -> TaskSpec {
        TaskSpec {
            domain: domain.into(),
            depends_on: after.iter().map(|t| t.id.clone()).collect(),
            ..spec(title)
        }
    }

    async fn a_dependent_in_another_domain_is_opened_and_retriaged_too(b: &Site) {
        let (mut l, _) = b.ledger().await;
        let done = admitted(&mut l, in_domain("dom-a", "Will be done", &[])).await;
        let dropped = admitted(&mut l, in_domain("dom-a", "Will be dropped", &[])).await;
        let opens = admitted(&mut l, in_domain("dom-b", "Waits across", &[&done])).await;
        let returns = admitted(&mut l, in_domain("dom-b", "Rests across", &[&dropped])).await;
        assert_eq!(
            (opens.status, returns.status),
            (Status::Blocked, Status::Blocked)
        );
        claim(&mut l, &done.id, "s", mins(5)).await.unwrap();
        l.complete(&done.id, held("s", 1), "mem-a-1").await.unwrap();
        assert_eq!(l.get(&opens.id).await.unwrap().status, Status::Open);
        l.invalidate(&dropped.id, "obsolete", None).await.unwrap();
        assert_eq!(l.get(&returns.id).await.unwrap().status, Status::Proposed);
    }

    /// The state is what the events said; the claim asks the dependencies themselves, so
    /// a task left blocked after its last dependency closed is not lost.
    async fn a_blocked_task_whose_dependencies_are_done_is_claimable_whatever_its_state_says(
        b: &Site,
    ) {
        let (mut l, _) = b.ledger().await;
        let first = admitted(&mut l, spec("First")).await;
        let waits = admitted(&mut l, in_domain("dom-a", "Waits", &[&first])).await;
        claim(&mut l, &first.id, "s", mins(5)).await.unwrap();
        l.complete(&first.id, held("s", 1), "mem-a-1")
            .await
            .unwrap();
        // as if the event that opens it had not reached it
        l.append(&waits.id, &Event::Blocked { on: Vec::new() })
            .await
            .unwrap();
        assert_eq!(l.get(&waits.id).await.unwrap().status, Status::Blocked);
        let c = next(&mut l, "s", mins(5)).await.unwrap();
        assert_eq!(c.task.id, waits.id);
        // and the events that open and send back move only a task that waits
        l.append(
            &waits.id,
            &Event::Retriaged {
                because: first.id.clone(),
            },
        )
        .await
        .unwrap();
        l.append(&waits.id, &Event::Blocked { on: Vec::new() })
            .await
            .unwrap();
        assert_eq!(l.get(&waits.id).await.unwrap().status, Status::Claimed);
    }

    async fn a_task_closed_by_its_continuation_opens_what_waited_on_it(b: &Site) {
        let (mut l, _) = b.ledger().await;
        let (parent, child, cont) = delegated(&mut l).await;
        let waits = admitted(&mut l, in_domain("dom-a", "After the parent", &[&parent])).await;
        assert_eq!(waits.status, Status::Blocked);
        claim(&mut l, &child.id, "s2", mins(5)).await.unwrap();
        l.complete_as(&child.id, "s2", "mem-a-child").await.unwrap();
        claim(&mut l, &cont.id, "s3", mins(5)).await.unwrap();
        l.complete_as(&cont.id, "s3", "mem-a-done").await.unwrap();
        l.complete_by_continuation(&parent.id, &cont.id, "mem-a-done")
            .await
            .unwrap();
        assert_eq!(l.get(&waits.id).await.unwrap().status, Status::Open);
    }

    async fn a_late_handoff_needs_a_lost_lease_and_an_open_task_and_is_kept_once(b: &Site) {
        let (mut l, now) = b.ledger().await;
        let t = admitted(&mut l, spec("Long job")).await;
        claim(&mut l, &t.id, "s1", mins(30)).await.unwrap();
        *now.lock().unwrap() = T0 + mins(31);
        // the lease ran out and nobody has claimed since: the claim is over
        l.handoff_after(&t.id, held("s1", 1), "mem-a-late")
            .await
            .unwrap();
        l.handoff_after(&t.id, held("s1", 1), "mem-a-late")
            .await
            .unwrap();
        assert_eq!(l.late_handoffs(&t.id).await.unwrap().len(), 1, "kept once");
        claim(&mut l, &t.id, "s2", mins(30)).await.unwrap();
        l.complete(&t.id, held("s2", 2), "mem-a-h").await.unwrap();
        // a closed task has no next holder to read it
        assert!(is_wrong_state(
            &l.handoff_after(&t.id, held("s1", 1), "mem-a-more").await
        ));
    }

    async fn a_refused_claim_is_recorded_once_and_not_on_a_closed_task(b: &Site) {
        let (mut l, _) = b.ledger().await;
        let t = admitted(&mut l, spec("One holder")).await;
        claim(&mut l, &t.id, "s1", mins(30)).await.unwrap();
        for _ in 0..3 {
            assert!(is_wrong_state(&claim(&mut l, &t.id, "s2", mins(30)).await));
        }
        let refused = l.refused_claims(&t.id).await.unwrap();
        assert_eq!(refused.len(), 1, "{refused:?}");
        assert_eq!(
            refused[0].1,
            "the lease of the claim of generation 1 was live"
        );
        refused_again_under_the_next_claim(&mut l, &t.id).await;
    }

    async fn refused_again_under_the_next_claim(l: &mut Ledger, id: &str) {
        // under the next claim, the same holder refused again is on the record again
        l.release(id, held("s1", 1), None).await.unwrap();
        claim(l, id, "s3", mins(30)).await.unwrap();
        assert!(is_wrong_state(&claim(l, id, "s2", mins(30)).await));
        let refused = l.refused_claims(id).await.unwrap();
        assert_eq!(refused.len(), 2, "{refused:?}");
        l.complete(id, held("s3", 2), "mem-a-h").await.unwrap();
        assert!(is_wrong_state(&claim(l, id, "s4", mins(30)).await));
        assert_eq!(l.refused_claims(id).await.unwrap().len(), 2);
    }

    async fn an_erasure_takes_a_late_handoff_too(b: &Site) {
        let (mut l, now) = b.ledger().await;
        let quoting = task_quoting_the_secret("promote:mem-a-1", "v1");
        let t = admitted(&mut l, quoting).await;
        claim(&mut l, &t.id, "s1", mins(30)).await.unwrap();
        *now.lock().unwrap() = T0 + mins(31);
        let late = format!("late: {SECRET}");
        l.handoff_after(&t.id, held("s1", 1), &late).await.unwrap();
        l.scrub_tasks("dom-a", "promote:mem-a-1", &redaction())
            .await
            .unwrap();
        let kept = l.late_handoffs(&t.id).await.unwrap();
        assert_eq!(kept[0].handoff, redaction().reason);
        assert!(b.payloads().await.iter().all(|p| !p.contains(SECRET)));
    }

    async fn a_task_is_not_admitted_onto_an_invalidated_dependency(b: &Site) {
        let (mut l, _) = b.ledger().await;
        let gone = admitted(&mut l, spec("Gone")).await;
        l.invalidate(&gone.id, "obsolete", None).await.unwrap();
        let rests = l
            .create(in_domain("dom-a", "Rests on it", &[&gone]))
            .await
            .unwrap()
            .task;
        assert!(is_wrong_state(&l.admit(&rests.id).await));
        assert_eq!(l.get(&rests.id).await.unwrap().status, Status::Proposed);
    }

    /// A ledger from before tasks were blocked admitted a waiting task as open.
    async fn an_open_task_of_an_older_ledger_is_retriaged_with_its_dependency(b: &Site) {
        let (mut l, _) = b.ledger().await;
        let gone = admitted(&mut l, spec("Will be dropped")).await;
        let rests = l
            .create(in_domain("dom-a", "Rests on it", &[&gone]))
            .await
            .unwrap()
            .task;
        l.append(&rests.id, &Event::Admitted).await.unwrap();
        assert_eq!(l.get(&rests.id).await.unwrap().status, Status::Open);
        l.invalidate(&gone.id, "obsolete", None).await.unwrap();
        assert_eq!(l.get(&rests.id).await.unwrap().status, Status::Proposed);
    }

    async fn an_erasure_that_invalidates_a_task_sends_back_what_it_blocked(b: &Site) {
        let (mut l, _) = b.ledger().await;
        let erased = admitted(&mut l, task_quoting_the_secret("promote:mem-a-1", "v1")).await;
        let rests = admitted(&mut l, in_domain("dom-a", "Rests on it", &[&erased])).await;
        assert_eq!(rests.status, Status::Blocked);
        l.scrub_tasks("dom-a", "promote:mem-a-1", &redaction())
            .await
            .unwrap();
        assert_eq!(l.get(&erased.id).await.unwrap().status, Status::Invalidated);
        assert_eq!(l.get(&rests.id).await.unwrap().status, Status::Proposed);
    }

    /// A ledger file from before a task carried a definition: no `extra` column.
    #[tokio::test]
    async fn on_sqlite_a_ledger_from_before_the_extra_column_is_given_it() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ledger.db");
        let id = {
            let mut l = Ledger::open(&file).unwrap();
            admitted(&mut l, spec("Old")).await.id
        };
        rusqlite::Connection::open(&file)
            .unwrap()
            .execute_batch("ALTER TABLE tasks DROP COLUMN extra;")
            .unwrap();
        let mut l = Ledger::open(&file).unwrap();
        assert_eq!(l.get(&id).await.unwrap().spec.definition, None);
        let mut defined = spec("New");
        defined.definition = Some("review-change@mem-p-1".into());
        defined.acceptance = vec!["It is checked.".into()];
        let new = l.create(defined.clone()).await.unwrap().task;
        assert_eq!(new.spec.definition, defined.definition);
        // a rebuild reads the same out of the events
        l.rebuild().await.unwrap();
        assert_eq!(
            l.get(&new.id).await.unwrap().spec.acceptance,
            defined.acceptance
        );
    }

    /// Every task row's state set to a name this code does not have.
    async fn make_stale(b: &Site) {
        match b {
            Site::Sqlite(dir) => {
                rusqlite::Connection::open(dir.path().join("ledger.db"))
                    .unwrap()
                    .execute("UPDATE tasks SET status = 'admitted'", [])
                    .unwrap();
            }
            Site::Postgres(server) => {
                let c = pg_client(server).await;
                let schemas = c
                    .query("SELECT schema FROM public.ledger_domains", &[])
                    .await
                    .unwrap();
                for row in schemas {
                    let s: String = row.get(0);
                    c.execute(&format!("UPDATE {s}.tasks SET status = 'admitted'"), &[])
                        .await
                        .unwrap();
                }
            }
        }
    }

    async fn a_projection_with_a_state_name_of_an_older_ledger_is_rebuilt_at_open(b: &Site) {
        let id = {
            let mut l = b.open().await;
            admitted(&mut l, spec("Old")).await.id
        };
        make_stale(b).await;
        let l = b.open().await;
        let open = l.list("dom-a", Status::Open).await.unwrap();
        assert_eq!(
            open.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            [id.as_str()]
        );
    }

    /// The lease ends on a whole second and the clock stands a half second after: the two
    /// times, as text, sort the other way.
    async fn a_lease_that_ran_out_half_a_second_ago_is_lost_to_claim_next(b: &Site) {
        let (mut l, now) = b.ledger().await;
        let t = admitted(&mut l, spec("Long job")).await;
        claim(&mut l, &t.id, "s1", mins(5)).await.unwrap();
        *now.lock().unwrap() = T0 + mins(5) - Duration::milliseconds(500);
        assert!(next(&mut l, "s2", mins(5)).await.is_none(), "still held");
        *now.lock().unwrap() = T0 + mins(5) + Duration::milliseconds(500);
        let c = next(&mut l, "s2", mins(5)).await.unwrap();
        assert_eq!((c.task.id.as_str(), c.generation), (t.id.as_str(), 2));
    }

    /// Filed half a second apart: the earlier time has fewer digits of a second, and
    /// as text sorts after the later one.
    async fn of_two_tasks_of_one_priority_the_one_filed_first_is_claimed_first(b: &Site) {
        let (mut l, now) = b.ledger().await;
        let early = admitted(&mut l, spec("Early")).await;
        *now.lock().unwrap() = T0 + Duration::milliseconds(500);
        let late = admitted(&mut l, spec("Late")).await;
        assert_eq!(next(&mut l, "s", mins(5)).await.unwrap().task.id, early.id);
        assert_eq!(next(&mut l, "s", mins(5)).await.unwrap().task.id, late.id);
    }

    /// The candidate was claimed by another connection, and that committed, between the
    /// read of the queue and the lock of its row.
    #[tokio::test]
    async fn on_postgres_a_candidate_claimed_meanwhile_is_not_taken() {
        let Some(server) = PgServer::start().await.unwrap() else {
            return;
        };
        let mut l = Ledger::connect(server.url()).await.unwrap();
        let t = admitted(&mut l, spec("One holder")).await;
        let other = admitted(&mut l, spec("Free")).await;
        let now = l.now();
        claim(&mut l, &t.id, "first", mins(30)).await.unwrap();
        let Backend::Postgres(db) = &mut l.backend else {
            unreachable!("a ledger connected to PostgreSQL");
        };
        let until = now + mins(30);
        assert!(!db.take_alone(&t.id, now, ("second", until)).await.unwrap());
        assert!(db
            .take_alone(&other.id, now, ("second", until))
            .await
            .unwrap());
        let held = l.get(&t.id).await.unwrap();
        assert_eq!(
            (held.holder.as_deref(), held.generation),
            (Some("first"), 1)
        );
        assert_eq!(
            l.get(&other.id).await.unwrap().holder.as_deref(),
            Some("second")
        );
    }

    fn is_fenced<T>(r: &Result<T, Error>, given: u64, current: u64) -> bool {
        matches!(r, Err(Error::Fenced { given: g, current: c, .. }) if *g == given && *c == current)
    }

    fn held(holder: &str, generation: u64) -> Held<'_> {
        Held { holder, generation }
    }

    /// One name, two sessions: the one whose lease ran out and the one that then claimed.
    async fn a_claim_made_since_fences_the_holder_of_an_earlier_one_whatever_its_name(b: &Site) {
        let (mut l, now) = b.ledger().await;
        let t = admitted(&mut l, spec("Long job")).await;
        let first = claim(&mut l, &t.id, "agent:codex", mins(30)).await.unwrap();
        assert_eq!(first.generation, 1);
        *now.lock().unwrap() = T0 + mins(31);
        let second = next(&mut l, "agent:codex", mins(30)).await.unwrap();
        assert_eq!(
            (second.task.id.as_str(), second.generation),
            (t.id.as_str(), 2)
        );
        the_earlier_claim_is_fenced(&mut l, &t.id).await;
        // and a generation nobody was given holds nothing either
        assert!(is_fenced(
            &l.complete(&t.id, held("agent:codex", 3), "mem-a-h").await,
            3,
            2
        ));
        let now_held = held("agent:codex", 2);
        l.heartbeat(&t.id, now_held, mins(30)).await.unwrap();
        let done = l.complete(&t.id, now_held, "mem-a-h").await.unwrap();
        assert_eq!((done.status, done.generation), (Status::Done, 2));
    }

    async fn a_handoff_is_left_after_a_claim_only_by_who_held_it(b: &Site) {
        let (mut l, now) = b.ledger().await;
        let t = admitted(&mut l, spec("Long job")).await;
        claim(&mut l, &t.id, "s1", mins(30)).await.unwrap();
        // under a claim in force, its holder completes or releases
        assert!(is_wrong_state(
            &l.handoff_after(&t.id, held("s1", 1), "mem-a-late").await
        ));
        *now.lock().unwrap() = T0 + mins(31);
        claim(&mut l, &t.id, "s2", mins(30)).await.unwrap();
        nobody_else_leaves_a_late_handoff(&mut l, &t.id).await;
        let after = l
            .handoff_after(&t.id, held("s1", 1), "mem-a-late")
            .await
            .unwrap();
        the_late_handoff_is_kept_and_moves_nothing(&mut l, &after).await;
    }

    /// The earlier session has the holder's name and a live lease to point at.
    async fn the_earlier_claim_is_fenced(l: &mut Ledger, id: &str) {
        let stale = held("agent:codex", 1);
        assert!(is_fenced(&l.heartbeat(id, stale, mins(30)).await, 1, 2));
        assert!(is_fenced(&l.release(id, stale, None).await, 1, 2));
        assert!(is_fenced(&l.complete(id, stale, "mem-a-h").await, 1, 2));
        assert_eq!(l.get(id).await.unwrap().status, Status::Claimed);
    }

    /// Not someone who never held it, at any generation, and not the holder now for the
    /// earlier claim.
    async fn nobody_else_leaves_a_late_handoff(l: &mut Ledger, id: &str) {
        for other in [
            held("s3", 1),
            held("s2", 1),
            held("s1", 2),
            held("s1", 0),
            held("s1", 9),
        ] {
            let refused = l.handoff_after(id, other, "mem-a-late").await;
            assert!(is_not_holder(&refused), "{other:?}");
        }
    }

    async fn an_invalidated_dependency_sends_its_dependents_back(
        l: &mut Ledger,
        other: &Task,
        waits: &Task,
    ) {
        // a second dependent of the task that is about to be invalidated
        let mut also = spec("Waits on the other");
        also.depends_on = vec![other.id.clone()];
        let also = admitted(l, also).await;
        claim(l, &other.id, "s", mins(5)).await.unwrap();
        l.release(&other.id, held("s", 1), None).await.unwrap();
        l.invalidate(&other.id, "obsolete", None).await.unwrap();
        // what rested on an invalidated task goes back to triage
        assert_eq!(l.get(&waits.id).await.unwrap().status, Status::Proposed);
        assert_eq!(l.get(&also.id).await.unwrap().status, Status::Proposed);
    }

    async fn the_late_handoff_is_kept_and_moves_nothing(l: &mut Ledger, after: &Task) {
        // nothing of the task moved
        assert_eq!(after.holder.as_deref(), Some("s2"));
        assert_eq!(
            (after.status, after.generation, after.handoff.clone()),
            (Status::Claimed, 2, None)
        );
        let late = l.late_handoffs(&after.id).await.unwrap();
        assert_eq!(
            late,
            [LateHandoff {
                holder: "s1".into(),
                generation: 1,
                handoff: "mem-a-late".into()
            }]
        );
        // a rebuild replays it and changes nothing of the task
        l.rebuild().await.unwrap();
        assert_eq!(&l.get(&after.id).await.unwrap(), after);
        assert_eq!(l.late_handoffs(&after.id).await.unwrap(), late);
    }

    async fn one_of_two_done_leaves_it_blocked(l: &mut Ledger, first: &Task, waits: &Task) {
        // one of two done: still blocked
        claim(l, &first.id, "s", mins(5)).await.unwrap();
        l.complete(&first.id, held("s", 1), "mem-a-1")
            .await
            .unwrap();
        assert_eq!(l.get(&waits.id).await.unwrap().status, Status::Blocked);
        // a rebuild reads the same states out of the events
        l.rebuild().await.unwrap();
        assert_eq!(l.get(&waits.id).await.unwrap().status, Status::Blocked);
    }

    async fn a_refused_claim_of_a_named_task_is_on_its_record(b: &Site) {
        let (mut l, _) = b.ledger().await;
        let t = admitted(&mut l, spec("One holder")).await;
        claim(&mut l, &t.id, "s1", mins(30)).await.unwrap();
        assert!(l.refused_claims(&t.id).await.unwrap().is_empty());
        assert!(is_wrong_state(&claim(&mut l, &t.id, "s2", mins(30)).await));
        let refused = l.refused_claims(&t.id).await.unwrap();
        assert_eq!(
            refused,
            [(
                "s2".to_string(),
                "the lease of the claim of generation 1 was live".to_string()
            )]
        );
        let held_still = l.get(&t.id).await.unwrap();
        assert_eq!(
            (held_still.holder.as_deref(), held_still.generation),
            (Some("s1"), 1)
        );
    }

    async fn a_task_is_blocked_until_its_dependencies_are_done_and_retriaged_when_one_is_invalidated(
        b: &Site,
    ) {
        let (mut l, _) = b.ledger().await;
        let first = admitted(&mut l, spec("First")).await;
        let other = admitted(&mut l, spec("Other")).await;
        let mut waits = spec("Waits on both");
        waits.depends_on = vec![first.id.clone(), other.id.clone()];
        let waits = admitted(&mut l, waits).await;
        assert_eq!(waits.status, Status::Blocked);
        assert_eq!(
            first.status,
            Status::Open,
            "a task with nothing to wait on is open"
        );
        assert!(is_wrong_state(
            &claim(&mut l, &waits.id, "s", mins(5)).await
        ));
        one_of_two_done_leaves_it_blocked(&mut l, &first, &waits).await;
        an_invalidated_dependency_sends_its_dependents_back(&mut l, &other, &waits).await;
    }

    async fn the_last_dependency_done_opens_a_blocked_task(b: &Site) {
        let (mut l, _) = b.ledger().await;
        let first = admitted(&mut l, spec("First")).await;
        let mut waits = spec("Waits");
        waits.depends_on = vec![first.id.clone()];
        let waits = admitted(&mut l, waits).await;
        assert_eq!(waits.status, Status::Blocked);
        claim(&mut l, &first.id, "s", mins(5)).await.unwrap();
        l.complete(&first.id, held("s", 1), "mem-a-1")
            .await
            .unwrap();
        assert_eq!(l.get(&waits.id).await.unwrap().status, Status::Open);
        claim(&mut l, &waits.id, "s", mins(5)).await.unwrap();
    }

    async fn an_unrenewed_lease_is_lost_at_expiry_for_both_sides(b: &Site) {
        let (mut l, now) = b.ledger().await;
        let t = admitted(&mut l, spec("Long job")).await;
        claim(&mut l, &t.id, "s1", mins(30)).await.unwrap();
        *now.lock().unwrap() = T0 + mins(29);
        assert!(next(&mut l, "s2", mins(30)).await.is_none(), "still held");
        *now.lock().unwrap() = T0 + mins(31);
        let c = next(&mut l, "s2", mins(30)).await.unwrap();
        assert_eq!(c.task.id, t.id);
        assert_eq!(c.task.holder.as_deref(), Some("s2"));
        assert_eq!(c.task.generation, 2);
        stale_holder_is_refused(&mut l, &t, &now).await;
    }

    async fn stale_holder_is_refused(l: &mut Ledger, t: &Task, now: &Mutex<OffsetDateTime>) {
        assert!(
            is_not_holder(&l.heartbeat_as(&t.id, "s1", mins(30)).await),
            "stale holder"
        );
        assert!(is_not_holder(&l.complete_as(&t.id, "s1", "mem-a-h").await));
        assert!(is_not_holder(&l.release_as(&t.id, "s1", None).await));
        // s2's own lease also expires if never renewed
        *now.lock().unwrap() = T0 + mins(62);
        assert!(is_not_holder(&l.heartbeat_as(&t.id, "s2", mins(30)).await));
    }

    async fn requirements_match_the_manifest_and_priority_orders_the_queue(b: &Site) {
        let (mut l, _) = b.ledger().await;
        let mut needs_tool = spec("Needs github");
        needs_tool.requirements = vec!["tool:github".into()];
        needs_tool.priority = 10;
        let needs_tool = admitted(&mut l, needs_tool).await;
        let mut low = spec("Low");
        low.priority = 1;
        let low = admitted(&mut l, low).await;
        let mut high = spec("High");
        high.priority = 5;
        let high = admitted(&mut l, high).await;
        the_queue_is_ranked(&mut l, &needs_tool, &low, &high).await;
    }

    async fn the_queue_is_ranked(l: &mut Ledger, needs_tool: &Task, low: &Task, high: &Task) {
        let c = next(l, "s1", mins(5)).await.unwrap();
        assert_eq!(c.task.id, high.id, "highest priority the manifest can take");
        let c = l
            .claim_next(
                "dom-a",
                "s1",
                mins(5),
                &["tool:github".into()],
                Label::Restricted,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(c.task.id, needs_tool.id);
        let c = next(l, "s1", mins(5)).await.unwrap();
        assert_eq!(c.task.id, low.id);
        assert!(is_wrong_state(
            &claim(l, &needs_tool.id, "s3", mins(5)).await
        ));
    }

    async fn a_task_goes_only_to_a_session_whose_handling_covers_it(b: &Site) {
        let (mut l, _) = b.ledger().await;
        let mut secret = spec("Rotate the signing key");
        secret.classification = Label::Restricted;
        secret.priority = 10;
        let secret = admitted(&mut l, secret).await;
        let mut low = spec("Tidy the docs");
        low.priority = 1;
        let low = admitted(&mut l, low).await;
        handling_bounds_the_claim(&mut l, &secret, &low).await;
    }

    /// `claim_next` in `dom-a` for a session whose handling is public.
    async fn public(l: &mut Ledger) -> Option<Claim> {
        l.claim_next("dom-a", "s1", mins(5), &[], Label::Public)
            .await
            .unwrap()
    }

    async fn handling_bounds_the_claim(l: &mut Ledger, secret: &Task, low: &Task) {
        let c = public(l).await.unwrap();
        assert_eq!(
            c.task.id, low.id,
            "the top task a public session can handle"
        );
        assert!(public(l).await.is_none(), "nothing it may handle is left");
        assert!(is_wrong_state(
            &l.claim(&secret.id, "s1", mins(5), &[], Label::Internal)
                .await
        ));
        assert_eq!(l.get(&secret.id).await.unwrap().status, Status::Open);
        let c = next(l, "s2", mins(5)).await.unwrap();
        assert_eq!(c.task.id, secret.id, "a restricted session takes it");
    }

    async fn dependencies_block_until_complete(b: &Site) {
        let (mut l, _) = b.ledger().await;
        let parent = admitted(&mut l, spec("Parent")).await;
        let mut child_spec = spec("Child");
        child_spec.depends_on = vec![parent.id.clone()];
        child_spec.priority = 100;
        let child = admitted(&mut l, child_spec).await;
        assert_eq!(l.dependents(&parent.id).await.unwrap()[0].id, child.id);
        let c = next(&mut l, "s1", mins(5)).await.unwrap();
        assert_eq!(c.task.id, parent.id, "the child outranks it but is blocked");
        assert!(is_wrong_state(
            &claim(&mut l, &child.id, "s2", mins(5)).await
        ));
        l.complete_as(&parent.id, "s1", "mem-a-h1").await.unwrap();
        let c = next(&mut l, "s1", mins(5)).await.unwrap();
        assert_eq!(c.task.id, child.id);
    }

    async fn release_returns_to_pool_and_invalidate_is_terminal(b: &Site) {
        let (mut l, _) = b.ledger().await;
        let t = admitted(&mut l, spec("Give back")).await;
        claim(&mut l, &t.id, "s1", mins(5)).await.unwrap();
        let t = l.release_as(&t.id, "s1", Some("mem-x-left")).await.unwrap();
        assert_eq!(
            t.handoff.as_deref(),
            Some("mem-x-left"),
            "the handoff rides with the released task"
        );
        assert_eq!(t.status, Status::Open);
        assert!(t.holder.is_none());
        let c = next(&mut l, "s2", mins(5)).await.unwrap();
        assert_eq!(c.task.generation, 2);
        invalidate_is_terminal(&mut l, &t).await;
    }

    /// A continuation of `parent`, filed by the run that delegated, waiting on `deps`.
    fn resume(parent: &Task, deps: &[&Task]) -> TaskSpec {
        TaskSpec {
            depends_on: deps.iter().map(|d| d.id.clone()).collect(),
            origin: Some(continues(&parent.id)),
            upstream_event: Some(parent.id.clone()),
            ..spec(&format!("Resume: {}", parent.spec.title))
        }
    }

    /// A parent whose run delegated one child, with a continuation waiting on it, and
    /// was released: (parent, child, continuation).
    async fn delegated(l: &mut Ledger) -> (Task, Task, Task) {
        let parent = admitted(l, spec("Compare two decisions")).await;
        claim(l, &parent.id, "s1", mins(5)).await.unwrap();
        let child = admitted(l, spec("Summarise one")).await;
        let cont = admitted(l, resume(&parent, &[&child])).await;
        l.release_as(&parent.id, "s1", Some("mem-a-left"))
            .await
            .unwrap();
        (parent, child, cont)
    }

    async fn a_task_waits_on_its_continuation(b: &Site) {
        let (mut l, _) = b.ledger().await;
        let (parent, child, cont) = delegated(&mut l).await;
        // back on the board, but nobody's to claim while its work goes on
        let c = next(&mut l, "s2", mins(5)).await.unwrap();
        assert_eq!(c.task.id, child.id, "the child, not the parent");
        assert!(is_wrong_state(
            &claim(&mut l, &parent.id, "s2", mins(5)).await
        ));
        let early = l
            .complete_by_continuation(&parent.id, &cont.id, "mem-a-h")
            .await;
        assert!(
            is_wrong_state(&early),
            "not before the continuation is complete"
        );
    }

    async fn a_task_closes_with_its_continuation(b: &Site) {
        let (mut l, _) = b.ledger().await;
        let (parent, child, cont) = delegated(&mut l).await;
        claim(&mut l, &child.id, "s2", mins(5)).await.unwrap();
        l.complete_as(&child.id, "s2", "mem-a-child").await.unwrap();
        claim(&mut l, &cont.id, "s3", mins(5)).await.unwrap();
        l.complete_as(&cont.id, "s3", "mem-a-done").await.unwrap();
        let by_child = l
            .complete_by_continuation(&parent.id, &child.id, "mem-a-h")
            .await;
        assert!(is_wrong_state(&by_child), "only by a continuation of it");
        let closed = l
            .complete_by_continuation(&parent.id, &cont.id, "mem-a-done")
            .await
            .unwrap();
        assert_eq!(closed.status, Status::Done);
        assert_eq!(closed.handoff.as_deref(), Some("mem-a-done"));
        let twice = l
            .complete_by_continuation(&parent.id, &cont.id, "mem-a-done")
            .await;
        assert!(is_wrong_state(&twice));
    }

    async fn a_continuation_given_up_on_gives_its_task_back(b: &Site) {
        let (mut l, _) = b.ledger().await;
        let parent = admitted(&mut l, spec("Compare again")).await;
        let dead = admitted(&mut l, resume(&parent, &[])).await;
        assert!(is_wrong_state(
            &claim(&mut l, &parent.id, "s1", mins(5)).await
        ));
        l.invalidate(&dead.id, "a dead end", None).await.unwrap();
        claim(&mut l, &parent.id, "s1", mins(5)).await.unwrap();
    }

    fn promotion(entry: &str, digest: &str) -> PromotionSpec {
        PromotionSpec {
            domain: "dom-a".into(),
            entry: entry.into(),
            tier: TrustTier::Curated,
            by: "agent:curator".into(),
            author: "human:test:me".into(),
            why: "checked against the tree".into(),
            classification: Label::Internal,
            digest: digest.into(),
            new_digest: format!("{digest}-curated"),
        }
    }

    async fn a_promotion_is_recorded_once_per_version(b: &Site) {
        let (mut l, now) = b.ledger().await;
        let first = promotion("mem-a-1", "d1");
        assert!(l.record_promotion(&first).await.unwrap(), "a new record");
        let events = l.event_count().await.unwrap();
        assert_eq!(events, 1, "one event for one record");
        assert!(
            !l.record_promotion(&first).await.unwrap(),
            "the same version again is not new"
        );
        assert_eq!(
            l.event_count().await.unwrap(),
            events,
            "and appends nothing"
        );
        *now.lock().unwrap() = T0 + mins(1);
        a_new_version_is_a_new_record(&mut l, first).await;
    }

    async fn a_new_version_is_a_new_record(l: &mut Ledger, first: PromotionSpec) {
        let again = PromotionSpec {
            digest: "d2".into(),
            why: "the id came back at raw and was checked again".into(),
            ..first.clone()
        };
        assert!(
            l.record_promotion(&again).await.unwrap(),
            "a new version is a new record"
        );
        let got = l.promotions("dom-a", "mem-a-1").await.unwrap();
        assert_eq!(
            got,
            vec![
                PromotionRecord {
                    spec: first,
                    why_erased: false,
                    at: T0
                },
                PromotionRecord {
                    spec: again,
                    why_erased: false,
                    at: T0 + mins(1)
                },
            ],
            "every field kept, oldest first"
        );
        assert!(l.promotions("dom-a", "mem-a-2").await.unwrap().is_empty());
        assert!(l.promotions("dom-b", "mem-a-1").await.unwrap().is_empty());
    }

    /// A promotion whose new version never landed leaves its record; a second promoter
    /// of the same version is not that record, and is kept with its own why.
    async fn a_second_promoter_of_the_same_version_is_recorded(b: &Site) {
        let (mut l, _) = b.ledger().await;
        let first = promotion("mem-a-1", "d1");
        assert!(l.record_promotion(&first).await.unwrap());
        let second = PromotionSpec {
            by: "agent:auditor".into(),
            why: "read it against the release notes".into(),
            ..first.clone()
        };
        assert!(
            l.record_promotion(&second).await.unwrap(),
            "another promoter is another record"
        );
        let got: Vec<PromotionSpec> = l
            .promotions("dom-a", "mem-a-1")
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.spec)
            .collect();
        assert_eq!(got, vec![first, second.clone()]);
        assert!(!l.record_promotion(&second).await.unwrap(), "once each");
    }

    const SECRET: &str = "passphrase-Qz7xx";

    /// Two promotions of `mem-a-1` whose whys quote its secret, one of `mem-a-2`, and one
    /// of an entry with the same id in `dom-b`, which is another entry.
    async fn promotions_that_quote_a_secret(l: &mut Ledger) -> Vec<PromotionSpec> {
        let first = PromotionSpec {
            why: format!("the vault opens with {SECRET}, as it says"),
            ..promotion("mem-a-1", "d1")
        };
        let second = PromotionSpec {
            by: "agent:auditor".into(),
            why: format!("tried {SECRET} on the vault"),
            ..first.clone()
        };
        let other = promotion("mem-a-2", "d9");
        let elsewhere = PromotionSpec {
            domain: "dom-b".into(),
            why: "dom-b's own reading of its own entry".into(),
            ..promotion("mem-a-1", "d1")
        };
        for p in [&first, &second, &other, &elsewhere] {
            assert!(l.record_promotion(p).await.unwrap());
        }
        vec![first, second, other, elsewhere]
    }

    /// The records of `entry` in `domain` are exactly `spec`, why and all.
    async fn kept_whole(l: &Ledger, domain: &str, entry: &str, spec: &PromotionSpec) {
        let got = l.promotions(domain, entry).await.unwrap();
        assert_eq!(got.len(), 1, "{domain}/{entry}: {got:?}");
        assert_eq!(&got[0].spec, spec, "{domain}/{entry} keeps its why");
        assert!(!got[0].why_erased);
    }

    async fn an_erasure_takes_the_why_and_a_replay_keeps_it_out(b: &Site) {
        let (mut l, _) = b.ledger().await;
        let specs = promotions_that_quote_a_secret(&mut l).await;
        let scrubbed = l.scrub_promotions("dom-a", "mem-a-1").await.unwrap();
        assert_eq!(scrubbed.records, 2);
        let kept: Vec<PromotionRecord> = specs[..2]
            .iter()
            .map(|s| PromotionRecord {
                spec: PromotionSpec {
                    why: String::new(),
                    ..s.clone()
                },
                why_erased: true,
                at: T0,
            })
            .collect();
        assert_eq!(
            l.promotions("dom-a", "mem-a-1").await.unwrap(),
            kept,
            "the why goes; who, which version, which tier and when stay"
        );
        another_entry_and_the_log_and_a_replay(b, &mut l, &specs, &kept).await;
    }

    async fn another_entry_and_the_log_and_a_replay(
        b: &Site,
        l: &mut Ledger,
        specs: &[PromotionSpec],
        kept: &[PromotionRecord],
    ) {
        kept_whole(l, "dom-a", "mem-a-2", &specs[2]).await;
        kept_whole(l, "dom-b", "mem-a-1", &specs[3]).await;
        assert!(
            !b.payloads().await.iter().any(|p| p.contains(SECRET)),
            "the log no longer holds the why"
        );
        l.rebuild().await.unwrap();
        assert_eq!(
            l.promotions("dom-a", "mem-a-1").await.unwrap(),
            kept,
            "replay keeps it out"
        );
        // and replay brings back every other why, in this domain and the other
        kept_whole(l, "dom-a", "mem-a-2", &specs[2]).await;
        kept_whole(l, "dom-b", "mem-a-1", &specs[3]).await;
        let again = l.scrub_promotions("dom-a", "mem-a-1").await.unwrap();
        assert_eq!(again.records, 2, "running it again counts what is scrubbed");
    }

    /// The redaction an erasure of `mem-a-1` gives its tasks.
    fn redaction() -> Redaction {
        Redaction {
            title: "Approve: verified for mem-a-1".into(),
            body: "`mem-a-1` was erased.".into(),
            reason: "the entry was erased".into(),
        }
    }

    /// A task filed from `origin` on `event` whose title and body quote the secret.
    fn task_quoting_the_secret(origin: &str, event: &str) -> TaskSpec {
        TaskSpec {
            title: format!("Approve: verified for The vault opens with {SECRET}"),
            body: format!("proposed because {SECRET} opens it"),
            origin: Some(origin.into()),
            upstream_event: Some(event.into()),
            ..spec("")
        }
    }

    /// Three tasks: an open one and a completed one filed from `promote:mem-a-1`, and an
    /// open one filed from `promote:mem-a-2`.
    async fn tasks_quoting_the_secret(l: &mut Ledger) -> (Task, Task, Task) {
        let open = admitted(l, task_quoting_the_secret("promote:mem-a-1", "verified")).await;
        let done = admitted(l, task_quoting_the_secret("promote:mem-a-1", "again")).await;
        claim(l, &done.id, "s1", mins(5)).await.unwrap();
        l.complete_as(&done.id, "s1", "mem-a-1").await.unwrap();
        let other = admitted(l, task_quoting_the_secret("promote:mem-a-2", "verified")).await;
        (open, done, other)
    }

    async fn an_erasure_rewrites_the_tasks_filed_from_an_origin(b: &Site) {
        let (mut l, _) = b.ledger().await;
        let (open, done, other) = tasks_quoting_the_secret(&mut l).await;
        let scrubbed = l
            .scrub_tasks("dom-a", "promote:mem-a-1", &redaction())
            .await;
        assert_eq!(scrubbed.unwrap(), 2);
        the_tasks_hold_the_redaction(&l, &open, &done, &other).await;
        let holding = b
            .payloads()
            .await
            .iter()
            .filter(|p| p.contains(SECRET))
            .count();
        assert_eq!(
            holding, 1,
            "the log holds the text in the other entry's task alone"
        );
        l.rebuild().await.unwrap();
        the_tasks_hold_the_redaction(&l, &open, &done, &other).await;
        let again = l
            .scrub_tasks("dom-a", "promote:mem-a-1", &redaction())
            .await;
        assert_eq!(
            again.unwrap(),
            2,
            "running it again counts what holds the redaction"
        );
    }

    async fn the_tasks_hold_the_redaction(l: &Ledger, open: &Task, done: &Task, other: &Task) {
        let open_now = l.get(&open.id).await.unwrap();
        assert_eq!(
            open_now.status,
            Status::Invalidated,
            "an open task is closed"
        );
        assert_eq!(open_now.reason.as_deref(), Some("the entry was erased"));
        redacted(&open_now);
        let done_now = l.get(&done.id).await.unwrap();
        assert_eq!(
            done_now.status,
            Status::Done,
            "a closed task keeps its status"
        );
        assert_eq!(
            done_now.handoff.as_deref(),
            Some("the entry was erased"),
            "the handoff is free text, and goes with the rest"
        );
        redacted(&done_now);
        let other_now = l.get(&other.id).await.unwrap();
        assert_eq!(
            other_now.spec, other.spec,
            "another entry's task is untouched"
        );
        assert_eq!(other_now.status, Status::Open);
    }

    /// The task holds the redaction and nothing else of the entry, and the rest of it
    /// is as it was filed.
    fn redacted(t: &Task) {
        assert_eq!(t.spec.title, "Approve: verified for mem-a-1");
        assert_eq!(t.spec.body, "`mem-a-1` was erased.");
        assert_eq!(
            t.spec.origin.as_deref(),
            Some("promote:mem-a-1"),
            "the origin stays"
        );
        assert_eq!(t.spec.created_by, "human:test:me", "who filed it stays");
        assert_eq!(t.created, T0, "and when");
    }

    /// The reason a proposal was declined for, and the handoffs left on or closing a
    /// task, are free text an approver can quote the entry in: the scrub takes them too,
    /// in the events, in the rows and after a replay.
    async fn an_erasure_takes_the_reason_and_the_handoff_of_the_tasks_too(b: &Site) {
        let (mut l, _) = b.ledger().await;
        let declined = admitted(
            &mut l,
            task_quoting_the_secret("promote:mem-a-1", "verified"),
        )
        .await;
        l.invalidate(
            &declined.id,
            &format!("declined: {SECRET} no longer opens it"),
            None,
        )
        .await
        .unwrap();
        let left = admitted(&mut l, task_quoting_the_secret("promote:mem-a-1", "again")).await;
        claim(&mut l, &left.id, "s1", mins(5)).await.unwrap();
        l.release_as(&left.id, "s1", Some(&format!("left: {SECRET}")))
            .await
            .unwrap();
        claim(&mut l, &left.id, "s2", mins(5)).await.unwrap();
        l.complete_as(&left.id, "s2", &format!("done: {SECRET}"))
            .await
            .unwrap();
        assert_eq!(
            l.scrub_tasks("dom-a", "promote:mem-a-1", &redaction())
                .await
                .unwrap(),
            2
        );
        the_reason_and_the_handoff_hold_the_redaction(b, &l, &declined, &left).await;
        l.rebuild().await.unwrap();
        the_reason_and_the_handoff_hold_the_redaction(b, &l, &declined, &left).await;
    }

    async fn the_reason_and_the_handoff_hold_the_redaction(
        b: &Site,
        l: &Ledger,
        declined: &Task,
        left: &Task,
    ) {
        let declined_now = l.get(&declined.id).await.unwrap();
        assert_eq!(declined_now.status, Status::Invalidated);
        assert_eq!(
            declined_now.reason.as_deref(),
            Some("the entry was erased"),
            "the reason"
        );
        let left_now = l.get(&left.id).await.unwrap();
        assert_eq!(left_now.status, Status::Done);
        assert_eq!(
            left_now.handoff.as_deref(),
            Some("the entry was erased"),
            "the handoff"
        );
        assert!(
            !b.payloads().await.iter().any(|p| p.contains(SECRET)),
            "no event holds the text"
        );
    }

    /// The first ledger file under `dir` whose bytes hold `needle`.
    fn file_holding(dir: &Path, needle: &str) -> Option<std::path::PathBuf> {
        std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|f| f.path())
            .find(|p| {
                std::fs::read(p)
                    .unwrap()
                    .windows(needle.len())
                    .any(|w| w == needle.as_bytes())
            })
    }

    #[tokio::test]
    async fn on_sqlite_an_erased_why_leaves_no_bytes_in_the_files() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ledger.db");
        let mut l = Ledger::open(&file).unwrap();
        // long enough to spill onto pages of its own, which an update frees and does not
        // overwrite
        let why = format!("{SECRET} {}", "and so on ".repeat(900));
        let long = PromotionSpec {
            why,
            ..promotion("mem-a-1", "d1")
        };
        assert!(l.record_promotion(&long).await.unwrap());
        // and buried under later records, so the pages it sat on are deep in the file
        for n in 0..40 {
            let later = promotion(&format!("mem-a-{}", n + 2), "d1");
            assert!(l.record_promotion(&later).await.unwrap());
        }
        // in the file itself, not only in the log
        rusqlite::Connection::open(&file)
            .unwrap()
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap();
        assert_eq!(
            file_holding(dir.path(), SECRET),
            Some(file.clone()),
            "the file held it"
        );
        let scrubbed = l.scrub_promotions("dom-a", "mem-a-1").await.unwrap();
        assert!(scrubbed.beyond_reach.is_empty());
        if let Some(f) = file_holding(dir.path(), SECRET) {
            panic!("{} still holds the erased why", f.display());
        }
    }

    #[test]
    fn a_promoted_payload_from_before_the_new_digest_still_parses() {
        let old = r#"{"kind":"promoted","domain":"dom-a","entry":"mem-a-1","tier":"curated","by":"agent:curator","author":"human:test:me","why":"checked","classification":"internal","digest":"d1"}"#;
        let Event::Promoted { spec, why_erased } = Event::parse("promotion:mem-a-1", old).unwrap()
        else {
            panic!("not a promotion");
        };
        assert_eq!(spec.new_digest, "");
        assert!(!why_erased);
    }

    /// The shapes the promotions table had before this one, each a commit of the branch:
    /// 41e0bc5 had neither `new_digest` nor `why_erased` and an index without the
    /// promoter; 088f890 and 9db9e03 had `new_digest` and the promoter in the index, and
    /// no `why_erased`.
    const OLD_SHAPES: [&str; 2] = ["41e0bc5", "088f890"];

    /// The `promotions` table and its index as `shape` made them, in SQLite's dialect
    /// (`domain` leads the index) or PostgreSQL's (`{s}` the schema, no `domain`).
    fn old_promotions_table(shape: &str, s: Option<&str>) -> String {
        let (table, dot) = s.map_or(("promotions".to_string(), String::new()), |s| {
            (format!("{s}.promotions"), format!("{s}."))
        });
        let (new_digest, key) = match (shape, s) {
            ("41e0bc5", None) => ("", "domain, entry, tier, digest"),
            ("41e0bc5", Some(_)) => ("", "entry, tier, digest"),
            (_, None) => (
                "new_digest TEXT NOT NULL,",
                "domain, entry, tier, digest, promoter",
            ),
            (_, Some(_)) => ("new_digest TEXT NOT NULL,", "entry, tier, digest, promoter"),
        };
        let serial = if s.is_some() {
            "BIGSERIAL PRIMARY KEY"
        } else {
            "INTEGER PRIMARY KEY AUTOINCREMENT"
        };
        let new_digest_value = if new_digest.is_empty() { "" } else { "'', " };
        let new_digest_column = if new_digest.is_empty() {
            ""
        } else {
            "new_digest, "
        };
        format!(
            "CREATE TABLE {table} (seq {serial},
                 domain TEXT NOT NULL, entry TEXT NOT NULL, tier TEXT NOT NULL,
                 promoter TEXT NOT NULL, author TEXT NOT NULL, why TEXT NOT NULL,
                 label_rank BIGINT NOT NULL, digest TEXT NOT NULL, {new_digest} at TEXT NOT NULL);
             CREATE UNIQUE INDEX promotions_version ON {table} ({key});
             INSERT INTO {dot}events (task_id, kind, at, payload) VALUES ('promotion:mem-a-1',
                 'promoted', '2026-10-01T10:00:00Z',
                 '{{\"kind\":\"promoted\",\"domain\":\"dom-a\",\"entry\":\"mem-a-1\",\"tier\":\"curated\",\"by\":\"agent:curator\",\"author\":\"human:test:me\",\"why\":\"checked\",\"classification\":\"internal\",\"digest\":\"d1\"}}');
             INSERT INTO {table} (domain, entry, tier, promoter, author, why, label_rank,
                 digest, {new_digest_column}at) VALUES ('dom-a', 'mem-a-1', 'curated', 'agent:curator',
                 'human:test:me', 'checked', 1, 'd1', {new_digest_value}'2026-10-01T10:00:00Z');"
        )
    }

    /// A ledger file whose promotions table is as `shape` made it, holding one record.
    fn ledger_of_shape(file: &Path, shape: &str) {
        let c = rusqlite::Connection::open(file).unwrap();
        c.execute_batch(&format!(
            "CREATE TABLE events (seq INTEGER PRIMARY KEY AUTOINCREMENT, task_id TEXT NOT NULL,
                 kind TEXT NOT NULL, at TEXT NOT NULL, payload TEXT NOT NULL);
             {}",
            old_promotions_table(shape, None)
        ))
        .unwrap();
    }

    #[tokio::test]
    async fn on_sqlite_a_ledger_of_each_old_shape_is_migrated() {
        for shape in OLD_SHAPES {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("ledger.db");
            ledger_of_shape(&file, shape);
            let l = Ledger::open(&file).unwrap();
            the_migrated_ledger_works(l, shape).await;
        }
    }

    /// Reading, a second promoter of the old version, a rebuild and a scrub all work on
    /// a ledger of an old shape once it is opened.
    async fn the_migrated_ledger_works(mut l: Ledger, shape: &str) {
        let old = l
            .promotions("dom-a", "mem-a-1")
            .await
            .unwrap_or_else(|e| panic!("{shape}: {e}"));
        assert_eq!(old.len(), 1, "{shape}");
        assert_eq!(old[0].spec.new_digest, "", "{shape}");
        assert!(!old[0].why_erased, "{shape}");
        let second = PromotionSpec {
            by: "agent:auditor".into(),
            ..promotion("mem-a-1", "d1")
        };
        assert!(
            l.record_promotion(&second)
                .await
                .unwrap_or_else(|e| panic!("{shape}: {e}")),
            "{shape}: the index has the promoter now"
        );
        assert_eq!(l.rebuild().await.unwrap(), 2, "{shape}");
        assert_eq!(
            l.promotions("dom-a", "mem-a-1").await.unwrap().len(),
            2,
            "{shape}"
        );
        let scrubbed = l.scrub_promotions("dom-a", "mem-a-1").await;
        assert_eq!(
            scrubbed.unwrap_or_else(|e| panic!("{shape}: {e}")).records,
            2,
            "{shape}"
        );
    }

    async fn a_restored_record_keeps_its_time_and_its_erased_why(b: &Site) {
        let (mut l, _) = b.ledger().await;
        let then = T0 - Duration::days(30);
        let records = [
            PromotionRecord {
                spec: promotion("mem-a-1", "d1"),
                why_erased: false,
                at: then,
            },
            PromotionRecord {
                spec: PromotionSpec {
                    why: String::new(),
                    ..promotion("mem-a-1", "d0")
                },
                why_erased: true,
                at: then - Duration::days(1),
            },
        ];
        for r in &records {
            assert!(l.restore_promotion(r).await.unwrap());
            assert!(!l.restore_promotion(r).await.unwrap(), "once");
        }
        assert_eq!(l.promotions("dom-a", "mem-a-1").await.unwrap(), records);
        l.rebuild().await.unwrap();
        assert_eq!(
            l.promotions("dom-a", "mem-a-1").await.unwrap(),
            records,
            "a replay keeps both"
        );
    }

    async fn a_promotion_record_is_no_task(b: &Site) {
        let mut l = b.open().await;
        let parent = admitted(&mut l, spec("A task beside it")).await;
        assert!(l
            .record_promotion(&promotion("mem-a-1", "d1"))
            .await
            .unwrap());
        only_the_task_is_listed(&l, &parent).await;
        no_task_query_finds_the_record(&mut l, &parent).await;
    }

    async fn only_the_task_is_listed(l: &Ledger, parent: &Task) {
        for st in [
            Status::Proposed,
            Status::Open,
            Status::Claimed,
            Status::Done,
            Status::Invalidated,
        ] {
            let ids: Vec<String> = l
                .list("dom-a", st)
                .await
                .unwrap()
                .into_iter()
                .map(|t| t.id)
                .collect();
            let expected = if st == Status::Open {
                vec![parent.id.clone()]
            } else {
                Vec::new()
            };
            assert_eq!(ids, expected, "{st:?}");
        }
    }

    async fn no_task_query_finds_the_record(l: &mut Ledger, parent: &Task) {
        assert!(l.unposted("dom-a").await.unwrap().is_empty());
        assert!(l.dependents("promotion:mem-a-1").await.unwrap().is_empty());
        assert!(l.filed_from("mem-a-1").await.unwrap().is_empty());
        assert!(matches!(
            l.get("promotion:mem-a-1").await,
            Err(Error::NotFound(_))
        ));
        let c = next(l, "s1", mins(5)).await.unwrap();
        assert_eq!(c.task.id, parent.id);
        assert!(
            next(l, "s1", mins(5)).await.is_none(),
            "nothing else to claim"
        );
    }

    async fn replay_keeps_the_promotions(b: &Site) {
        let mut l = b.open().await;
        admitted(&mut l, spec("A task beside it")).await;
        l.record_promotion(&promotion("mem-a-1", "d1"))
            .await
            .unwrap();
        l.record_promotion(&promotion("mem-a-1", "d2"))
            .await
            .unwrap();
        let before = l.promotions("dom-a", "mem-a-1").await.unwrap();
        assert_eq!(before.len(), 2);
        assert_eq!(l.rebuild().await.unwrap(), 4);
        assert_eq!(l.promotions("dom-a", "mem-a-1").await.unwrap(), before);
        assert!(
            !l.record_promotion(&promotion("mem-a-1", "d1"))
                .await
                .unwrap(),
            "replay keeps what makes a record new"
        );
        assert_eq!(l.list("dom-a", Status::Open).await.unwrap().len(), 1);
    }

    async fn invalidate_is_terminal(l: &mut Ledger, t: &Task) {
        let t = l
            .invalidate(&t.id, "duplicate", Some("task-a-other"))
            .await
            .unwrap();
        assert_eq!(t.status, Status::Invalidated);
        assert_eq!(t.reason.as_deref(), Some("duplicate"));
        assert_eq!(t.superseded_by.as_deref(), Some("task-a-other"));
        assert!(
            is_wrong_state(&l.heartbeat_as(&t.id, "s2", mins(5)).await),
            "holder learns at heartbeat"
        );
        assert!(is_wrong_state(&l.invalidate(&t.id, "again", None).await));
        assert!(next(l, "s3", mins(5)).await.is_none());
    }

    async fn origin_and_upstream_event_dedup_a_second_filing(b: &Site) {
        let (mut l, _) = b.ledger().await;
        let mut s = spec("From an issue");
        s.origin = Some("github:apache/x#12".into());
        s.upstream_event = Some("labeled:2026-10-01T09:00:00Z".into());
        let first = l.create(s.clone()).await.unwrap();
        assert!(!first.deduplicated);
        let again = l.create(s.clone()).await.unwrap();
        assert!(again.deduplicated);
        assert_eq!(again.task.id, first.task.id);
        the_pair_finds_the_task(&l, &first.task).await;
        s.upstream_event = Some("labeled:2026-10-02T09:00:00Z".into());
        let other = l.create(s).await.unwrap();
        assert!(!other.deduplicated, "a new upstream event is a new task");
        assert_ne!(other.task.id, first.task.id);
        assert_eq!(
            l.event_count().await.unwrap(),
            2,
            "the duplicate appended nothing"
        );
        dedup_is_scoped_to_the_domain(&mut l, &first.task).await;
    }

    /// The pair names the task create would return, and a pair never filed names none.
    async fn the_pair_finds_the_task(l: &Ledger, first: &Task) {
        let dom = first.spec.domain.as_str();
        let found = l
            .by_origin(dom, "github:apache/x#12", "labeled:2026-10-01T09:00:00Z")
            .await
            .unwrap();
        assert_eq!(found.map(|t| t.id), Some(first.id.clone()));
        let none = l
            .by_origin(dom, "github:apache/x#12", "labeled:2026-10-02T09:00:00Z")
            .await
            .unwrap();
        assert!(none.is_none(), "{none:?}");
        let all = l.with_origin(dom, "github:apache/x#12").await.unwrap();
        assert_eq!(
            all.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            vec![first.id.as_str()]
        );
        assert!(l
            .with_origin(dom, "github:apache/x#13")
            .await
            .unwrap()
            .is_empty());
    }

    /// The same issue filed into another domain is that domain's task: dedup is scoped
    /// to the domain, since one domain's ledger says nothing about another's.
    async fn dedup_is_scoped_to_the_domain(l: &mut Ledger, first: &Task) {
        let mut elsewhere = spec("From the same issue, elsewhere");
        elsewhere.domain = "dom-b".into();
        elsewhere.origin = Some("github:apache/x#12".into());
        elsewhere.upstream_event = Some("labeled:2026-10-01T09:00:00Z".into());
        let theirs = l.create(elsewhere.clone()).await.unwrap();
        assert!(
            !theirs.deduplicated,
            "another domain's filing is not a duplicate"
        );
        assert_ne!(theirs.task.id, first.id);
        assert_eq!(theirs.task.spec.domain, "dom-b");
        assert!(l.create(elsewhere).await.unwrap().deduplicated);
        assert_eq!(l.event_count().await.unwrap(), 3);
    }

    async fn what_was_filed_from_a_task_is_found_by_its_origin(b: &Site) {
        let mut l = b.open().await;
        let parent = l.create(spec("parent")).await.unwrap().task;
        let mut child = spec("child");
        child.origin = Some(format!("task:{}", parent.id));
        child.upstream_event = Some("h1".into());
        let child = l.create(child).await.unwrap().task;
        let mut cont = spec("resume");
        cont.origin = Some(format!("continues:{}", parent.id));
        cont.upstream_event = Some("h2".into());
        cont.depends_on = vec![child.id.clone()];
        let cont = l.create(cont).await.unwrap().task;
        let mut other = spec("other");
        other.origin = Some("task:elsewhere".into());
        other.upstream_event = Some("h3".into());
        l.create(other).await.unwrap();
        let filed = l.filed_from(&parent.id).await.unwrap();
        assert_eq!(
            filed.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            vec![child.id.as_str(), cont.id.as_str()]
        );
        assert!(l.filed_from(&child.id).await.unwrap().is_empty());
    }

    async fn every_operation_appends_one_event_and_replay_rebuilds_the_projection(b: &Site) {
        let (mut l, now) = b.ledger().await;
        let (a, b_task) = nine_events(&mut l, &now).await;
        let before = get_all(&l, &[&a.id, &b_task.id]).await;
        assert_eq!(l.rebuild().await.unwrap(), 9);
        let after = get_all(&l, &[&a.id, &b_task.id]).await;
        assert_eq!(
            before, after,
            "the projection is a pure function of the events"
        );
        assert_eq!(after[0].generation, 2);
        assert_eq!(after[0].status, Status::Done);
        assert_eq!(after[1].status, Status::Invalidated);
    }

    /// One task through every operation and a second one invalidated: nine events.
    async fn nine_events(l: &mut Ledger, now: &Mutex<OffsetDateTime>) -> (Task, Task) {
        let a = l.create(spec("A")).await.unwrap().task;
        l.admit(&a.id).await.unwrap();
        claim(l, &a.id, "s1", mins(5)).await.unwrap();
        *now.lock().unwrap() = T0 + mins(2);
        l.heartbeat_as(&a.id, "s1", mins(5)).await.unwrap();
        l.release_as(&a.id, "s1", None).await.unwrap();
        claim(l, &a.id, "s2", mins(5)).await.unwrap();
        l.complete_as(&a.id, "s2", "mem-a-h").await.unwrap();
        let b = l.create(spec("B")).await.unwrap().task;
        l.invalidate(&b.id, "obsolete", None).await.unwrap();
        assert_eq!(l.event_count().await.unwrap(), 9);
        (a, b)
    }

    async fn not_found_and_persistence(b: &Site) {
        let (l, _) = b.ledger().await;
        assert!(matches!(
            l.get("task-a-none").await,
            Err(Error::NotFound(_))
        ));
        drop(l);
        let id = {
            let mut l = b.open().await;
            admitted(&mut l, spec("Persist")).await.id
        };
        let l = b.open().await;
        assert_eq!(l.get(&id).await.unwrap().status, Status::Open);
        assert_eq!(l.list("dom-a", Status::Open).await.unwrap().len(), 1);
    }

    async fn a_completed_task_from_upstream_is_posted_back_once(b: &Site) {
        let (mut l, _) = b.ledger().await;
        let mut s = spec("From an issue");
        s.origin = Some("github:acme/widgets#12".into());
        s.upstream_event = Some("open:2026-10-01T09:00:00Z".into());
        let from_upstream = l.create(s).await.unwrap().task;
        let local = l.create(spec("Filed by hand")).await.unwrap().task;
        for t in [&from_upstream, &local] {
            l.admit(&t.id).await.unwrap();
            claim(&mut l, &t.id, "s1", mins(5)).await.unwrap();
        }
        assert!(
            l.unposted("dom-a").await.unwrap().is_empty(),
            "nothing is done yet"
        );
        assert!(is_wrong_state(
            &l.post(&from_upstream.id, "https://x/1").await
        ));
        let posted = only_upstream_is_due_and_posted_once(&mut l, &from_upstream, &local).await;
        replay_keeps(&mut l, &posted).await;
    }

    async fn replay_keeps(l: &mut Ledger, task: &Task) {
        let events = l.event_count().await.unwrap();
        l.rebuild().await.unwrap();
        assert_eq!(l.event_count().await.unwrap(), events);
        assert_eq!(&l.get(&task.id).await.unwrap(), task, "replay keeps it");
    }

    async fn only_upstream_is_due_and_posted_once(
        l: &mut Ledger,
        from_upstream: &Task,
        local: &Task,
    ) -> Task {
        l.complete_as(&from_upstream.id, "s1", "mem-a-h1")
            .await
            .unwrap();
        l.complete_as(&local.id, "s1", "mem-a-h2").await.unwrap();
        let due: Vec<String> = l
            .unposted("dom-a")
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(
            due,
            vec![from_upstream.id.clone()],
            "only what came from upstream"
        );
        assert!(l.unposted("dom-b").await.unwrap().is_empty());
        let posted = l
            .post(
                &from_upstream.id,
                "https://github.com/acme/widgets/issues/12#c1",
            )
            .await
            .unwrap();
        assert_eq!(
            posted.posted_to.as_deref(),
            Some("https://github.com/acme/widgets/issues/12#c1")
        );
        assert!(l.unposted("dom-a").await.unwrap().is_empty());
        assert!(is_wrong_state(
            &l.post(&from_upstream.id, "https://x/2").await
        ));
        posted
    }

    /// Two ledgers over the same tables, as two sidecars on one server would be, each
    /// claiming: one holder a task, and nothing claimed twice.
    async fn two_claimers_on_two_connections_get_two_tasks(b: &Site) {
        let (mut l1, _) = b.ledger().await;
        let (mut l2, _) = b.ledger().await;
        let first = admitted(&mut l1, spec("First")).await;
        let second = admitted(&mut l1, spec("Second")).await;
        let (c1, c2) = tokio::join!(
            l1.claim_next("dom-a", "s1", mins(5), &[], Label::Restricted),
            l2.claim_next("dom-a", "s2", mins(5), &[], Label::Restricted),
        );
        let c1 = c1.unwrap().unwrap();
        let c2 = c2.unwrap().unwrap();
        assert_ne!(c1.task.id, c2.task.id, "nothing claimed twice");
        let mut ids = vec![c1.task.id.clone(), c2.task.id.clone()];
        ids.sort();
        let mut expected = vec![first.id, second.id];
        expected.sort();
        assert_eq!(ids, expected);
        assert!(next(&mut l1, "s3", mins(5)).await.is_none());
    }

    /// Two domains, one ledger: each claims only from its own queue, sees only its own
    /// tasks listed, and finds either task by id.
    async fn a_domain_claims_only_its_own_queue(b: &Site) {
        let (mut l, _) = b.ledger().await;
        let a = admitted(&mut l, spec("In a")).await;
        let other = admitted(
            &mut l,
            TaskSpec {
                domain: "dom-b".into(),
                ..spec("In b")
            },
        )
        .await;
        let listed = l.list("dom-b", Status::Open).await.unwrap();
        assert_eq!(
            listed.iter().map(|t| &t.id).collect::<Vec<_>>(),
            vec![&other.id]
        );
        let got = l
            .claim_next("dom-b", "s", mins(5), &[], Label::Restricted)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.task.id, other.id);
        assert!(l
            .claim_next("dom-b", "s", mins(5), &[], Label::Restricted)
            .await
            .unwrap()
            .is_none());
        assert_eq!(l.get(&a.id).await.unwrap().status, Status::Open);
        a_domain_with_no_tasks_has_an_empty_queue(&mut l).await;
    }

    async fn a_domain_with_no_tasks_has_an_empty_queue(l: &mut Ledger) {
        assert!(l.list("dom-none", Status::Open).await.unwrap().is_empty());
        assert!(l
            .claim_next("dom-none", "s", mins(5), &[], Label::Restricted)
            .await
            .unwrap()
            .is_none());
    }

    async fn pg_client(server: &PgServer) -> tokio_postgres::Client {
        let (client, conn) = tokio_postgres::connect(server.url(), tokio_postgres::NoTls)
            .await
            .unwrap();
        tokio::spawn(conn);
        client
    }

    #[tokio::test]
    async fn on_postgres_each_domain_has_a_schema_of_its_own() {
        let Some(server) = PgServer::start().await.unwrap() else {
            return;
        };
        let mut l = Ledger::connect(server.url()).await.unwrap();
        let a = admitted(&mut l, spec("In a")).await;
        let b = admitted(
            &mut l,
            TaskSpec {
                domain: "dom-b".into(),
                ..spec("In b")
            },
        )
        .await;
        let c = pg_client(&server).await;
        let count = |schema: String, id: String| {
            let c = &c;
            async move {
                let row = c
                    .query_one(
                        &format!("SELECT COUNT(*) FROM {schema}.tasks WHERE id = $1"),
                        &[&id],
                    )
                    .await
                    .unwrap();
                row.get::<_, i64>(0)
            }
        };
        let sa = crate::postgres::schema_for("dom-a");
        let sb = crate::postgres::schema_for("dom-b");
        assert_ne!(sa, sb);
        assert_eq!(count(sa.clone(), a.id.clone()).await, 1);
        assert_eq!(count(sb.clone(), b.id.clone()).await, 1);
        assert_eq!(
            count(sa, b.id.clone()).await,
            0,
            "dom-b's task is not in dom-a's schema"
        );
        assert_eq!(count(sb, a.id.clone()).await, 0);
        assert!(
            c.query_one("SELECT to_regclass('public.tasks') IS NULL", &[])
                .await
                .unwrap()
                .get::<_, bool>(0),
            "no shared tasks table"
        );
        // a schema name is readable and does not collide for domains that read alike
        assert!(crate::postgres::schema_for("dom-a").starts_with("\"ledger_dom_a_"));
        assert_ne!(
            crate::postgres::schema_for("dom-a.b"),
            crate::postgres::schema_for("dom-a_b")
        );
    }

    #[tokio::test]
    async fn on_postgres_a_ledger_from_before_domain_schemas_moves_into_them() {
        let Some(server) = PgServer::start().await.unwrap() else {
            return;
        };
        let c = pg_client(&server).await;
        // the shared tables as the first PostgreSQL ledger made them, with one task in
        // each of two domains, one of them admitted
        c.batch_execute(
            "CREATE TABLE public.events (seq BIGSERIAL PRIMARY KEY, task_id TEXT NOT NULL,
                 kind TEXT NOT NULL, at TEXT NOT NULL, payload TEXT NOT NULL);
             CREATE TABLE public.tasks (id TEXT PRIMARY KEY, domain TEXT NOT NULL);",
        )
        .await
        .unwrap();
        let mut ids = Vec::new();
        for (domain, admit) in [("dom-a", true), ("dom-b", false)] {
            let id = dm_core::id::new_task_id(domain);
            let created = Event::Created {
                id: id.clone(),
                spec: TaskSpec {
                    domain: domain.into(),
                    ..spec("Old")
                },
            };
            let mut events = vec![created];
            if admit {
                events.push(Event::Admitted);
            }
            for e in &events {
                c.execute(
                    "INSERT INTO public.events (task_id, kind, at, payload) VALUES ($1, $2, $3, $4)",
                    &[&id, &e.kind(), &fmt_time(T0), &e.payload(&id).unwrap()],
                )
                .await
                .unwrap();
            }
            c.execute(
                "INSERT INTO public.tasks (id, domain) VALUES ($1, $2)",
                &[&id, &domain],
            )
            .await
            .unwrap();
            ids.push(id);
        }
        let l = Ledger::connect(server.url()).await.unwrap();
        assert_eq!(l.get(&ids[0]).await.unwrap().status, Status::Open);
        assert_eq!(l.get(&ids[1]).await.unwrap().status, Status::Proposed);
        assert_eq!(l.get(&ids[1]).await.unwrap().spec.domain, "dom-b");
        assert_eq!(l.event_count().await.unwrap(), 3);
        let kept = c
            .query_one(
                "SELECT to_regclass('public.events_before_domain_schemas') IS NOT NULL
                    AND to_regclass('public.tasks') IS NULL",
                &[],
            )
            .await
            .unwrap();
        assert!(
            kept.get::<_, bool>(0),
            "the old tables are renamed, not dropped"
        );
        // and a second connect finds nothing left to move
        let again = Ledger::connect(server.url()).await.unwrap();
        assert_eq!(again.event_count().await.unwrap(), 3);
    }

    /// A domain whose schema was made before promotions were recorded has no table for
    /// them; connecting adds it, and a promotion there is kept.
    #[tokio::test]
    async fn on_postgres_a_domain_from_before_promotions_gets_their_table() {
        let Some(server) = PgServer::start().await.unwrap() else {
            return;
        };
        let mut l = Ledger::connect(server.url()).await.unwrap();
        let t = admitted(&mut l, spec("Old")).await;
        drop(l);
        let s = crate::postgres::schema_for("dom-a");
        let c = pg_client(&server).await;
        c.batch_execute(&format!("DROP TABLE IF EXISTS {s}.promotions"))
            .await
            .unwrap();
        let mut l = Ledger::connect(server.url()).await.unwrap();
        assert!(l
            .record_promotion(&promotion("mem-a-1", "d1"))
            .await
            .unwrap());
        assert_eq!(l.promotions("dom-a", "mem-a-1").await.unwrap().len(), 1);
        assert_eq!(l.get(&t.id).await.unwrap().status, Status::Open);
        let routed = c
            .query_one(
                "SELECT COUNT(*) FROM public.ledger_tasks WHERE id LIKE 'promotion:%'",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(
            routed.get::<_, i64>(0),
            0,
            "a record is not routed as a task"
        );
    }

    /// A connected ledger on a server whose `dom-a` schema has a promotions table of
    /// `shape`, holding the one old record and no other event, and a client on the server.
    async fn pg_domain_of_shape(
        server: &PgServer,
        shape: &str,
    ) -> (Ledger, tokio_postgres::Client) {
        let mut l = Ledger::connect(server.url()).await.unwrap();
        admitted(&mut l, spec("Makes the schema")).await;
        drop(l);
        let s = crate::postgres::schema_for("dom-a");
        let c = pg_client(server).await;
        c.batch_execute(&format!(
            "DROP TABLE {s}.promotions; DELETE FROM {s}.events; DELETE FROM {s}.tasks;
             DELETE FROM public.ledger_tasks; {}",
            old_promotions_table(shape, Some(&s))
        ))
        .await
        .unwrap();
        (Ledger::connect(server.url()).await.unwrap(), c)
    }

    /// A domain schema of each old shape is migrated when the ledger connects.
    #[tokio::test]
    async fn on_postgres_a_domain_of_each_old_shape_is_migrated_at_connect() {
        for shape in OLD_SHAPES {
            let Some(server) = PgServer::start().await.unwrap() else {
                return;
            };
            let (l, _c) = pg_domain_of_shape(&server, shape).await;
            the_migrated_ledger_works(l, shape).await;
        }
    }

    /// A domain schema of an old shape made after this ledger connected is migrated where
    /// a record, a rebuild or a scrub reaches it.
    #[tokio::test]
    async fn on_postgres_a_domain_of_an_old_shape_made_after_connect_is_migrated() {
        for shape in OLD_SHAPES {
            let Some(server) = PgServer::start().await.unwrap() else {
                return;
            };
            let (mut l, c) = pg_domain_of_shape(&server, shape).await;
            let s = crate::postgres::schema_for("dom-a");
            let back_to_old = format!(
                "DROP TABLE {s}.promotions; DELETE FROM {s}.events; {}",
                old_promotions_table(shape, Some(&s))
            );
            c.batch_execute(&back_to_old).await.unwrap();
            let second = PromotionSpec {
                by: "agent:auditor".into(),
                ..promotion("mem-a-1", "d1")
            };
            assert!(
                l.record_promotion(&second).await.unwrap(),
                "{shape}: record"
            );
            assert_eq!(
                l.promotions("dom-a", "mem-a-1").await.unwrap().len(),
                2,
                "{shape}"
            );
            c.batch_execute(&back_to_old).await.unwrap();
            assert_eq!(l.rebuild().await.unwrap(), 1, "{shape}: rebuild");
            assert_eq!(
                l.promotions("dom-a", "mem-a-1").await.unwrap().len(),
                1,
                "{shape}"
            );
            c.batch_execute(&back_to_old).await.unwrap();
            let scrubbed = l.scrub_promotions("dom-a", "mem-a-1").await;
            assert_eq!(scrubbed.unwrap().records, 1, "{shape}: scrub");
        }
    }

    /// Connecting migrates only a schema that lacks the promotions table, so a domain
    /// another process holds does not hold up the connect.
    #[tokio::test]
    async fn on_postgres_connect_waits_on_no_domain_that_has_its_table() {
        let Some(server) = PgServer::start().await.unwrap() else {
            return;
        };
        let mut l = Ledger::connect(server.url()).await.unwrap();
        admitted(&mut l, spec("In a")).await;
        drop(l);
        let holder = pg_client(&server).await;
        holder
            .execute(
                "SELECT pg_advisory_lock(hashtext('ledger_domain:' || $1))",
                &[&"dom-a"],
            )
            .await
            .unwrap();
        let connected = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            Ledger::connect(server.url()),
        )
        .await;
        assert!(
            connected.is_ok(),
            "connect waited on a domain that needed nothing"
        );
    }

    /// A domain schema an older binary made after this ledger connected has no
    /// promotions table; reading, recording and rebuilding there still work.
    #[tokio::test]
    async fn on_postgres_a_schema_without_the_table_made_after_connect_still_works() {
        let Some(server) = PgServer::start().await.unwrap() else {
            return;
        };
        let mut l = Ledger::connect(server.url()).await.unwrap();
        admitted(&mut l, spec("In a")).await;
        let c = pg_client(&server).await;
        let drop_table = format!(
            "DROP TABLE {}.promotions",
            crate::postgres::schema_for("dom-a")
        );
        c.batch_execute(&drop_table).await.unwrap();
        assert!(l.promotions("dom-a", "mem-a-1").await.unwrap().is_empty());
        assert!(l
            .record_promotion(&promotion("mem-a-1", "d1"))
            .await
            .unwrap());
        c.batch_execute(&drop_table).await.unwrap();
        assert_eq!(l.rebuild().await.unwrap(), 3);
        assert_eq!(l.promotions("dom-a", "mem-a-1").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn every_transition_is_a_span_with_the_task_as_it_stands() {
        use opentelemetry::Value;
        use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let _guard = dm_otel::install(provider);
        let mut l = Ledger::in_memory().unwrap();
        let t = admitted(&mut l, spec("Traced")).await;
        let c = l
            .claim_next("dom-a", "agent:x", mins(5), &[], Label::Restricted)
            .await
            .unwrap()
            .unwrap();
        l.complete_as(&c.task.id, "agent:x", "mem-a-handoff")
            .await
            .unwrap();
        let spans: Vec<_> = exporter
            .get_finished_spans()
            .unwrap()
            .into_iter()
            .filter(|sp| {
                sp.attributes
                    .iter()
                    .any(|kv| kv.key.as_str() == "task.id" && kv.value.as_str() == t.id.as_str())
            })
            .collect();
        let names: Vec<String> = spans.iter().map(|sp| sp.name.to_string()).collect();
        assert_eq!(
            names,
            vec![
                "task.created",
                "task.admitted",
                "task.claimed",
                "task.completed"
            ]
        );
        let attr = |i: usize, k: &str| {
            spans[i]
                .attributes
                .iter()
                .find(|kv| kv.key.as_str() == k)
                .map(|kv| kv.value.clone())
        };
        assert_eq!(attr(0, "task.status"), Some(Value::from("proposed")));
        assert_eq!(attr(2, "task.holder"), Some(Value::from("agent:x")));
        assert_eq!(attr(2, "task.generation"), Some(Value::I64(1)));
        assert_eq!(attr(3, "task.status"), Some(Value::from("done")));
        assert_eq!(attr(3, "memory.domain"), Some(Value::from("dom-a")));
    }

    async fn a_rescore_moves_the_queue_and_keeps_the_filed_priority(b: &Site) {
        let (mut l, _) = b.ledger().await;
        let first = admitted(&mut l, spec("First")).await;
        let second = admitted(&mut l, spec("Second")).await;
        let moved = l
            .rescore(&second.id, 7, "unblocks a ranked task")
            .await
            .unwrap();
        assert_eq!(moved.spec.priority, 7);
        assert_eq!(
            l.filed_priority(&second.id).await.unwrap(),
            0,
            "the filer's say is kept"
        );
        let events = l.event_count().await.unwrap();
        l.rescore(&second.id, 7, "same").await.unwrap();
        assert_eq!(
            l.event_count().await.unwrap(),
            events,
            "no event for no change"
        );
        a_rescored_task_is_claimed_first_and_replayed(&mut l, &first, &second).await;
    }

    async fn a_rescored_task_is_claimed_first_and_replayed(
        l: &mut Ledger,
        first: &Task,
        second: &Task,
    ) {
        let c = next(l, "s", mins(5)).await.unwrap();
        assert_eq!(c.task.id, second.id, "the rescored task is claimed first");
        l.rebuild().await.unwrap();
        assert_eq!(
            l.get(&second.id).await.unwrap().spec.priority,
            7,
            "replay keeps it"
        );
        l.complete_as(&second.id, "s", "h").await.unwrap();
        assert!(is_wrong_state(&l.rescore(&second.id, 1, "late").await));
        assert_eq!(l.get(&first.id).await.unwrap().spec.priority, 0);
    }
}
