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
//! An approval is recorded on the same log too, as one event under the key
//! `approval:<target>`, and projected nowhere: the events are the store, and
//! [`Ledger::approvals`] reads them. It is no task either. It names one action, one
//! entry, one version of that entry, who approved, who may act under it and when it
//! expires. The ledger keeps it and judges nothing: the policy decides what it permits.
//!
//! A task may name a position, with a reason: it is then pinned. A pinned task goes only
//! to a claimant that fills that position, and a claimant that fills a position takes an
//! unpinned task only when the position's charter covers the task's definition. One
//! function, [`seat_takes`], holds that rule for both backends. Here a position is a
//! name; the record that describes it is not the ledger's.
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
    /// The attempt key was used before, and that attempt is over or was for something
    /// else: the same key does not claim or complete a second time. The refusal names
    /// the key and nothing of what it was used for.
    #[error("the attempt key {key} was used before, and that attempt is over")]
    AttemptSpent {
        /// The key.
        key: String,
    },
    /// A task was filed with half a pin: a position and no reason, a reason and no
    /// position, or a position with no name.
    #[error("a pin names a position and gives a reason: {0}")]
    BadPin(String),
    /// The task is pinned to a position the claimant does not fill.
    #[error("task {id} is pinned to position {position}; only a session that fills that position claims it")]
    Pinned {
        /// The task.
        id: String,
        /// The position it is pinned to.
        position: String,
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

/// Why a task is pinned to a position.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PinReason {
    /// The position holds open commitments the task depends on.
    Continuity,
    /// Someone outside expects one identity.
    Relationship,
    /// Review goes to a position other than the author's.
    Separation,
}

impl PinReason {
    /// The reason as the ledger stores it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            PinReason::Continuity => "continuity",
            PinReason::Relationship => "relationship",
            PinReason::Separation => "separation",
        }
    }
}

impl std::str::FromStr for PinReason {
    type Err = Error;

    fn from_str(s: &str) -> Result<PinReason, Error> {
        match s {
            "continuity" => Ok(PinReason::Continuity),
            "relationship" => Ok(PinReason::Relationship),
            "separation" => Ok(PinReason::Separation),
            other => Err(Error::BadPin(format!(
                "{other:?} is no reason; one of continuity, relationship, separation"
            ))),
        }
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
    /// Whether the task is content from authors outside the fleet: a tap filed it, or a
    /// session that had read such content filed it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub outside: bool,
    /// The task this one was split from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// What triage scored it, beside the one number the queue is ordered by.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scores: Option<Scores>,
    /// The decisions this task carries out: entries of the record.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub implements: Vec<String>,
    /// Whether what the task does can be undone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reversibility: Option<Reversibility>,
    /// The owned paths the task changes, as globs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts_touched: Vec<String>,
    /// The policy in force when the task was filed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub policy_context: Vec<String>,
    /// The position the task is pinned to: only a claimant that fills it claims the
    /// task. Absent when any matching claimant may.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<String>,
    /// Why it is pinned. Present exactly when `position` is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin_reason: Option<PinReason>,
}

/// What triage scored a task: the parts its rank is made of.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scores {
    /// What it is worth done.
    #[serde(default)]
    pub value: i64,
    /// How soon it loses that worth.
    #[serde(default)]
    pub urgency: i64,
    /// What it costs to do.
    #[serde(default)]
    pub effort: i64,
    /// The tasks that wait on it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unblocks: Vec<String>,
}

/// Whether what a task does can be undone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Reversibility {
    /// It can.
    Reversible,
    /// It cannot.
    Irreversible,
}

impl std::str::FromStr for Reversibility {
    type Err = String;

    fn from_str(s: &str) -> Result<Reversibility, String> {
        match s {
            "reversible" => Ok(Reversibility::Reversible),
            "irreversible" => Ok(Reversibility::Irreversible),
            other => Err(format!("{other:?} is not reversible or irreversible")),
        }
    }
}

impl std::str::FromStr for Outcome {
    type Err = String;

    fn from_str(s: &str) -> Result<Outcome, String> {
        match s {
            "applied" => Ok(Outcome::Applied),
            "not_applied" => Ok(Outcome::NotApplied),
            other => Err(format!("{other:?} is not applied or not_applied")),
        }
    }
}

/// How an external effect turned out, as whoever reconciled it found the destination.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The destination holds the effect.
    Applied,
    /// The destination does not hold it, so the action may be made again.
    NotApplied,
}

/// The fields of a task that the ledger keeps and never queries by, as one JSON column.
#[derive(Default, Serialize, Deserialize)]
struct Extra {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    definition: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    acceptance: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    outside: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scores: Option<Scores>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    implements: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reversibility: Option<Reversibility>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    artifacts_touched: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    policy_context: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    position: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pin_reason: Option<PinReason>,
    /// The outside event the task waits on. Not of the spec: events set and clear it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    waits_on: Option<String>,
    /// The external actions begun under a claim and not yet settled, by action id.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    effects: Vec<String>,
}

impl Extra {
    fn parse(id: &str, text: Option<&str>) -> Result<Extra, Error> {
        match text {
            // a row from before the column has none, and reads as a task with nothing more
            None | Some("") => Ok(Extra::default()),
            Some(text) => serde_json::from_str(text).map_err(|_| Error::BadRow {
                id: id.to_string(),
                what: format!("extra {text:?}"),
            }),
        }
    }
}

/// The `extra` column of task `id` after `event`, when the event changes it: the event
/// a task waits on, and the effects not settled. Both backends project through this.
pub(crate) fn extra_after(
    id: &str,
    extra: Option<&str>,
    event: &Event,
) -> Result<Option<String>, Error> {
    let mut e = Extra::parse(id, extra)?;
    match event {
        Event::Waiting { on } => e.waits_on = Some(on.clone()),
        Event::Arrived { .. } => e.waits_on = None,
        Event::EffectBegun { action } => {
            if !e.effects.contains(action) {
                e.effects.push(action.clone());
            }
        }
        Event::EffectSettled { action, .. } => e.effects.retain(|a| a != action),
        _ => return Ok(None),
    }
    serde_json::to_string(&e)
        .map(Some)
        .map_err(|err| Error::BadRow {
            id: id.to_string(),
            what: format!("extra serialises: {err}"),
        })
}

impl TaskSpec {
    /// The `extra` column of this spec's row.
    pub(crate) fn extra(&self) -> String {
        let extra = Extra {
            definition: self.definition.clone(),
            acceptance: self.acceptance.clone(),
            outside: self.outside,
            parent: self.parent.clone(),
            scores: self.scores.clone(),
            implements: self.implements.clone(),
            reversibility: self.reversibility,
            artifacts_touched: self.artifacts_touched.clone(),
            policy_context: self.policy_context.clone(),
            position: self.position.clone(),
            pin_reason: self.pin_reason,
            waits_on: None,
            effects: Vec::new(),
        };
        serde_json::to_string(&extra).unwrap_or_default()
    }

    /// Refuse half a pin: a position and a reason come together, and a position has a
    /// name.
    fn check_pin(&self) -> Result<(), Error> {
        match (self.position.as_deref(), self.pin_reason) {
            (None, None) => Ok(()),
            (Some(""), _) => Err(Error::BadPin("the position has no name".into())),
            (Some(_), Some(_)) => Ok(()),
            (Some(p), None) => Err(Error::BadPin(format!("position {p} has no reason"))),
            (None, Some(r)) => Err(Error::BadPin(format!(
                "the reason {} has no position",
                r.as_str()
            ))),
        }
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
    /// The outside event it waits on: a check that runs, a reply, a change that has not
    /// landed. Nobody may claim it until that event arrives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_on: Option<String>,
    /// The external actions begun under a claim of it whose outcome nobody has recorded.
    /// Nobody may claim it while there is one: a lease that ran out does not say whether
    /// the destination holds the effect.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effects: Vec<String>,
}

impl Task {
    /// Why nobody may claim this task whatever its lease says, when something holds it
    /// back: the event it waits on, or an effect nobody has settled.
    #[must_use]
    pub fn held_back(&self) -> Option<String> {
        if let Some(on) = &self.waiting_on {
            return Some(format!("waits on {on}"));
        }
        self.effects
            .first()
            .map(|a| format!("effect {a} is not settled"))
    }
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

/// The position a claimant fills, and what its charter covers.
#[derive(Clone, Copy, Debug)]
pub struct Seat<'a> {
    /// The position's name.
    pub position: &'a str,
    /// The names of the task definitions the position's charter covers.
    pub covers: &'a [String],
}

/// Who claims, and what the ledger matches a task against.
#[derive(Clone, Copy, Debug)]
pub struct Claimer<'a> {
    /// Who holds the task once it is claimed.
    pub holder: &'a str,
    /// The capabilities the claimant carries: a task's requirements must all be here.
    pub manifest: &'a [String],
    /// The highest classification the claimant handles.
    pub handling: Label,
    /// The position the claimant fills, when it fills one.
    pub seat: Option<Seat<'a>>,
    /// The attempt key the claimant gave: a retry of the same claim carries the same
    /// key, so it is the same claim and not a second one.
    pub attempt: Option<&'a str>,
}

impl Claimer<'_> {
    /// The event of a claim this claimant makes: the claim of `generation`, under a lease
    /// until `until`, from the position it fills.
    pub(crate) fn claimed(&self, until: OffsetDateTime, generation: u64) -> Event {
        Event::Claimed {
            holder: self.holder.to_string(),
            until,
            generation,
            attempt: self.attempt.map(str::to_string),
            position: self.seat.map(|s| s.position.to_string()),
        }
    }
}

/// What a backend's `claim_next` did: claimed a task, or found the claim the caller's
/// attempt key made before, with its generation.
pub(crate) enum Taken {
    New(String),
    Again { id: String, generation: u64 },
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

/// What an approval records: a person approved one action on one version of one entry,
/// for one principal, until a time. It holds no free text, so an erasure of the entry
/// has nothing to remove from it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalSpec {
    /// The domain the entry lives in, whose ledger keeps the approval.
    pub domain: String,
    /// The person who approved, system-qualified.
    pub principal: String,
    /// The action approved, as one token: `erase`, `declassify:internal`. The ledger
    /// does not interpret it.
    pub action: String,
    /// The entry the action is on.
    pub target: String,
    /// The digest of the entry text that was approved: the approval is of that version
    /// and of no other.
    pub version: String,
    /// The principal that may run the action under this approval.
    pub scope: String,
    /// When the approval stops permitting anything.
    #[serde(with = "time::serde::rfc3339")]
    pub expires: OffsetDateTime,
}

/// An approval as the ledger holds it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRecord {
    /// The approval's id, `apr-<domain>-<suffix>`.
    pub id: String,
    /// What was approved.
    #[serde(flatten)]
    pub spec: ApprovalSpec,
    /// When it was recorded, on the ledger's clock.
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
}

impl ApprovalRecord {
    /// Whether the approval permits nothing any more at `now`: at its expiry and after
    /// it. The two are compared as times, since the text of a time does not order as the
    /// time does.
    #[must_use]
    pub fn expired(&self, now: OffsetDateTime) -> bool {
        now >= self.spec.expires
    }
}

type Clock = Box<dyn Fn() -> OffsetDateTime + Send + Sync>;

/// One appended event, as replay reads it.
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum Event {
    Created {
        id: String,
        /// Boxed: a spec is several times the size of any other event.
        spec: Box<TaskSpec>,
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
        /// The generation this claim got. Absent from a claim of an older ledger.
        #[serde(default)]
        generation: u64,
        /// The attempt key the caller gave, when it gave one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attempt: Option<String>,
        /// The position the claimant filled, when it filled one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        position: Option<String>,
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
        /// The attempt key the caller gave, when it gave one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attempt: Option<String>,
    },
    Invalidated {
        reason: String,
        superseded_by: Option<String>,
    },
    /// The holder gave the task up until an outside event comes: nobody's to claim
    /// until [`Event::Arrived`].
    Waiting {
        on: String,
    },
    /// The outside event a task waited on came.
    Arrived {
        event: String,
    },
    /// The holder is about to make an external action. Until [`Event::EffectSettled`]
    /// names the same action, nobody else may claim the task.
    EffectBegun {
        action: String,
    },
    /// Someone looked at the destination and recorded what became of an action.
    EffectSettled {
        action: String,
        outcome: Outcome,
        by: String,
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
    /// The pin was taken off a task, so the task is back in matching: the position it
    /// was pinned to, and why. The why is a sentence the caller builds, not a person's
    /// text: an erasure does not rewrite it.
    Unpinned {
        position: String,
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
    /// A person approved one action on one version of one entry. Appended under
    /// [`approval_key`], never a task id, and projected nowhere: the event is the record.
    /// Boxed: a record is larger than every other event but a boxed one.
    Approved(Box<ApprovalRecord>),
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
            Event::Waiting { .. } => "waiting",
            Event::Arrived { .. } => "arrived",
            Event::EffectBegun { .. } => "effect_begun",
            Event::EffectSettled { .. } => "effect_settled",
            Event::Posted { .. } => "posted",
            Event::Rescored { .. } => "rescored",
            Event::Unpinned { .. } => "unpinned",
            Event::Promoted { .. } => "promoted",
            Event::Approved(_) => "approved",
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

/// One event of the ledger as the log of its domain takes it: which task, what happened
/// to it, who did it and under which attempt key. It has no field for a title, a body, a
/// handoff, a reason or a why, so no text of a task or of a promotion leaves the ledger
/// in it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Logged {
    /// Its place among the ledger's events: the order the ledger appended them in.
    pub seq: u64,
    /// The task, or the key that a promotion or an approval of an entry is appended
    /// under.
    pub task: String,
    /// What happened: `created`, `claimed`, `completed` and the rest, as the ledger's
    /// `kind` column names them.
    pub op: String,
    /// When, as the ledger wrote it.
    pub at: String,
    /// Who did it: the filer of a `created`, the holder that a `claimed`, a
    /// `claim_refused` or a `late_handoff` names, the promoter of a `promoted`, the
    /// approver of an `approved`, and for a
    /// `heartbeat`, a `released`, a `handoff_left`, a `waiting`, an `effect_begun` and a
    /// `completed` the holder whose lease was live; for an `effect_settled`, who settled
    /// it. Absent for the others, and for a `completed` that the task's
    /// continuation caused, which no holder did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal: Option<String>,
    /// The attempt key a `claimed` or a `completed` was made under.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<String>,
    /// The classification of the task, or of the promotion. An approval records none,
    /// and is given at the highest label.
    pub classification: Label,
}

/// The kinds of event that say who holds a task: a backend gives these for a task from
/// before the number asked for too, so the holder of a later event can be named.
pub(crate) const LEASE_KINDS: &str = "'claimed', 'heartbeat', 'released', 'waiting'";

/// A row of the events table, with what the task's row says of the task.
pub(crate) struct RawEvent {
    pub(crate) seq: i64,
    pub(crate) task: String,
    pub(crate) at: String,
    pub(crate) payload: String,
    /// The domain of the task's row. None for an event of no task: a promotion or an
    /// approval.
    pub(crate) domain: Option<String>,
    /// The label rank of the task's row.
    pub(crate) label_rank: Option<i64>,
}

/// The claim in force on a task, as the events read so far say.
struct Lease {
    holder: String,
    until: OffsetDateTime,
}

/// Who caused `event`, and the attempt key it was made under. `lease` is the claim in
/// force on the task before the event, `at` the event's time. An event only the holder
/// may cause names the holder when the lease was live at that time, which is what the
/// ledger checked as it appended the event; a time that does not parse names the holder.
fn who(
    event: &Event,
    lease: Option<&Lease>,
    at: Option<OffsetDateTime>,
) -> (Option<String>, Option<String>) {
    let holder = || {
        lease
            .filter(|l| at.is_none_or(|at| l.until >= at))
            .map(|l| l.holder.clone())
    };
    match event {
        Event::Created { spec, .. } => (
            Some(spec.created_by.clone()).filter(|by| !by.is_empty()),
            None,
        ),
        Event::Claimed {
            holder, attempt, ..
        } => (Some(holder.clone()), attempt.clone()),
        Event::ClaimRefused { holder, .. } | Event::LateHandoff { holder, .. } => {
            (Some(holder.clone()), None)
        }
        Event::Promoted { spec, .. } => (Some(spec.by.clone()), None),
        // the person who approved, as the event records it; never the one it is for
        Event::Approved(record) => (Some(record.spec.principal.clone()), None),
        Event::Heartbeat { .. }
        | Event::Released
        | Event::HandoffLeft { .. }
        | Event::Waiting { .. }
        | Event::EffectBegun { .. } => (holder(), None),
        // the one who settled it is on the event: the holder, or who reconciled after
        Event::EffectSettled { by, .. } => (Some(by.clone()), None),
        Event::Completed { attempt, .. } => (holder(), attempt.clone()),
        Event::Admitted
        | Event::Blocked { .. }
        | Event::Unblocked { .. }
        | Event::Retriaged { .. }
        | Event::Invalidated { .. }
        | Event::Posted { .. }
        | Event::Arrived { .. }
        | Event::Rescored { .. }
        // the event names the position the pin was on, and no person
        | Event::Unpinned { .. } => (None, None),
    }
}

/// The events of `domain` after sequence number `after`, from `rows` in the ledger's
/// order. The rows hold the earlier events of [`LEASE_KINDS`] too: they say who held
/// each task, and none of them is given back.
fn logged(rows: Vec<RawEvent>, domain: &str, after: u64) -> Result<Vec<Logged>, Error> {
    let mut held: std::collections::HashMap<String, Lease> = std::collections::HashMap::new();
    let mut out = Vec::new();
    for row in rows {
        let event = Event::parse(&row.task, &row.payload)?;
        let at = OffsetDateTime::parse(&row.at, &Rfc3339).ok();
        let (principal, attempt) = who(&event, held.get(&row.task), at);
        let (own_domain, own_label) = match &event {
            Event::Claimed { holder, until, .. } => {
                let lease = Lease {
                    holder: holder.clone(),
                    until: *until,
                };
                held.insert(row.task.clone(), lease);
                (None, None)
            }
            Event::Heartbeat { until } => {
                if let Some(lease) = held.get_mut(&row.task) {
                    lease.until = *until;
                }
                (None, None)
            }
            // a holder that waits for an outside event gives the task up, as a release does
            Event::Released | Event::Waiting { .. } => {
                held.remove(&row.task);
                (None, None)
            }
            // a created event and a promoted one name their own domain and label
            Event::Created { spec, .. } => (Some(spec.domain.as_str()), Some(spec.classification)),
            Event::Promoted { spec, .. } => (Some(spec.domain.as_str()), Some(spec.classification)),
            // an approval names its own domain. It records no label and has no task row,
            // so it is at the highest label, as an event of a task whose row is gone
            Event::Approved(record) => (Some(record.spec.domain.as_str()), None),
            _ => (None, None),
        };
        let seq = u64::try_from(row.seq).unwrap_or(0);
        if seq <= after || own_domain.or(row.domain.as_deref()) != Some(domain) {
            continue;
        }
        let classification = match (own_label, row.label_rank) {
            (Some(label), _) => label,
            (None, Some(rank)) => label_from_rank(&row.task, rank)?,
            // a task whose row is gone: nothing says what it is, so it is the highest
            (None, None) => Label::Restricted,
        };
        out.push(Logged {
            seq,
            task: row.task,
            op: event.kind().to_string(),
            at: row.at,
            principal,
            attempt,
            classification,
        });
    }
    Ok(out)
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
        Event::Completed { handoff, attempt } if handoff != redaction.reason => Event::Completed {
            handoff: redaction.reason.clone(),
            attempt,
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

/// The key an approval's event is appended under. A task id starts `task-`, so the two
/// never meet.
fn approval_key(target: &str) -> String {
    format!("approval:{target}")
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
        let extra = Extra::parse(&id, self.extra.as_deref())?;
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
                outside: extra.outside,
                parent: extra.parent,
                scores: extra.scores,
                implements: extra.implements,
                reversibility: extra.reversibility,
                artifacts_touched: extra.artifacts_touched,
                policy_context: extra.policy_context,
                position: extra.position,
                pin_reason: extra.pin_reason,
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
            waiting_on: extra.waits_on,
            effects: extra.effects,
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

/// The task whose work `task` is under another name: the one it continues, or the one
/// it retries. An effect of that task that nobody has settled holds this one too.
pub(crate) fn resumed(task: &Task) -> Option<&str> {
    let origin = task.spec.origin.as_deref()?;
    origin
        .strip_prefix("continues:")
        .or_else(|| origin.strip_prefix("retries:"))
}

/// The task a filing with this origin is filed from: the one it continues, retries, or
/// is a part of.
fn filed_from_of(origin: Option<&str>) -> Option<&str> {
    let origin = origin?;
    ["continues:", "retries:", "task:"]
        .iter()
        .find_map(|p| origin.strip_prefix(p))
}

/// Whether a claimant in `seat` takes a task filed as `spec`, as far as positions go. A
/// pinned task goes only to a claimant that fills its position. An unpinned task goes to
/// a claimant that fills no position, and to one that fills a position whose charter
/// covers the task's definition: the name before the `@` of `<name>@<version>`.
fn seat_takes(spec: &TaskSpec, seat: Option<Seat<'_>>) -> bool {
    match (spec.position.as_deref(), seat) {
        (Some(pinned), Some(seat)) => seat.position == pinned,
        (Some(_), None) => false,
        (None, None) => true,
        (None, Some(seat)) => spec.definition.as_deref().is_some_and(|d| {
            let name = d.split_once('@').map_or(d, |(name, _)| name);
            seat.covers.iter().any(|c| c == name)
        }),
    }
}

/// Whether what `who` carries lets it hold `task`, whatever state the task is in: its
/// manifest carries every requirement, its handling covers the task's classification,
/// and its seat takes the task.
fn matches_task(task: &Task, who: Claimer<'_>) -> bool {
    task.spec.classification <= who.handling
        && task
            .spec
            .requirements
            .iter()
            .all(|r| who.manifest.contains(r))
        && seat_takes(&task.spec, who.seat)
}

/// Whether `who` may claim `task` now: it is admitted, or claimed under a lease that has
/// run out; nothing holds it back, an event it waits on or an effect nobody has settled;
/// and `who` matches it. Whether its dependencies are complete is the backend's question,
/// asked inside the transaction.
fn claimable(task: &Task, now: OffsetDateTime, who: Claimer<'_>) -> bool {
    let lease_live = task.lease_until.is_some_and(|t| t >= now);
    // a blocked task is a candidate too: the backend asks whether its dependencies are
    // done, so one whose last dependency closed without the event reaching it is not lost
    let state = matches!(task.status, Status::Open | Status::Blocked)
        || (task.status == Status::Claimed && !lease_live);
    state && task.held_back().is_none() && matches_task(task, who)
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

    /// The ledger's clock. A lease and the expiry of an approval are measured against
    /// it, so whoever judges one asks the ledger for the time.
    #[must_use]
    pub fn now(&self) -> OffsetDateTime {
        (self.clock)()
    }

    /// File a task. It is born proposed. If `origin` and `upstream_event` are both set and
    /// the domain already has a task with that pair, that task is returned and nothing is
    /// filed; another domain's task with the same pair is not this domain's.
    ///
    /// A task that names a position is pinned to it, and gives the reason. A continuation
    /// of a pinned task that names no position takes its parent's pin: the work it
    /// resumes is the position's. A subagent task and a retry do not.
    ///
    /// # Errors
    ///
    /// [`Error::BadPin`] for a position without a reason, a reason without a position,
    /// or a position with no name; [`Error::WrongState`], naming the task, when the
    /// origin files this one from a task whose work is held by an effect that nobody has
    /// settled; a database error.
    pub async fn create(&mut self, mut spec: TaskSpec) -> Result<Created, Error> {
        if let (Some(o), Some(u)) = (&spec.origin, &spec.upstream_event) {
            let existing = on_backend!(&self.backend, db => db.find_origin(&spec.domain, o, u))?;
            if let Some(id) = existing {
                return Ok(Created {
                    task: self.get(&id).await?,
                    deduplicated: true,
                });
            }
        }
        spec.check_pin()?;
        // the work of a task with an effect that nobody has settled goes to nobody else,
        // under another task's name either: no continuation, retry or part of it is filed
        if let Some(from) = filed_from_of(spec.origin.as_deref()) {
            if let Some(unsettled) = self.unsettled_above(from).await? {
                return Err(Error::WrongState {
                    id: unsettled.id,
                    status: unsettled.status,
                    op: "hand its work on while an effect of it is not settled",
                });
            }
        }
        self.inherit_pin(&mut spec).await?;
        let id = dm_core::id::new_task_id(&spec.domain);
        let now = self.now();
        let event = Event::Created {
            id: id.clone(),
            spec: Box::new(spec),
        };
        self.append_at(&id, now, &event).await?;
        Ok(Created {
            task: self.get(&id).await?,
            deduplicated: false,
        })
    }

    /// Give a continuation that names no position the pin of the task it continues, when
    /// this ledger holds that task in the same domain and it is pinned.
    async fn inherit_pin(&self, spec: &mut TaskSpec) -> Result<(), Error> {
        let parent = match spec.origin.as_deref().map(|o| o.strip_prefix("continues:")) {
            Some(Some(parent)) if spec.position.is_none() => parent.to_string(),
            _ => return Ok(()),
        };
        match self.get(&parent).await {
            Ok(p) if p.spec.domain == spec.domain => {
                spec.position = p.spec.position;
                spec.pin_reason = p.spec.pin_reason;
                Ok(())
            }
            Ok(_) | Err(Error::NotFound(_)) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// The task whose unsettled effect holds the work of `id`: `id` itself, or a task
    /// that `id` continues or retries, however far up. A task this ledger does not hold
    /// ends the walk.
    async fn unsettled_above(&self, id: &str) -> Result<Option<Task>, Error> {
        let mut seen = std::collections::HashSet::new();
        let mut up = Some(id.to_string());
        while let Some(id) = up.take() {
            if !seen.insert(id.clone()) {
                break;
            }
            let task = match self.get(&id).await {
                Ok(t) => t,
                Err(Error::NotFound(_)) => break,
                Err(e) => return Err(e),
            };
            if !task.effects.is_empty() {
                return Ok(Some(task));
            }
            up = resumed(&task).map(str::to_string);
        }
        Ok(None)
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
        // a task sent back to triage while it waited on an event waits on it still
        if !waits_on.is_empty() || task.waiting_on.is_some() {
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
    /// passed over inside it, so it is never claimed and then given back. The claimant
    /// fills no position: a pinned task is passed over too.
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
        let who = Claimer {
            holder,
            manifest,
            handling,
            seat: None,
            attempt: None,
        };
        self.claim_next_by(domain, who, lease).await
    }

    /// [`Ledger::claim_next`] under an attempt key. A caller that did not hear the answer
    /// asks again with the same key: while the claim that key made in this domain is in
    /// force for this caller, it gets that claim back, and no second task. The key is
    /// looked up in the transaction that claims, so two callers with one key make one
    /// claim.
    ///
    /// # Errors
    ///
    /// [`Error::AttemptSpent`] when the key's claim is over, or is not one this caller
    /// could have made; a database error.
    pub async fn claim_next_once(
        &mut self,
        domain: &str,
        holder: &str,
        lease: Duration,
        (manifest, handling): (&[String], Label),
        attempt: &str,
    ) -> Result<Option<Claim>, Error> {
        let who = Claimer {
            holder,
            manifest,
            handling,
            seat: None,
            attempt: Some(attempt),
        };
        self.claim_next_by(domain, who, lease).await
    }

    /// The claim an attempt key made, if it is still in force and is one this caller
    /// could hold: its holder, its generation and a live lease, within the caller's
    /// handling and manifest, and from a seat that takes the task. Anything else is a
    /// key that is spent.
    async fn same_claim(
        &self,
        (id, generation): (&str, u64),
        who: Claimer<'_>,
    ) -> Result<Claim, Error> {
        let task = self.get(id).await?;
        let now = self.now();
        let in_force = task.status == Status::Claimed
            && task.generation == generation
            && task.holder.as_deref() == Some(who.holder)
            && task.lease_until.is_some_and(|t| t >= now)
            && matches_task(&task, who);
        match (in_force, task.lease_until) {
            (true, Some(lease_until)) => Ok(Claim {
                generation: task.generation,
                task,
                lease_until,
            }),
            _ => Err(Error::AttemptSpent {
                key: who.attempt.unwrap_or_default().to_string(),
            }),
        }
    }

    /// [`Ledger::claim_next`] by a claimant that says all of what it is: its manifest,
    /// its handling, the position it fills and its attempt key. A claimant that fills a
    /// position takes the tasks pinned to it, and the unpinned tasks whose definition the
    /// position's charter covers. With an attempt key, as [`Ledger::claim_next_once`].
    /// The claim records the position it was made from.
    ///
    /// # Errors
    ///
    /// [`Error::AttemptSpent`] when the key's claim is over, or is not one this claimant
    /// could have made; a database error.
    pub async fn claim_next_by(
        &mut self,
        domain: &str,
        who: Claimer<'_>,
        lease: Duration,
    ) -> Result<Option<Claim>, Error> {
        let now = self.now();
        let until = now + lease;
        let taken = on_backend!(&mut self.backend, db => db.claim_next(domain, now, who, until))?;
        match taken {
            Some(Taken::New(id)) => {
                self.trace("claimed", &id).await;
                let task = self.get(&id).await?;
                Ok(Some(Claim {
                    generation: task.generation,
                    task,
                    lease_until: until,
                }))
            }
            Some(Taken::Again { id, generation }) => {
                self.same_claim((&id, generation), who).await.map(Some)
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
    /// `handling`; [`Error::Pinned`] when it is pinned.
    pub async fn claim(
        &mut self,
        id: &str,
        holder: &str,
        lease: Duration,
        manifest: &[String],
        handling: Label,
    ) -> Result<Claim, Error> {
        let who = Claimer {
            holder,
            manifest,
            handling,
            seat: None,
            attempt: None,
        };
        self.claim_by(id, who, lease).await
    }

    /// Claim one named task, under the same rules as [`Ledger::claim_next_by`]. The
    /// attempt key is recorded with the claim and is not looked up.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`]; [`Error::Pinned`] when the task is admitted and pinned to a
    /// position the claimant does not fill; [`Error::WrongState`] when it is not
    /// claimable for another reason.
    pub async fn claim_by(
        &mut self,
        id: &str,
        who: Claimer<'_>,
        lease: Duration,
    ) -> Result<Claim, Error> {
        let now = self.now();
        let until = now + lease;
        let claimed = on_backend!(&mut self.backend, db => db.claim(id, now, who, until));
        if matches!(claimed, Err(Error::WrongState { .. })) {
            if let Some(position) = self.record_refusal(id, who, now).await? {
                return Err(Error::Pinned {
                    id: id.to_string(),
                    position,
                });
            }
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
    /// Returns the position, when the pin is why: the task is admitted and pinned to a
    /// position the claimant does not fill.
    async fn record_refusal(
        &mut self,
        id: &str,
        who: Claimer<'_>,
        now: OffsetDateTime,
    ) -> Result<Option<String>, Error> {
        let task = self.get(id).await?;
        if task.status.is_terminal() {
            return Ok(None);
        }
        let lease_live = task.lease_until.is_some_and(|t| t >= now);
        let pinned_away =
            task.spec.position.clone().filter(|p| {
                task.status != Status::Proposed && who.seat.map(|s| s.position) != Some(p)
            });
        let why = if let Some(position) = &pinned_away {
            format!("the task is pinned to position {position}, and the session does not fill it")
        } else if task.status == Status::Claimed && lease_live {
            format!(
                "the lease of the claim of generation {} was live",
                task.generation
            )
        } else if task.status == Status::Proposed {
            "the task was not admitted".to_string()
        } else if let Some(held_back) = self.why_held_back(&task).await? {
            format!(
                "after the claim of generation {}, {held_back}",
                task.generation
            )
        } else {
            // claimable by someone, and still not this session's to take
            format!(
                "after the claim of generation {}, the session did not meet the task's requirements or its handling, or a task it depends on was not done",
                task.generation
            )
        };
        let last = self.refused_claims(id).await?.pop();
        if last.as_ref() != Some(&(who.holder.to_string(), why.clone())) {
            let refused = Event::ClaimRefused {
                holder: who.holder.to_string(),
                why,
            };
            self.append_at(id, now, &refused).await?;
        }
        Ok(pinned_away)
    }

    /// Take the pin off a task, so it returns to matching: any claimant that matches it
    /// may claim it. `why` is a sentence the caller builds and never a person's own
    /// text: it is kept in the log, where an erasure does not reach it.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`]; [`Error::WrongState`] when the task is terminal or is not
    /// pinned.
    pub async fn unpin(&mut self, id: &str, why: &str) -> Result<Task, Error> {
        let task = self.get(id).await?;
        let position = match task.spec.position {
            Some(position) if !task.status.is_terminal() => position,
            _ => {
                return Err(Error::WrongState {
                    id: id.to_string(),
                    status: task.status,
                    op: "unpin",
                })
            }
        };
        let unpinned = Event::Unpinned {
            position,
            why: why.to_string(),
        };
        self.append(id, &unpinned).await?;
        self.get(id).await
    }

    /// The tasks of a domain that are pinned to a position and not closed: proposed,
    /// open, blocked or claimed, ranked as the queue ranks them.
    ///
    /// # Errors
    ///
    /// A database error, or a row the ledger did not write.
    pub async fn pinned_to(&self, domain: &str, position: &str) -> Result<Vec<Task>, Error> {
        let mut out = Vec::new();
        for status in [
            Status::Proposed,
            Status::Open,
            Status::Blocked,
            Status::Claimed,
        ] {
            let tasks = self.list(domain, status).await?;
            out.extend(
                tasks
                    .into_iter()
                    .filter(|t| t.spec.position.as_deref() == Some(position)),
            );
        }
        in_queue_order(&mut out);
        Ok(out)
    }

    /// Who fills a position in a domain now, as far as the ledger sees: the holders of
    /// the tasks claimed from that position under a lease that is live. Sorted, each
    /// once. A session that fills the position and holds no task is not seen.
    ///
    /// # Errors
    ///
    /// A database error, or an event the ledger did not write.
    pub async fn seated(&self, domain: &str, position: &str) -> Result<Vec<String>, Error> {
        let held = self.held_from(domain, position).await?;
        let mut out: Vec<String> = held.into_iter().filter_map(|t| t.holder).collect();
        out.sort();
        out.dedup();
        Ok(out)
    }

    /// The tasks of a domain that a session claimed from a position and holds under a
    /// lease that is live: each names its holder. A caller that shows who is in the
    /// position shows the holders of the tasks its reader may see.
    ///
    /// # Errors
    ///
    /// A database error, or an event the ledger did not write.
    pub async fn held_from(&self, domain: &str, position: &str) -> Result<Vec<Task>, Error> {
        let now = self.now();
        let mut out = Vec::new();
        for task in self.list(domain, Status::Claimed).await? {
            let live = task.lease_until.is_some_and(|t| t >= now);
            if !live || task.holder.is_none() {
                continue;
            }
            let claims = on_backend!(&self.backend, db => db.payloads_of(&task.id, "claimed"))?;
            let from = match claims
                .last()
                .map(|p| Event::parse(&task.id, p))
                .transpose()?
            {
                Some(Event::Claimed { position, .. }) => position,
                _ => None,
            };
            if from.as_deref() == Some(position) {
                out.push(task);
            }
        }
        Ok(out)
    }

    /// What holds `task` back from every claim, in words for its record: the event it
    /// waits on, an effect of it that nobody has settled, or such an effect of the task
    /// whose work it resumes.
    async fn why_held_back(&self, task: &Task) -> Result<Option<String>, Error> {
        if let Some(on) = &task.waiting_on {
            return Ok(Some(format!("the task waited on {on}")));
        }
        if let Some(action) = task.effects.first() {
            return Ok(Some(format!("effect {action} of the task was not settled")));
        }
        let Some(up) = resumed(task) else {
            return Ok(None);
        };
        Ok(self.unsettled_above(up).await?.map(|t| {
            format!(
                "effect {} of {}, whose work the task resumes, was not settled",
                t.effects.first().map_or("", String::as_str),
                t.id
            )
        }))
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
        self.complete_keyed(id, by, handoff, None).await
    }

    async fn complete_keyed(
        &mut self,
        id: &str,
        by: Held<'_>,
        handoff: &str,
        attempt: Option<&str>,
    ) -> Result<Task, Error> {
        let now = self.now();
        let task = self.get(id).await?;
        if task.status != Status::Claimed {
            return Err(Error::WrongState {
                id: id.to_string(),
                status: task.status,
                op: "complete",
            });
        }
        Self::effects_settled(&task)?;
        let completed = Event::Completed {
            handoff: handoff.to_string(),
            attempt: attempt.map(str::to_string),
        };
        self.append_held(id, now, by, &completed).await?;
        self.unblock_dependents(id).await?;
        self.get(id).await
    }

    /// [`Ledger::complete`] under an attempt key. A caller that did not hear the answer
    /// asks again with the same key and the same handoff, and gets the completed task
    /// back: the task is completed once.
    ///
    /// # Errors
    ///
    /// As [`Ledger::complete`]; [`Error::AttemptSpent`] when the key completed this task
    /// with another handoff.
    pub async fn complete_once(
        &mut self,
        id: &str,
        by: Held<'_>,
        handoff: &str,
        attempt: &str,
    ) -> Result<Task, Error> {
        let before = on_backend!(&self.backend, db => db.completed_under(id, attempt))?;
        match before {
            Some(with) if with == handoff => self.get(id).await,
            Some(_) => Err(Error::AttemptSpent {
                key: attempt.to_string(),
            }),
            None => self.complete_keyed(id, by, handoff, Some(attempt)).await,
        }
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
        Self::effects_settled(&task)?;
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
                attempt: None,
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

    /// A task with an effect nobody has settled is not done, whoever says so: whether
    /// the destination holds the effect is part of what done means.
    fn effects_settled(task: &Task) -> Result<(), Error> {
        if task.effects.is_empty() {
            return Ok(());
        }
        Err(Error::WrongState {
            id: task.id.clone(),
            status: task.status,
            op: "complete while an effect of it is not settled",
        })
    }

    /// Give a task up until an outside event comes: a check that runs, a reply, a change
    /// that has not landed. The task is blocked and nobody's to claim until
    /// [`Ledger::arrived`] is told of `on`; the holder's session can end meanwhile. A
    /// handoff says where the next holder starts.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`], [`Error::NotHolder`], [`Error::Fenced`], or
    /// [`Error::WrongState`].
    pub async fn wait_for(
        &mut self,
        id: &str,
        by: Held<'_>,
        on: &str,
        handoff: Option<&str>,
    ) -> Result<Task, Error> {
        let now = self.now();
        let task = self.get(id).await?;
        if task.status != Status::Claimed {
            return Err(Error::WrongState {
                id: id.to_string(),
                status: task.status,
                op: "wait",
            });
        }
        if let Some(handoff) = handoff {
            let left = Event::HandoffLeft {
                handoff: handoff.to_string(),
            };
            self.append_held(id, now, by, &left).await?;
        }
        let waiting = Event::Waiting { on: on.to_string() };
        self.append_held(id, now, by, &waiting).await?;
        self.get(id).await
    }

    /// An outside event came: every task of the domain that waited on it waits on it no
    /// more, and is open again once its dependencies are done. The tasks that waited.
    ///
    /// # Errors
    ///
    /// A database error.
    pub async fn arrived(&mut self, domain: &str, event: &str) -> Result<Vec<Task>, Error> {
        let mut woken = Vec::new();
        // whatever its state says: a task sent back to triage while it waited waits still
        for status in [Status::Blocked, Status::Proposed, Status::Open] {
            for t in self.list(domain, status).await? {
                if t.waiting_on.as_deref() != Some(event) {
                    continue;
                }
                let came = Event::Arrived {
                    event: event.to_string(),
                };
                self.append(&t.id, &came).await?;
                if status == Status::Blocked && self.not_done(&t.spec.depends_on).await?.is_empty()
                {
                    let by = event.to_string();
                    self.append(&t.id, &Event::Unblocked { by }).await?;
                }
                woken.push(self.get(&t.id).await?);
            }
        }
        Ok(woken)
    }

    /// Record that the holder is about to make an external action, before it makes it.
    /// From here until [`Ledger::effect_settled`] names the action, nobody else may
    /// claim the task: a lease that runs out does not say whether the destination holds
    /// the effect. A retry of the action carries the same id, and is the same effect.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`], [`Error::NotHolder`], or [`Error::Fenced`]: the claim is
    /// checked in the transaction that records the effect.
    pub async fn effect_begun(
        &mut self,
        id: &str,
        by: Held<'_>,
        action: &str,
    ) -> Result<Task, Error> {
        let now = self.now();
        let begun = Event::EffectBegun {
            action: action.to_string(),
        };
        self.append_held(id, now, by, &begun).await?;
        self.get(id).await
    }

    /// Record what became of an external action, as `by` found the destination. With
    /// `held`, it is the holder that says so, and the claim is checked in the
    /// transaction that records it. Without, it is someone who reconciles after the
    /// claim: whether `by` may is the caller's rule, and the ledger records who did.
    /// Asked again for an action already settled with the same outcome, it changes
    /// nothing. With the last effect settled, a task whose continuation finished
    /// meanwhile closes, as [`Ledger::close_resumed`] closes it.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`]; [`Error::WrongState`] for an action the task never began,
    /// or one already settled with the other outcome; with `held`,
    /// [`Error::NotHolder`] or [`Error::Fenced`].
    pub async fn effect_settled(
        &mut self,
        id: &str,
        action: &str,
        outcome: Outcome,
        (by, held): (&str, Option<Held<'_>>),
    ) -> Result<Task, Error> {
        let task = self.get(id).await?;
        if !task.effects.iter().any(|a| a == action) {
            return self.settled_before(task, action, outcome).await;
        }
        let settled = Event::EffectSettled {
            action: action.to_string(),
            outcome,
            by: by.to_string(),
        };
        match held {
            Some(held) => {
                let now = self.now();
                self.append_held(id, now, held, &settled).await?;
            }
            None => self.append(id, &settled).await?,
        }
        self.close_after_settlement(id).await?;
        self.get(id).await
    }

    /// The answer for an action that is not pending. Settled before with this outcome,
    /// it is the same settlement, and the task comes back as it is. Settled with the
    /// other outcome, the record stays as it is and the caller is told: an answer of
    /// "taken" would say that the record now holds what it does not.
    async fn settled_before(
        &self,
        task: Task,
        action: &str,
        outcome: Outcome,
    ) -> Result<Task, Error> {
        let op = match self.settlement(&task.id, action).await? {
            Some((was, _)) if was == outcome => return Ok(task),
            Some(_) => "settle with another outcome an effect that is settled",
            None => "settle an effect the task did not begin",
        };
        Err(Error::WrongState {
            id: task.id,
            status: task.status,
            op,
        })
    }

    /// A completion of a continuation leaves the task it resumed open while an effect of
    /// that task is not settled. With the last effect settled, the task closes with the
    /// continuation that finished it. Otherwise its work would be on offer a second time.
    async fn close_after_settlement(&mut self, id: &str) -> Result<(), Error> {
        let task = self.get(id).await?;
        if !task.effects.is_empty() || task.status.is_terminal() {
            return Ok(());
        }
        let marker = continues(id);
        let finished = self.filed_from(id).await?.into_iter().rev().find(|t| {
            t.status == Status::Done && t.spec.origin.as_deref() == Some(marker.as_str())
        });
        if let Some(continuation) = finished {
            self.close_resumed(&continuation).await;
        }
        Ok(())
    }

    /// A continuation that is done finishes the task it resumed, and that one the task
    /// it resumed in turn: up the chain, with the continuation's handoff, while each is
    /// open, nobody holds it and every effect of it is settled. The chain stops at the
    /// first task that does not close.
    pub async fn close_resumed(&mut self, done: &Task) {
        let Some(handoff) = done.handoff.clone() else {
            return;
        };
        let mut by = done.clone();
        while let Some(parent) = by
            .spec
            .origin
            .as_deref()
            .and_then(|o| o.strip_prefix("continues:"))
            .map(str::to_string)
        {
            let Ok(closed) = self
                .complete_by_continuation(&parent, &by.id, &handoff)
                .await
            else {
                break;
            };
            by = closed;
        }
    }

    /// How an action of a task was settled, and by whom, when it was.
    ///
    /// # Errors
    ///
    /// A database error.
    pub async fn settlement(
        &self,
        id: &str,
        action: &str,
    ) -> Result<Option<(Outcome, String)>, Error> {
        let payloads = on_backend!(&self.backend, db => db.payloads_of(id, "effect_settled"))?;
        let mut last = None;
        for p in payloads {
            if let Event::EffectSettled {
                action: a,
                outcome,
                by,
            } = Event::parse(id, &p)?
            {
                if a == action {
                    last = Some((outcome, by));
                }
            }
        }
        Ok(last)
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

    /// Record an approval: one event, projected nowhere. The ledger gives it an id and
    /// the time of its clock. Nothing is checked and nothing is deduplicated: the same
    /// spec recorded twice is two approvals, each with its own id.
    ///
    /// # Errors
    ///
    /// A database error.
    pub async fn record_approval(&mut self, spec: &ApprovalSpec) -> Result<ApprovalRecord, Error> {
        let record = ApprovalRecord {
            id: dm_core::id::new_task_id(&spec.domain).replacen("task-", "apr-", 1),
            spec: spec.clone(),
            at: self.now(),
        };
        on_backend!(&mut self.backend, db => db.record_approval(&record))?;
        Ok(record)
    }

    /// The approvals of a domain, oldest first: those of one entry when `target` names
    /// it, else all of them. Expired ones are returned too; [`ApprovalRecord::expired`]
    /// says which.
    ///
    /// # Errors
    ///
    /// A database error, or an event the ledger did not write.
    pub async fn approvals(
        &self,
        domain: &str,
        target: Option<&str>,
    ) -> Result<Vec<ApprovalRecord>, Error> {
        let key = target.map(approval_key);
        let payloads = match &self.backend {
            Backend::Sqlite(db) => db.approval_payloads(key.as_deref())?,
            Backend::Postgres(db) => db.approval_payloads(domain, key.as_deref()).await?,
        };
        let mut out = Vec::new();
        for p in payloads {
            if let Event::Approved(record) = Event::parse(key.as_deref().unwrap_or("approval"), &p)?
            {
                // SQLite keeps every domain's events in one table
                if record.spec.domain == domain {
                    out.push(*record);
                }
            }
        }
        Ok(out)
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

    /// The events of `domain` after sequence number `after`, each once, in the order the
    /// ledger appended them: what the log of the domain copies. `after` is the `seq` of
    /// the last event a caller has, or zero. A domain the ledger has no event of gives
    /// none.
    ///
    /// # Errors
    ///
    /// A database error, or an event the ledger did not write.
    pub async fn events_after(&self, domain: &str, after: u64) -> Result<Vec<Logged>, Error> {
        let rows = on_backend!(&self.backend, db => db.events_after(domain, after))?;
        logged(rows, domain, after)
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
        a_claim_asked_twice_under_one_key_is_one_claim,
        a_key_gives_no_task_above_the_caller_s_handling,
        two_connections_under_one_key_make_one_claim,
        a_completion_asked_twice_under_one_key_is_one_completion,
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
        a_task_that_waits_on_an_event_is_nobody_s_until_the_event_comes,
        an_effect_not_settled_holds_the_task_past_its_lease,
        a_settled_effect_gives_the_task_back_and_fences_who_began_it,
        a_task_keeps_the_rest_of_its_record,
        a_task_sent_back_to_triage_while_it_waits_still_waits,
        no_work_is_filed_from_a_task_whose_effect_is_not_settled,
        a_task_that_resumes_one_with_an_unsettled_effect_is_nobody_s,
        a_task_closes_with_its_done_continuation_when_its_last_effect_is_settled,
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
        an_approval_is_one_event_and_is_read_back_whole,
        approvals_are_of_one_target_and_one_domain,
        an_approval_is_no_task_and_a_replay_keeps_it,
        the_event_of_an_approval_names_its_approver_and_its_own_domain,
        half_a_pin_is_refused_and_nothing_is_filed,
        a_pinned_task_goes_only_to_a_claimant_that_fills_its_position,
        a_claim_of_a_pinned_task_from_outside_its_position_is_refused_and_recorded,
        a_claimant_in_a_position_takes_an_unpinned_task_only_when_its_charter_covers_it,
        a_claim_asked_again_under_its_key_from_outside_the_position_is_spent,
        an_unpinned_task_returns_to_matching_and_a_replay_keeps_it_unpinned,
        only_an_open_pinned_task_is_unpinned,
        a_continuation_of_a_pinned_task_takes_its_pin,
        a_position_is_seated_while_a_claim_made_from_it_is_under_a_live_lease,
        the_tasks_pinned_to_a_position_are_its_open_ones_in_queue_order,
        a_pinned_task_that_is_held_back_goes_to_nobody_and_keeps_its_pin,
        an_unpin_leaves_an_effect_that_is_not_settled_and_the_task_stays_held_back,
        the_event_of_an_unpin_names_no_principal,
        events_after_gives_each_event_of_a_domain_once_in_the_ledger_s_order,
        events_after_gives_only_the_events_after_the_number_given,
        the_events_of_a_wait_and_of_an_effect_name_who_caused_them,
        each_event_given_names_its_task_its_kind_its_principal_and_its_attempt_key,
        a_completion_by_a_continuation_names_no_principal,
        a_completion_after_the_lease_ran_out_names_no_principal,
        the_events_given_hold_no_text_of_a_task_or_of_a_promotion,
        a_task_filed_as_outside_keeps_it_and_another_task_does_not,
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

    async fn a_task_filed_as_outside_keeps_it_and_another_task_does_not(b: &Site) {
        let (mut l, _) = b.ledger().await;
        let mut from_outside = spec("From a tap");
        from_outside.outside = true;
        let marked = l.create(from_outside).await.unwrap().task;
        let plain = l.create(spec("From the fleet")).await.unwrap().task;
        for after in ["create", "replay"] {
            assert!(l.get(&marked.id).await.unwrap().spec.outside, "{after}");
            assert!(!l.get(&plain.id).await.unwrap().spec.outside, "{after}");
            let listed = l.list("dom-a", Status::Proposed).await.unwrap();
            let outside: Vec<(&str, bool)> = listed
                .iter()
                .map(|t| (t.id.as_str(), t.spec.outside))
                .collect();
            assert!(outside.contains(&(marked.id.as_str(), true)), "{after}");
            assert!(outside.contains(&(plain.id.as_str(), false)), "{after}");
            l.rebuild().await.unwrap();
        }
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
        assert!(!l.get(&id).await.unwrap().spec.outside);
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

    fn is_spent<T>(r: &Result<T, Error>) -> bool {
        matches!(r, Err(Error::AttemptSpent { .. }))
    }

    async fn next_once(l: &mut Ledger, holder: &str, key: &str) -> Result<Option<Claim>, Error> {
        l.claim_next_once("dom-a", holder, mins(30), (&[], Label::Restricted), key)
            .await
    }

    /// A caller that did not hear the answer asks again with the same key.
    async fn a_claim_asked_twice_under_one_key_is_one_claim(b: &Site) {
        let (mut l, now) = b.ledger().await;
        let first = admitted(&mut l, spec("First")).await;
        // filed a second later, so the queue's order between the two is by time
        *now.lock().unwrap() = T0 + Duration::seconds(1);
        let second = admitted(&mut l, spec("Second")).await;
        let c = next_once(&mut l, "s1", "key-1").await.unwrap().unwrap();
        let again = next_once(&mut l, "s1", "key-1").await.unwrap().unwrap();
        assert_eq!(
            (again.task.id.as_str(), again.generation),
            (c.task.id.as_str(), 1)
        );
        assert_eq!(c.task.id, first.id);
        assert_eq!(
            l.get(&second.id).await.unwrap().status,
            Status::Open,
            "no second task"
        );
        a_key_is_its_holder_s_and_its_domain_s(&mut l, &second).await;
        a_key_whose_claim_is_over_claims_nothing(&mut l, &first, &now).await;
    }

    /// The key gives its claim back only to who could have made it, and the refusal says
    /// nothing of the task.
    async fn a_key_is_its_holder_s_and_its_domain_s(l: &mut Ledger, second: &Task) {
        let other_holder = next_once(l, "s2", "key-1").await;
        assert!(is_spent(&other_holder));
        let said = other_holder
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(!said.contains("task-"), "{said}");
        // a caller whose handling or manifest the task is beyond is no holder of it
        let low = l
            .claim_next_once("dom-a", "s1", mins(30), (&[], Label::Public), "key-1")
            .await;
        assert!(low.is_ok(), "the task is public: {low:?}");
        // the same key in another domain is another key
        let elsewhere = l
            .claim_next_once("dom-b", "s1", mins(30), (&[], Label::Restricted), "key-1")
            .await
            .unwrap();
        assert!(
            elsewhere.is_none(),
            "nothing to claim there, and no claim of dom-a"
        );
        // a new key is a new claim
        let other = next_once(l, "s1", "key-2").await.unwrap().unwrap();
        assert_eq!(other.task.id, second.id);
    }

    async fn a_key_whose_claim_is_over_claims_nothing(
        l: &mut Ledger,
        first: &Task,
        now: &Mutex<OffsetDateTime>,
    ) {
        // the claim the key made is lost, and its holder claims the task again without
        // the key: a live lease of the same name, at the next generation
        *now.lock().unwrap() = T0 + mins(31);
        // a lapsed lease alone ends the claim: the key gets nothing, and no new task
        assert!(is_spent(&next_once(l, "s1", "key-1").await));
        let reclaimed = claim(l, &first.id, "s1", mins(30)).await.unwrap();
        assert_eq!(reclaimed.generation, 2);
        assert!(is_spent(&next_once(l, "s1", "key-1").await));
        // and a rebuild keeps what the key means
        l.rebuild().await.unwrap();
        assert!(is_spent(&next_once(l, "s1", "key-1").await));
    }

    /// A restricted task claimed under a key is not handed to a caller below it.
    async fn a_key_gives_no_task_above_the_caller_s_handling(b: &Site) {
        let (mut l, _) = b.ledger().await;
        let mut secret = spec("Rotate the key");
        secret.classification = Label::Restricted;
        let secret = admitted(&mut l, secret).await;
        let c = next_once(&mut l, "s1", "key-1").await.unwrap().unwrap();
        assert_eq!(c.task.id, secret.id);
        let capped = l
            .claim_next_once("dom-a", "s1", mins(30), (&[], Label::Public), "key-1")
            .await;
        assert!(
            is_spent(&capped),
            "{:?}",
            capped.map(|c| c.map(|c| c.task.id))
        );
        // nor to a caller that lacks what the task requires
        let mut needs = spec("Train the model");
        needs.requirements = vec!["gpu".into()];
        let needs = admitted(&mut l, needs).await;
        let gpu = ["gpu".to_string()];
        let with = l
            .claim_next_once("dom-a", "s1", mins(30), (&gpu, Label::Restricted), "key-2")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(with.task.id, needs.id);
        assert!(is_spent(&next_once(&mut l, "s1", "key-2").await));
    }

    /// Two ledgers over the same tables, one caller asking both with one key at the same
    /// time: one claim, and no second task.
    async fn two_connections_under_one_key_make_one_claim(b: &Site) {
        let (mut l1, _) = b.ledger().await;
        let (mut l2, _) = b.ledger().await;
        for round in 1..=15 {
            admitted(&mut l1, spec("One")).await;
            admitted(&mut l1, spec("Other")).await;
            let key = format!("key-{round}");
            let (c1, c2) = tokio::join!(
                next_once(&mut l1, "s1", &key),
                next_once(&mut l2, "s1", &key),
            );
            let (c1, c2) = (c1.unwrap().unwrap(), c2.unwrap().unwrap());
            assert_eq!(c1.task.id, c2.task.id, "round {round}");
            let held = l1.list("dom-a", Status::Claimed).await.unwrap();
            assert_eq!(held.len(), round, "one claim a round");
        }
    }

    async fn a_completion_asked_twice_under_one_key_is_one_completion(b: &Site) {
        let (mut l, _) = b.ledger().await;
        let t = admitted(&mut l, spec("Once")).await;
        let other = admitted(&mut l, spec("Other")).await;
        claim(&mut l, &t.id, "s1", mins(30)).await.unwrap();
        claim(&mut l, &other.id, "s1", mins(30)).await.unwrap();
        let before = l.event_count().await.unwrap();
        let done = l
            .complete_once(&t.id, held("s1", 1), "mem-a-h", "done-1")
            .await
            .unwrap();
        let again = l
            .complete_once(&t.id, held("s1", 1), "mem-a-h", "done-1")
            .await
            .unwrap();
        assert_eq!(again, done);
        assert_eq!(l.event_count().await.unwrap(), before + 1, "one event");
        a_key_completes_once_and_with_one_handoff(&mut l, &t, &other).await;
    }

    async fn a_key_completes_once_and_with_one_handoff(l: &mut Ledger, t: &Task, other: &Task) {
        // asked again with another handoff, it is not the same completion
        let changed = l
            .complete_once(&t.id, held("s1", 1), "mem-a-other", "done-1")
            .await;
        assert!(is_spent(&changed));
        assert_eq!(
            l.get(&t.id).await.unwrap().handoff.as_deref(),
            Some("mem-a-h")
        );
        // a key is a task's: the same words complete another task as a first attempt
        let elsewhere = l
            .complete_once(&other.id, held("s1", 1), "mem-a-h", "done-1")
            .await
            .unwrap();
        assert_eq!(elsewhere.status, Status::Done);
        // a claim's key is no completion's
        let claimed_under = admitted(l, spec("Third")).await;
        let c = next_once(l, "s1", "both").await.unwrap().unwrap();
        assert_eq!(c.task.id, claimed_under.id);
        l.complete_once(&c.task.id, held("s1", 1), "mem-a-h", "both")
            .await
            .unwrap();
        // without a key, a second completion is refused as it was
        assert!(is_wrong_state(
            &l.complete(&t.id, held("s1", 1), "mem-a-h").await
        ));
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

    fn approval(domain: &str, target: &str) -> ApprovalSpec {
        ApprovalSpec {
            domain: domain.into(),
            principal: "human:test:me".into(),
            action: "erase".into(),
            target: target.into(),
            version: "d1".into(),
            scope: "agent:test:asker".into(),
            expires: T0 + Duration::hours(24),
        }
    }

    async fn an_approval_is_one_event_and_is_read_back_whole(b: &Site) {
        let (mut l, now) = b.ledger().await;
        // a time with a fraction, which the text of the event must keep
        let at = T0 + Duration::milliseconds(90_500);
        *now.lock().unwrap() = at;
        let events = l.event_count().await.unwrap();
        let spec = approval("dom-a", "mem-a-1");
        let first = l.record_approval(&spec).await.unwrap();
        assert!(first.id.starts_with("apr-"), "{}", first.id);
        assert_eq!((&first.spec, first.at), (&spec, at));
        assert_eq!(l.event_count().await.unwrap(), events + 1, "one event");
        assert_eq!(
            l.approvals("dom-a", Some("mem-a-1")).await.unwrap(),
            vec![first.clone()]
        );
        the_event_holds_the_approval_and_nothing_else(b, &first).await;
        the_same_spec_again_is_another_approval(&mut l, first, events).await;
    }

    /// The log of a domain takes an approval as it takes a promotion: the event names
    /// the person who approved and never the principal it is for, it is given to its own
    /// domain alone, and it is at the highest label, since an approval records none.
    async fn the_event_of_an_approval_names_its_approver_and_its_own_domain(b: &Site) {
        let (mut l, _) = b.ledger().await;
        for (domain, target) in [("dom-a", "mem-a-1"), ("dom-b", "mem-b-1")] {
            l.record_approval(&approval(domain, target)).await.unwrap();
        }
        let got = l.events_after("dom-a", 0).await.unwrap();
        let seen: Vec<_> = got
            .iter()
            .map(|e| {
                let (who, attempt) = (e.principal.as_deref(), e.attempt.as_deref());
                (
                    e.op.as_str(),
                    e.task.as_str(),
                    who,
                    attempt,
                    e.classification,
                )
            })
            .collect();
        let approver = Some("human:test:me");
        let expected = (
            "approved",
            "approval:mem-a-1",
            approver,
            None,
            Label::Restricted,
        );
        assert_eq!(seen, vec![expected], "{got:?}");
        let other = l.events_after("dom-b", 0).await.unwrap();
        let tasks: Vec<&str> = other.iter().map(|e| e.task.as_str()).collect();
        assert_eq!(tasks, ["approval:mem-b-1"], "{other:?}");
        let after = l.events_after("dom-a", got[0].seq).await.unwrap();
        assert!(after.is_empty(), "{after:?}");
    }

    async fn the_same_spec_again_is_another_approval(
        l: &mut Ledger,
        first: ApprovalRecord,
        events: u64,
    ) {
        let second = l.record_approval(&first.spec).await.unwrap();
        assert_ne!(second.id, first.id);
        assert_eq!(l.event_count().await.unwrap(), events + 2);
        assert_eq!(
            l.approvals("dom-a", Some("mem-a-1")).await.unwrap(),
            vec![first, second],
            "oldest first"
        );
    }

    async fn the_event_holds_the_approval_and_nothing_else(b: &Site, record: &ApprovalRecord) {
        let payloads = b.payloads().await;
        assert_eq!(payloads.len(), 1);
        let v: serde_json::Value = serde_json::from_str(&payloads[0]).unwrap();
        let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "action",
                "at",
                "domain",
                "expires",
                "id",
                "kind",
                "principal",
                "scope",
                "target",
                "version"
            ]
        );
        assert_eq!(v["kind"], "approved");
        assert_eq!(v["id"], record.id.as_str());
        assert_eq!(v["at"], "2026-10-01T10:01:30.5Z");
    }

    async fn approvals_are_of_one_target_and_one_domain(b: &Site) {
        let mut l = b.open().await;
        // neither domain has filed a task: an approval is the first thing each records
        let one = l
            .record_approval(&approval("dom-a", "mem-a-1"))
            .await
            .unwrap();
        let two = l
            .record_approval(&approval("dom-a", "mem-a-2"))
            .await
            .unwrap();
        // the same entry id, in another domain
        let elsewhere = l
            .record_approval(&approval("dom-b", "mem-a-1"))
            .await
            .unwrap();
        assert_eq!(
            l.approvals("dom-a", Some("mem-a-1")).await.unwrap(),
            vec![one.clone()],
            "not another target's, and not another domain's"
        );
        assert_eq!(
            l.approvals("dom-a", Some("mem-a-2")).await.unwrap(),
            vec![two.clone()]
        );
        assert_eq!(l.approvals("dom-a", None).await.unwrap(), vec![one, two]);
        the_other_domain_lists_its_own(&l, elsewhere).await;
    }

    async fn the_other_domain_lists_its_own(l: &Ledger, elsewhere: ApprovalRecord) {
        assert_eq!(
            l.approvals("dom-b", None).await.unwrap(),
            vec![elsewhere.clone()]
        );
        assert_eq!(
            l.approvals("dom-b", Some("mem-a-1")).await.unwrap(),
            vec![elsewhere]
        );
        assert!(l
            .approvals("dom-b", Some("mem-a-2"))
            .await
            .unwrap()
            .is_empty());
        assert!(l.approvals("dom-c", None).await.unwrap().is_empty());
    }

    async fn an_approval_is_no_task_and_a_replay_keeps_it(b: &Site) {
        let mut l = b.open().await;
        let parent = admitted(&mut l, spec("A task beside it")).await;
        let record = l
            .record_approval(&approval("dom-a", "mem-a-1"))
            .await
            .unwrap();
        no_task_query_finds_the_approval(&l, &parent, &record).await;
        let before = l.approvals("dom-a", None).await.unwrap();
        assert_eq!(before, vec![record.clone()]);
        assert_eq!(l.rebuild().await.unwrap(), 3, "created, admitted, approved");
        assert_eq!(l.approvals("dom-a", None).await.unwrap(), before);
        assert_eq!(l.get(&parent.id).await.unwrap(), parent);
        no_task_query_finds_the_approval(&l, &parent, &record).await;
    }

    async fn no_task_query_finds_the_approval(l: &Ledger, parent: &Task, record: &ApprovalRecord) {
        only_the_task_is_listed(l, parent).await;
        for name in [record.id.as_str(), "approval:mem-a-1", "mem-a-1"] {
            assert!(
                l.with_origin("dom-a", name).await.unwrap().is_empty(),
                "{name}"
            );
            assert!(l.filed_from(name).await.unwrap().is_empty(), "{name}");
            assert!(l.dependents(name).await.unwrap().is_empty(), "{name}");
        }
        assert!(l.unposted("dom-a").await.unwrap().is_empty());
        for id in [record.id.as_str(), "approval:mem-a-1"] {
            assert!(matches!(l.get(id).await, Err(Error::NotFound(_))), "{id}");
        }
    }

    /// The text of a time does not order as the time does: `10:00:00Z` sorts after
    /// `10:00:00.5Z`, and `10:00:00.5Z` after `10:00:00.50001Z`.
    #[test]
    fn an_approval_is_expired_at_its_expiry_and_after_it_and_not_before() {
        let at = |s: &str| OffsetDateTime::parse(s, &Rfc3339).unwrap();
        let until = |expires: &str| ApprovalRecord {
            id: "apr-a-1".into(),
            spec: ApprovalSpec {
                expires: at(expires),
                ..approval("dom-a", "mem-a-1")
            },
            at: T0,
        };
        let half = until("2026-10-01T10:00:00.5Z");
        assert!(!half.expired(at("2026-10-01T10:00:00.123Z")), "before");
        assert!(!half.expired(at("2026-10-01T10:00:00Z")), "before");
        assert!(half.expired(at("2026-10-01T10:00:00.5Z")), "at its expiry");
        assert!(half.expired(at("2026-10-01T10:00:00.50001Z")), "after");
        let whole = until("2026-10-01T10:00:00Z");
        assert!(whole.expired(at("2026-10-01T10:00:00.5Z")), "after");
        assert!(!whole.expired(at("2026-10-01T09:59:59.999Z")), "before");
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

    /// Each event as the task and what happened to it.
    fn ops(events: &[Logged]) -> Vec<(&str, &str)> {
        events
            .iter()
            .map(|e| (e.task.as_str(), e.op.as_str()))
            .collect()
    }

    async fn events_after_gives_each_event_of_a_domain_once_in_the_ledger_s_order(b: &Site) {
        let (mut l, now) = b.ledger().await;
        let (a, second) = nine_events(&mut l, &now).await;
        let elsewhere = TaskSpec {
            domain: "dom-b".into(),
            ..spec("In b")
        };
        let other = admitted(&mut l, elsewhere).await;
        l.record_promotion(&promotion("mem-a-1", "d1"))
            .await
            .unwrap();
        let got = l.events_after("dom-a", 0).await.unwrap();
        let (a, second) = (a.id.as_str(), second.id.as_str());
        assert_eq!(
            ops(&got),
            [
                (a, "created"),
                (a, "admitted"),
                (a, "claimed"),
                (a, "heartbeat"),
                (a, "released"),
                (a, "claimed"),
                (a, "completed"),
                (second, "created"),
                (second, "invalidated"),
                ("promotion:mem-a-1", "promoted"),
            ]
        );
        assert!(got.windows(2).all(|w| w[0].seq < w[1].seq), "{got:?}");
        let got = l.events_after("dom-b", 0).await.unwrap();
        let other = other.id.as_str();
        assert_eq!(ops(&got), [(other, "created"), (other, "admitted")]);
        assert!(l.events_after("dom-none", 0).await.unwrap().is_empty());
    }

    async fn the_events_of_a_wait_and_of_an_effect_name_who_caused_them(b: &Site) {
        let (mut l, _) = b.ledger().await;
        let t = admitted(&mut l, spec("Post, then wait")).await;
        let c = claim(&mut l, &t.id, "s1", mins(30)).await.unwrap();
        let by = held("s1", c.generation);
        l.effect_begun(&t.id, by, "act-1").await.unwrap();
        l.effect_settled(&t.id, "act-1", Outcome::Applied, ("human:a:x", None))
            .await
            .unwrap();
        l.wait_for(&t.id, by, "check:1", Some("mem-a-h1"))
            .await
            .unwrap();
        l.arrived("dom-a", "check:1").await.unwrap();
        let events = l.events_after("dom-a", 0).await.unwrap();
        let by_op = |op: &str| -> Vec<Option<String>> {
            let of_op = events.iter().filter(|e| e.op == op);
            of_op.map(|e| e.principal.clone()).collect()
        };
        let s1 = Some("s1".to_string());
        assert_eq!(by_op("effect_begun"), vec![s1.clone()]);
        assert_eq!(by_op("effect_settled"), vec![Some("human:a:x".to_string())]);
        // the handoff left before the wait, and the wait: the holder's. What comes
        // after the wait is nobody's, since the wait gave the task up
        assert_eq!(by_op("handoff_left"), vec![s1.clone()]);
        assert_eq!(by_op("waiting"), vec![s1]);
        assert_eq!(by_op("arrived"), vec![None]);
        assert_eq!(by_op("unblocked"), vec![None]);
    }

    async fn events_after_gives_only_the_events_after_the_number_given(b: &Site) {
        let (mut l, now) = b.ledger().await;
        nine_events(&mut l, &now).await;
        let all = l.events_after("dom-a", 0).await.unwrap();
        assert_eq!(all.len(), 9);
        // from the heartbeat on: the release after it is by the holder of a claim from
        // before the number given, and is given as it was
        assert_eq!(all[4].principal.as_deref(), Some("s1"), "{:?}", all[4]);
        let rest = l.events_after("dom-a", all[3].seq).await.unwrap();
        assert_eq!(rest, all[4..]);
        let last = all[8].seq;
        assert!(l.events_after("dom-a", last).await.unwrap().is_empty());
    }

    async fn each_event_given_names_its_task_its_kind_its_principal_and_its_attempt_key(b: &Site) {
        let (mut l, now) = b.ledger().await;
        let internal = TaskSpec {
            classification: Label::Internal,
            ..spec("Keyed")
        };
        let t = admitted(&mut l, internal).await;
        let c = next_once(&mut l, "s1", "key-1").await.unwrap().unwrap();
        assert_eq!(c.task.id, t.id);
        assert!(is_wrong_state(&claim(&mut l, &t.id, "s2", mins(5)).await));
        *now.lock().unwrap() = T0 + mins(2);
        l.heartbeat_as(&t.id, "s1", mins(30)).await.unwrap();
        l.complete_once(&t.id, held("s1", 1), "mem-a-h", "done-1")
            .await
            .unwrap();
        l.record_promotion(&promotion("mem-a-1", "d1"))
            .await
            .unwrap();
        let got = l.events_after("dom-a", 0).await.unwrap();
        let named: Vec<(&str, Option<&str>, Option<&str>)> = got
            .iter()
            .map(|e| (e.op.as_str(), e.principal.as_deref(), e.attempt.as_deref()))
            .collect();
        assert_eq!(
            named,
            [
                ("created", Some("human:test:me"), None),
                ("admitted", None, None),
                ("claimed", Some("s1"), Some("key-1")),
                ("claim_refused", Some("s2"), None),
                ("heartbeat", Some("s1"), None),
                ("completed", Some("s1"), Some("done-1")),
                ("promoted", Some("agent:curator"), None),
            ]
        );
        assert!(got[..6].iter().all(|e| e.task == t.id), "{got:?}");
        each_event_given_is_classified_as_its_task_is(&mut l, &got).await;
    }

    /// `got` is of an internal task and an internal promotion.
    async fn each_event_given_is_classified_as_its_task_is(l: &mut Ledger, got: &[Logged]) {
        assert!(
            got.iter().all(|e| e.classification == Label::Internal),
            "{got:?}"
        );
        let public = admitted(l, spec("Public")).await;
        let after = got.last().unwrap().seq;
        let got = l.events_after("dom-a", after).await.unwrap();
        assert_eq!(ops(&got)[0], (public.id.as_str(), "created"));
        assert!(got.iter().all(|e| e.classification == Label::Public));
    }

    async fn a_completion_by_a_continuation_names_no_principal(b: &Site) {
        let (mut l, _) = b.ledger().await;
        let (parent, child, cont) = delegated(&mut l).await;
        claim(&mut l, &child.id, "s2", mins(5)).await.unwrap();
        l.complete_as(&child.id, "s2", "mem-a-child").await.unwrap();
        claim(&mut l, &cont.id, "s3", mins(5)).await.unwrap();
        l.complete_as(&cont.id, "s3", "mem-a-done").await.unwrap();
        l.complete_by_continuation(&parent.id, &cont.id, "mem-a-done")
            .await
            .unwrap();
        let got = l.events_after("dom-a", 0).await.unwrap();
        let of_parent: Vec<(&str, Option<&str>)> = got
            .iter()
            .filter(|e| e.task == parent.id)
            .map(|e| (e.op.as_str(), e.principal.as_deref()))
            .collect();
        // s1 held it, left a handoff and released it: the completion after is nobody's
        assert_eq!(
            of_parent[2..],
            [
                ("claimed", Some("s1")),
                ("handoff_left", Some("s1")),
                ("released", Some("s1")),
                ("completed", None),
            ]
        );
        let by_holder = got
            .iter()
            .find(|e| e.task == cont.id && e.op == "completed");
        assert_eq!(by_holder.unwrap().principal.as_deref(), Some("s3"));
    }

    async fn a_completion_after_the_lease_ran_out_names_no_principal(b: &Site) {
        let (mut l, now) = b.ledger().await;
        let parent = admitted(&mut l, spec("Compare two decisions")).await;
        claim(&mut l, &parent.id, "s1", mins(5)).await.unwrap();
        let cont = admitted(&mut l, resume(&parent, &[])).await;
        // s1 did not release it: its lease ran out, and the continuation closed the task
        *now.lock().unwrap() = T0 + mins(10);
        claim(&mut l, &cont.id, "s3", mins(5)).await.unwrap();
        l.complete_as(&cont.id, "s3", "mem-a-done").await.unwrap();
        l.complete_by_continuation(&parent.id, &cont.id, "mem-a-done")
            .await
            .unwrap();
        let got = l.events_after("dom-a", 0).await.unwrap();
        let closed = got
            .iter()
            .find(|e| e.task == parent.id && e.op == "completed");
        assert_eq!(closed.unwrap().principal, None, "{got:?}");
    }

    async fn the_events_given_hold_no_text_of_a_task_or_of_a_promotion(b: &Site) {
        let (mut l, _) = b.ledger().await;
        let t = admitted(&mut l, spec("Rotate the signing key")).await;
        claim(&mut l, &t.id, "s1", mins(5)).await.unwrap();
        l.release_as(&t.id, "s1", Some("mem-a-left")).await.unwrap();
        claim(&mut l, &t.id, "s1", mins(5)).await.unwrap();
        l.complete_as(&t.id, "s1", "mem-a-handoff").await.unwrap();
        let dropped = admitted(&mut l, spec("Second")).await;
        l.invalidate(&dropped.id, "the vault moved", None)
            .await
            .unwrap();
        l.record_promotion(&promotion("mem-a-1", "d1"))
            .await
            .unwrap();
        let got = l.events_after("dom-a", 0).await.unwrap();
        assert_eq!(got.len(), 11, "{got:?}");
        let json = serde_json::to_string(&got).unwrap();
        for text in [
            "Rotate the signing key",
            "Do it.",
            "mem-a-left",
            "mem-a-handoff",
            "the vault moved",
            "checked against the tree",
        ] {
            assert!(!json.contains(text), "{text:?} in {json}");
        }
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

    /// PostgreSQL gives an event its number at the insert, and not at the commit. Without
    /// the lock, the second append here commits number 4 while number 3 is in flight, a
    /// copy that reads then moves its cursor to 4, and number 3 is never copied.
    #[tokio::test]
    async fn on_postgres_an_append_waits_for_the_append_in_flight_in_its_domain() {
        let Some(server) = PgServer::start().await.unwrap() else {
            return;
        };
        let mut l = Ledger::connect(server.url()).await.unwrap();
        let a = l.create(spec("A")).await.unwrap().task;
        let b = l.create(spec("B")).await.unwrap().task;
        // an append in flight, as the ledger makes one: it holds the lock of the domain,
        // its row has its number, and it has not committed
        let mut c = pg_client(&server).await;
        let tx = c.transaction().await.unwrap();
        crate::postgres::lock_domain(&tx, "dom-a").await.unwrap();
        let s = crate::postgres::schema_for("dom-a");
        let admitted = Event::Admitted;
        tx.execute(
            &format!("INSERT INTO {s}.events (task_id, kind, at, payload) VALUES ($1, $2, $3, $4)"),
            &[
                &a.id,
                &admitted.kind(),
                &fmt_time(T0),
                &admitted.payload(&a.id).unwrap(),
            ],
        )
        .await
        .unwrap();
        // another connection appends an event of another task of the domain
        let mut other = Ledger::connect(server.url()).await.unwrap();
        let second = other.admit(&b.id);
        tokio::pin!(second);
        let early = tokio::time::timeout(std::time::Duration::from_millis(500), &mut second).await;
        assert!(early.is_err(), "the second append waits for the first");
        let seqs = |events: Vec<Logged>| events.into_iter().map(|e| e.seq).collect::<Vec<_>>();
        let seen = seqs(l.events_after("dom-a", 0).await.unwrap());
        tx.commit().await.unwrap();
        second.await.unwrap();
        // a copy that read while the first was in flight goes on from its last number
        let cursor = seen.last().copied().unwrap_or(0);
        let rest = seqs(l.events_after("dom-a", cursor).await.unwrap());
        let all = seqs(l.events_after("dom-a", 0).await.unwrap());
        assert_eq!(all.len(), 4, "{all:?}");
        assert_eq!([seen, rest].concat(), all, "and misses no event");
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
                spec: Box::new(TaskSpec {
                    domain: domain.into(),
                    ..spec("Old")
                }),
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

    async fn a_task_that_waits_on_an_event_is_nobody_s_until_the_event_comes(b: &Site) {
        let (mut l, now) = b.ledger().await;
        let t = admitted(&mut l, spec("Open the change and wait for its check")).await;
        let c = claim(&mut l, &t.id, "s1", mins(30)).await.unwrap();
        let by = held("s1", c.generation);
        let parked = l
            .wait_for(&t.id, by, "check:42", Some("mem-a-h1"))
            .await
            .unwrap();
        it_is_parked(&parked);
        // its dependencies are done and no lease is live, and still nobody gets it
        assert!(next(&mut l, "s2", mins(30)).await.is_none());
        *now.lock().unwrap() = T0 + mins(90);
        nobody_gets_it(&mut l, &t.id).await;
        // and its record says what the claim lost to
        assert_eq!(
            last_refusal(&l, &t.id).await,
            (
                "s2".to_string(),
                "after the claim of generation 1, the task waited on check:42".to_string()
            )
        );
        it_waits_through_a_replay(&mut l, &t.id, by).await;
        only_its_event_wakes_it(&mut l, &t).await;
        the_woken_task_is_claimed_again(&mut l, &t).await;
    }

    fn it_is_parked(parked: &Task) {
        assert_eq!(parked.status, Status::Blocked);
        assert_eq!(parked.waiting_on.as_deref(), Some("check:42"));
        assert_eq!(parked.holder, None);
        assert_eq!(parked.handoff.as_deref(), Some("mem-a-h1"));
        assert_eq!(parked.held_back().as_deref(), Some("waits on check:42"));
    }

    async fn it_waits_through_a_replay(l: &mut Ledger, id: &str, by: Held<'_>) {
        // who gave it up holds it no more
        assert!(is_not_holder(&l.heartbeat(id, by, mins(30)).await));
        // a replay reads the same from the events
        l.rebuild().await.unwrap();
        let replayed = l.get(id).await.unwrap();
        assert_eq!(replayed.waiting_on.as_deref(), Some("check:42"));
        assert!(next(l, "s2", mins(30)).await.is_none());
    }

    /// Neither the next claim nor a claim by name gets the task.
    async fn nobody_gets_it(l: &mut Ledger, id: &str) {
        assert!(next(l, "s2", mins(30)).await.is_none());
        assert!(is_wrong_state(&claim(l, id, "s2", mins(30)).await));
    }

    /// The last refused claim on the task's record: who asked, and why not.
    async fn last_refusal(l: &Ledger, id: &str) -> (String, String) {
        l.refused_claims(id).await.unwrap().pop().unwrap()
    }

    async fn only_its_event_wakes_it(l: &mut Ledger, t: &Task) {
        assert!(l.arrived("dom-a", "check:41").await.unwrap().is_empty());
        assert!(l.arrived("dom-b", "check:42").await.unwrap().is_empty());
        assert!(next(l, "s2", mins(30)).await.is_none());
        let woken = l.arrived("dom-a", "check:42").await.unwrap();
        assert_eq!(woken.len(), 1);
        assert_eq!(woken[0].id, t.id);
        assert_eq!(woken[0].status, Status::Open);
        assert_eq!(woken[0].waiting_on, None);
    }

    async fn the_woken_task_is_claimed_again(l: &mut Ledger, t: &Task) {
        // told twice, the event wakes nothing more
        assert!(l.arrived("dom-a", "check:42").await.unwrap().is_empty());
        let c = next(l, "s2", mins(30)).await.unwrap();
        assert_eq!(c.task.id, t.id);
        assert_eq!(c.generation, 2);
        assert_eq!(c.task.handoff.as_deref(), Some("mem-a-h1"));
        assert!(is_fenced(
            &l.wait_for(&t.id, held("s1", 1), "check:43", None).await,
            1,
            2
        ));
    }

    async fn an_effect_not_settled_holds_the_task_past_its_lease(b: &Site) {
        let (mut l, now) = b.ledger().await;
        let t = admitted(&mut l, spec("Post the release note")).await;
        let c = claim(&mut l, &t.id, "s1", mins(30)).await.unwrap();
        let by = held("s1", c.generation);
        l.effect_begun(&t.id, by, "act-1").await.unwrap();
        // the retry of the action is the same effect
        let again = l.effect_begun(&t.id, by, "act-1").await.unwrap();
        assert_eq!(again.effects, vec!["act-1".to_string()]);
        // its holder cannot call it done while nobody knows what the destination holds
        assert!(is_wrong_state(&l.complete(&t.id, by, "mem-a-h").await));
        // and says what it holds itself while its claim is in force
        l.effect_begun(&t.id, by, "act-0").await.unwrap();
        let own = l
            .effect_settled(&t.id, "act-0", Outcome::Applied, ("s1", Some(by)))
            .await
            .unwrap();
        assert_eq!(own.effects, vec!["act-1".to_string()]);
        *now.lock().unwrap() = T0 + mins(31);
        the_lease_that_ran_out_gives_it_to_nobody(&mut l, &t.id).await;
        the_effect_holds_through_a_replay(&mut l, &t.id, by).await;
    }

    /// The lease ran out, and that alone gives the task to nobody. Its record says what
    /// the claim lost to.
    async fn the_lease_that_ran_out_gives_it_to_nobody(l: &mut Ledger, id: &str) {
        nobody_gets_it(l, id).await;
        assert_eq!(
            last_refusal(l, id).await.1,
            "after the claim of generation 1, effect act-1 of the task was not settled"
        );
    }

    async fn the_effect_holds_through_a_replay(l: &mut Ledger, id: &str, by: Held<'_>) {
        let held_back = l.get(id).await.unwrap().held_back();
        assert_eq!(held_back.as_deref(), Some("effect act-1 is not settled"));
        // nor may the one who lost the lease begin another
        assert!(is_not_holder(&l.effect_begun(id, by, "act-2").await));
        l.rebuild().await.unwrap();
        assert_eq!(l.get(id).await.unwrap().effects, vec!["act-1"]);
        assert!(next(l, "s2", mins(30)).await.is_none());
    }

    async fn a_settled_effect_gives_the_task_back_and_fences_who_began_it(b: &Site) {
        let (mut l, now) = b.ledger().await;
        let t = admitted(&mut l, spec("Post the release note")).await;
        let c = claim(&mut l, &t.id, "s1", mins(30)).await.unwrap();
        let by = held("s1", c.generation);
        l.effect_begun(&t.id, by, "act-1").await.unwrap();
        *now.lock().unwrap() = T0 + mins(31);
        not_settled_by_a_wrong_name_or_a_stale_holder(&mut l, &t.id, by).await;
        assert_eq!(l.settlement(&t.id, "act-1").await.unwrap(), None);
        let settled = l
            .effect_settled(&t.id, "act-1", Outcome::NotApplied, ("human:a:x", None))
            .await
            .unwrap();
        assert!(settled.effects.is_empty());
        assert_eq!(
            l.settlement(&t.id, "act-1").await.unwrap(),
            Some((Outcome::NotApplied, "human:a:x".to_string()))
        );
        settled_once_and_claimed_again(&mut l, &t, by).await;
    }

    async fn not_settled_by_a_wrong_name_or_a_stale_holder(l: &mut Ledger, id: &str, by: Held<'_>) {
        let unknown = l
            .effect_settled(id, "act-9", Outcome::Applied, ("human:a:x", None))
            .await;
        assert!(is_wrong_state(&unknown));
        // who lost the lease does not say what became of it
        let stale = l
            .effect_settled(id, "act-1", Outcome::Applied, ("s1", Some(by)))
            .await;
        assert!(is_not_holder(&stale));
    }

    async fn settled_once_and_claimed_again(l: &mut Ledger, t: &Task, by: Held<'_>) {
        // said again, it is the same settlement and no second event
        let events = l.event_count().await.unwrap();
        l.effect_settled(&t.id, "act-1", Outcome::NotApplied, ("human:a:y", None))
            .await
            .unwrap();
        // said again with the other outcome, it is refused: the record keeps the first
        let other = l
            .effect_settled(&t.id, "act-1", Outcome::Applied, ("human:a:y", None))
            .await;
        assert!(is_wrong_state(&other), "{other:?}");
        assert_eq!(l.event_count().await.unwrap(), events);
        assert_eq!(
            l.settlement(&t.id, "act-1").await.unwrap(),
            Some((Outcome::NotApplied, "human:a:x".to_string()))
        );
        let c2 = next(l, "s2", mins(30)).await.unwrap();
        assert_eq!((c2.task.id.as_str(), c2.generation), (t.id.as_str(), 2));
        // the delayed request of the first holder is refused where the effect is recorded
        assert!(is_fenced(&l.effect_begun(&t.id, by, "act-1").await, 1, 2));
        assert!(l.get(&t.id).await.unwrap().effects.is_empty());
    }

    /// A task that `s1` claimed and gave up for `check:42`, then sent back to triage. No
    /// call sends a waiting task back today: a task is claimed only when each task it
    /// depends on is done, and a done task is not invalidated. The events can hold it
    /// all the same, so the test appends the event.
    async fn parked_and_sent_back(l: &mut Ledger, title: &str) -> Task {
        let t = admitted(l, spec(title)).await;
        let c = claim(l, &t.id, "s1", mins(30)).await.unwrap();
        l.wait_for(&t.id, held("s1", c.generation), "check:42", None)
            .await
            .unwrap();
        let because = "task-a-gone".to_string();
        l.append(&t.id, &Event::Retriaged { because })
            .await
            .unwrap();
        let back = l.get(&t.id).await.unwrap();
        assert_eq!(back.status, Status::Proposed);
        assert_eq!(back.waiting_on.as_deref(), Some("check:42"));
        back
    }

    async fn a_task_sent_back_to_triage_while_it_waits_still_waits(b: &Site) {
        let (mut l, _) = b.ledger().await;
        // admitted again before the event: it shows as blocked, and it is nobody's
        let first = parked_and_sent_back(&mut l, "Admitted before the event").await;
        let again = l.admit(&first.id).await.unwrap();
        assert_eq!(again.status, Status::Blocked);
        assert_eq!(again.waiting_on.as_deref(), Some("check:42"));
        nobody_gets_it(&mut l, &first.id).await;
        // still in triage when the event comes: the event reaches it there
        let second = parked_and_sent_back(&mut l, "Admitted after the event").await;
        // the blocked task first, then the proposed one: the order in which arrived looks
        let woken = l.arrived("dom-a", "check:42").await.unwrap();
        let seen: Vec<_> = woken
            .iter()
            .map(|t| (t.id.as_str(), t.status, t.waiting_on.as_deref()))
            .collect();
        assert_eq!(
            seen,
            vec![
                (first.id.as_str(), Status::Open, None),
                (second.id.as_str(), Status::Proposed, None)
            ]
        );
        assert_eq!(l.admit(&second.id).await.unwrap().status, Status::Open);
        for id in [&first.id, &second.id] {
            let c = claim(&mut l, id, "s2", mins(30)).await.unwrap();
            assert_eq!(c.generation, 2);
        }
    }

    /// A task filed from `from` under `kind`: `continues`, `retries` or `task`.
    fn filed_from(kind: &str, from: &str, title: &str) -> TaskSpec {
        TaskSpec {
            origin: Some(format!("{kind}:{from}")),
            ..spec(title)
        }
    }

    async fn no_work_is_filed_from_a_task_whose_effect_is_not_settled(b: &Site) {
        let (mut l, _) = b.ledger().await;
        let t = admitted(&mut l, spec("Post the release note")).await;
        let c = claim(&mut l, &t.id, "s1", mins(30)).await.unwrap();
        let by = held("s1", c.generation);
        let resumes = l
            .create(filed_from("continues", &t.id, "Resume"))
            .await
            .unwrap()
            .task;
        l.effect_begun(&t.id, by, "act-1").await.unwrap();
        l.release(&t.id, by, None).await.unwrap();
        nothing_is_filed_from(&mut l, &t.id, &resumes.id).await;
        // a task filed from nothing this ledger holds is filed as before
        l.create(filed_from("task", "task-a-nowhere", "Elsewhere"))
            .await
            .unwrap();
        l.effect_settled(&t.id, "act-1", Outcome::NotApplied, ("human:a:x", None))
            .await
            .unwrap();
        let retry = l.create(filed_from("retries", &t.id, "Again")).await;
        assert_eq!(retry.unwrap().task.status, Status::Proposed);
    }

    /// No retry, no continuation and no part of `id` is filed, nor a retry of the task
    /// that resumes it. Each refusal names `id`, and none leaves an event.
    async fn nothing_is_filed_from(l: &mut Ledger, id: &str, resumes: &str) {
        let before = l.event_count().await.unwrap();
        for (kind, from) in [
            ("retries", id),
            ("continues", id),
            ("task", id),
            ("retries", resumes),
        ] {
            let filed = l.create(filed_from(kind, from, "Again")).await;
            assert!(
                matches!(&filed, Err(Error::WrongState { id: held, .. }) if held == id),
                "{kind}:{from} gave {filed:?}"
            );
        }
        assert_eq!(l.event_count().await.unwrap(), before);
    }

    async fn a_task_that_resumes_one_with_an_unsettled_effect_is_nobody_s(b: &Site) {
        let (mut l, _) = b.ledger().await;
        let t = admitted(&mut l, spec("Post the release note")).await;
        let c = claim(&mut l, &t.id, "s1", mins(30)).await.unwrap();
        let by = held("s1", c.generation);
        // filed before the effect began, so the filing was not refused
        let resumes = admitted(&mut l, filed_from("continues", &t.id, "Resume")).await;
        let again = admitted(&mut l, filed_from("retries", &resumes.id, "Again")).await;
        l.effect_begun(&t.id, by, "act-1").await.unwrap();
        l.release(&t.id, by, None).await.unwrap();
        neither_is_claimed(&mut l, &t.id, [&resumes.id, &again.id]).await;
        l.effect_settled(&t.id, "act-1", Outcome::Applied, ("human:a:x", None))
            .await
            .unwrap();
        let c2 = claim(&mut l, &resumes.id, "s2", mins(30)).await.unwrap();
        assert_eq!(c2.generation, 1);
    }

    /// Nobody gets the task that resumes `id`, or the retry of that task, by the next
    /// claim or by name. The record of the first says which effect the claim lost to.
    async fn neither_is_claimed(l: &mut Ledger, id: &str, resuming: [&String; 2]) {
        assert!(next(l, "s2", mins(30)).await.is_none());
        for r in resuming {
            assert!(is_wrong_state(&claim(l, r, "s2", mins(30)).await));
        }
        assert_eq!(
            last_refusal(l, resuming[0]).await.1,
            format!(
                "after the claim of generation 0, effect act-1 of {id}, whose work the task resumes, was not settled"
            )
        );
    }

    async fn a_task_closes_with_its_done_continuation_when_its_last_effect_is_settled(b: &Site) {
        let (mut l, now) = b.ledger().await;
        // a root that was given up, the task that resumes it, and the task that resumes that
        let root = given_up(&mut l, "Ship the release").await;
        let t = admitted(&mut l, filed_from("continues", &root.id, "Resume")).await;
        let c = claim(&mut l, &t.id, "s1", mins(30)).await.unwrap();
        let resumes = admitted(&mut l, filed_from("continues", &t.id, "Resume")).await;
        let c2 = claim(&mut l, &resumes.id, "s2", mins(120)).await.unwrap();
        // its holder begins an effect while the continuation runs, and its lease runs out
        l.effect_begun(&t.id, held("s1", c.generation), "act-1")
            .await
            .unwrap();
        *now.lock().unwrap() = T0 + mins(31);
        let done = l
            .complete(&resumes.id, held("s2", c2.generation), "mem-a-h2")
            .await
            .unwrap();
        l.close_resumed(&done).await;
        the_chain_stays_open_and_nobody_s(&mut l, &t.id, &root.id).await;
        the_settlement_closes_the_chain(&mut l, &t.id, &root.id).await;
    }

    /// Not done while nobody knows what the destination holds, and nobody's meanwhile.
    async fn the_chain_stays_open_and_nobody_s(l: &mut Ledger, id: &str, root: &str) {
        let left = get_all(l, &[id, root]).await;
        assert_eq!(left[0].status, Status::Claimed);
        assert_eq!(left[1].status, Status::Open);
        assert!(next(l, "s3", mins(30)).await.is_none());
    }

    /// A task that `s0` claimed and gave back.
    async fn given_up(l: &mut Ledger, title: &str) -> Task {
        let t = admitted(l, spec(title)).await;
        let c = claim(l, &t.id, "s0", mins(30)).await.unwrap();
        l.release(&t.id, held("s0", c.generation), None)
            .await
            .unwrap()
    }

    /// With `act-1` of `id` settled, `id` and `root` are done with the handoff of the
    /// continuation. The work was finished once, so nothing is on offer again.
    async fn the_settlement_closes_the_chain(l: &mut Ledger, id: &str, root: &str) {
        let settled = l
            .effect_settled(id, "act-1", Outcome::Applied, ("human:a:x", None))
            .await
            .unwrap();
        assert_eq!(settled.status, Status::Done);
        assert_eq!(settled.handoff.as_deref(), Some("mem-a-h2"));
        let root = l.get(root).await.unwrap();
        assert_eq!(root.status, Status::Done);
        assert_eq!(root.handoff.as_deref(), Some("mem-a-h2"));
        assert!(next(l, "s3", mins(30)).await.is_none());
    }

    async fn a_task_keeps_the_rest_of_its_record(b: &Site) {
        let (mut l, _) = b.ledger().await;
        let mut s = spec("Carry out the decision");
        s.parent = Some("task-a-1".into());
        s.scores = Some(Scores {
            value: 8,
            urgency: 5,
            effort: 3,
            unblocks: vec!["task-a-9".into()],
        });
        s.implements = vec!["mem-a-0847".into()];
        s.reversibility = Some(Reversibility::Irreversible);
        s.artifacts_touched = vec!["docs/**".into()];
        s.policy_context = vec!["pol-L1-007".into()];
        let filed = l.create(s.clone()).await.unwrap().task;
        assert_eq!(filed.spec, s);
        drop(l);
        let l = b.open().await;
        assert_eq!(l.get(&filed.id).await.unwrap().spec, s);
        // a task without them says nothing of them
        let plain = serde_json::to_value(spec("Plain")).unwrap();
        for absent in ["parent", "scores", "implements", "reversibility"] {
            assert!(plain.get(absent).is_none(), "{absent}");
        }
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

    const QA: &str = "pos-a-qa";
    const DEV: &str = "pos-a-dev";

    /// A task pinned to `position`, for continuity.
    fn pinned(title: &str, position: &str) -> TaskSpec {
        TaskSpec {
            position: Some(position.into()),
            pin_reason: Some(PinReason::Continuity),
            ..spec(title)
        }
    }

    /// An unpinned task that runs under `definition`.
    fn defined(title: &str, definition: &str) -> TaskSpec {
        TaskSpec {
            definition: Some(definition.into()),
            ..spec(title)
        }
    }

    /// A claimant with an empty manifest and the highest handling, in `seat` or in none.
    fn claimer<'a>(holder: &'a str, seat: Option<Seat<'a>>) -> Claimer<'a> {
        Claimer {
            holder,
            manifest: &[],
            handling: Label::Restricted,
            seat,
            attempt: None,
        }
    }

    /// The seat of a position whose charter covers nothing.
    fn seat(position: &str) -> Option<Seat<'_>> {
        Some(Seat {
            position,
            covers: &[],
        })
    }

    /// `claim_next_by` in `dom-a` from `seat`.
    async fn next_from(l: &mut Ledger, holder: &str, seat: Option<Seat<'_>>) -> Option<Claim> {
        l.claim_next_by("dom-a", claimer(holder, seat), mins(30))
            .await
            .unwrap()
    }

    /// `claim_by` from `seat`.
    async fn claim_from(
        l: &mut Ledger,
        id: &str,
        holder: &str,
        seat: Option<Seat<'_>>,
    ) -> Result<Claim, Error> {
        l.claim_by(id, claimer(holder, seat), mins(30)).await
    }

    fn is_pinned_to<T>(r: &Result<T, Error>, to: &str) -> bool {
        matches!(r, Err(Error::Pinned { position, .. }) if position == to)
    }

    fn is_bad_pin<T>(r: &Result<T, Error>) -> bool {
        matches!(r, Err(Error::BadPin(_)))
    }

    async fn half_a_pin_is_refused_and_nothing_is_filed(b: &Site) {
        let (mut l, _) = b.ledger().await;
        let no_reason = TaskSpec {
            pin_reason: None,
            ..pinned("No reason", QA)
        };
        assert!(is_bad_pin(&l.create(no_reason).await));
        let no_position = TaskSpec {
            position: None,
            ..pinned("No position", QA)
        };
        assert!(is_bad_pin(&l.create(no_position).await));
        assert!(is_bad_pin(&l.create(pinned("No name", "")).await));
        assert_eq!(l.event_count().await.unwrap(), 0, "nothing is filed");
        assert!(l.list("dom-a", Status::Proposed).await.unwrap().is_empty());
        let whole = l.create(pinned("Whole", QA)).await.unwrap().task;
        assert_eq!(whole.spec.position.as_deref(), Some(QA));
        assert_eq!(whole.spec.pin_reason, Some(PinReason::Continuity));
    }

    async fn a_pinned_task_goes_only_to_a_claimant_that_fills_its_position(b: &Site) {
        let (mut l, _) = b.ledger().await;
        let t = admitted(&mut l, pinned("Review the change", QA)).await;
        assert!(
            next_from(&mut l, "s1", None).await.is_none(),
            "not to a claimant in no position"
        );
        assert!(next(&mut l, "s1", mins(30)).await.is_none());
        assert!(
            next_from(&mut l, "s2", seat(DEV)).await.is_none(),
            "not to a claimant in another position"
        );
        let c = next_from(&mut l, "s3", seat(QA)).await.unwrap();
        assert_eq!(c.task.id, t.id);
        assert_eq!(c.task.holder.as_deref(), Some("s3"));
        assert_eq!(c.task.spec.position.as_deref(), Some(QA));
        assert_eq!(c.task.spec.pin_reason, Some(PinReason::Continuity));
    }

    async fn a_claim_of_a_pinned_task_from_outside_its_position_is_refused_and_recorded(b: &Site) {
        let (mut l, _) = b.ledger().await;
        let filed = l.create(pinned("Review", QA)).await.unwrap().task;
        let early = claim_from(&mut l, &filed.id, "s1", None).await;
        assert!(is_wrong_state(&early), "not admitted comes first");
        let t = l.admit(&filed.id).await.unwrap();
        for _ in 0..2 {
            let refused = claim_from(&mut l, &t.id, "s1", None).await;
            assert!(is_pinned_to(&refused, QA), "{:?}", refused.err());
        }
        the_refusal_names_the_position_once(&l, &t).await;
        let other = claim_from(&mut l, &t.id, "s2", seat(DEV)).await;
        assert!(is_pinned_to(&other, QA));
        assert!(is_pinned_to(
            &claim(&mut l, &t.id, "s3", mins(30)).await,
            QA
        ));
        the_position_s_own_claims_are_judged_as_any_other(&mut l, &t).await;
    }

    async fn the_refusal_names_the_position_once(l: &Ledger, t: &Task) {
        let why = format!("the task is pinned to position {QA}, and the session does not fill it");
        let refused = l.refused_claims(&t.id).await.unwrap();
        assert_eq!(refused[0].1, "the task was not admitted");
        assert_eq!(refused[1..], [("s1".to_string(), why)]);
    }

    /// A claimant that fills the position claims the task, and a second one is refused
    /// for the live lease, not for the pin.
    async fn the_position_s_own_claims_are_judged_as_any_other(l: &mut Ledger, t: &Task) {
        claim_from(l, &t.id, "s4", seat(QA)).await.unwrap();
        let second = claim_from(l, &t.id, "s5", seat(QA)).await;
        assert!(is_wrong_state(&second), "{:?}", second.err());
        let last = l.refused_claims(&t.id).await.unwrap().pop().unwrap();
        assert_eq!(
            last,
            (
                "s5".to_string(),
                "the lease of the claim of generation 1 was live".to_string()
            )
        );
    }

    async fn a_claimant_in_a_position_takes_an_unpinned_task_only_when_its_charter_covers_it(
        b: &Site,
    ) {
        let (mut l, _) = b.ledger().await;
        let covers = ["review".to_string()];
        let qa = Some(Seat {
            position: QA,
            covers: &covers,
        });
        let covered = admitted(&mut l, defined("Covered", "review@mem-x")).await;
        // each of the others ranks above it, so a seat that took them would take them first
        let high = |s: TaskSpec| TaskSpec { priority: 5, ..s };
        let bare = admitted(&mut l, high(spec("No definition"))).await;
        let other = admitted(&mut l, high(defined("Another", "reviewer@mem-y"))).await;
        let c = next_from(&mut l, "s1", qa).await.unwrap();
        assert_eq!(c.task.id, covered.id, "the one its charter covers");
        assert!(next_from(&mut l, "s1", qa).await.is_none(), "and no other");
        let named = claim_from(&mut l, &bare.id, "s1", qa).await;
        assert!(is_wrong_state(&named), "{:?}", named.err());
        // a claimant in no position takes each of the three
        let again = admitted(&mut l, defined("Covered too", "review@mem-x")).await;
        let mut taken = Vec::new();
        while let Some(c) = next_from(&mut l, "s2", None).await {
            taken.push(c.task.id);
        }
        // the two of one priority were filed at one time on the test's clock, so the
        // queue orders them by id: the test asks which were taken, not in which order
        let mut all = [bare.id, other.id, again.id];
        all.sort();
        taken.sort();
        assert_eq!(taken, all);
    }

    async fn a_claim_asked_again_under_its_key_from_outside_the_position_is_spent(b: &Site) {
        let (mut l, _) = b.ledger().await;
        let t = admitted(&mut l, pinned("Review", QA)).await;
        let keyed = |holder, seat| Claimer {
            attempt: Some("key-1"),
            ..claimer(holder, seat)
        };
        let first = l
            .claim_next_by("dom-a", keyed("s1", seat(QA)), mins(30))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.task.id, t.id);
        let again = l
            .claim_next_by("dom-a", keyed("s1", seat(QA)), mins(30))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(again, first, "the same claim from the same seat");
        let unseated = l.claim_next_by("dom-a", keyed("s1", None), mins(30)).await;
        assert!(is_spent(&unseated), "{:?}", unseated.err());
        let elsewhere = l
            .claim_next_by("dom-a", keyed("s1", seat(DEV)), mins(30))
            .await;
        assert!(is_spent(&elsewhere), "{:?}", elsewhere.err());
        assert_eq!(l.get(&t.id).await.unwrap().generation, 1, "one claim");
    }

    const RETIRED: &str = "position pos-a-qa is retired";

    /// The `unpinned` events of the log, as JSON.
    async fn unpinned_events(b: &Site) -> Vec<serde_json::Value> {
        let all = b.payloads().await;
        let parsed = all.iter().map(|p| serde_json::from_str(p).unwrap());
        parsed
            .filter(|e: &serde_json::Value| e["kind"] == "unpinned")
            .collect()
    }

    async fn an_unpinned_task_returns_to_matching_and_a_replay_keeps_it_unpinned(b: &Site) {
        let (mut l, _) = b.ledger().await;
        let filed = TaskSpec {
            definition: Some("review@mem-x".into()),
            acceptance: vec!["the tests pass".into()],
            ..pinned("Review", QA)
        };
        let t = admitted(&mut l, filed).await;
        let kept = admitted(&mut l, pinned("Stays pinned", QA)).await;
        assert!(next_from(&mut l, "s1", None).await.is_none());
        let unpinned = l.unpin(&t.id, RETIRED).await.unwrap();
        assert_eq!(unpinned.status, Status::Open);
        let without_pin = TaskSpec {
            position: None,
            pin_reason: None,
            ..t.spec.clone()
        };
        assert_eq!(unpinned.spec, without_pin, "the pin leaves, and no more");
        the_log_holds_one_unpinned_event(b).await;
        a_replay_keeps_the_one_unpinned_and_the_other_pinned(&mut l, &t, &kept).await;
        let c = next_from(&mut l, "s1", None).await.unwrap();
        assert_eq!(c.task.id, t.id, "back in matching");
        assert!(next_from(&mut l, "s1", None).await.is_none());
    }

    async fn the_log_holds_one_unpinned_event(b: &Site) {
        let events = unpinned_events(b).await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["position"], QA);
        assert_eq!(events[0]["why"], RETIRED);
    }

    async fn a_replay_keeps_the_one_unpinned_and_the_other_pinned(
        l: &mut Ledger,
        unpinned: &Task,
        pinned: &Task,
    ) {
        let before = get_all(l, &[&unpinned.id, &pinned.id]).await;
        l.rebuild().await.unwrap();
        let after = get_all(l, &[&unpinned.id, &pinned.id]).await;
        assert_eq!(before, after);
        assert_eq!(after[0].spec.position, None);
        assert_eq!(after[1].spec.position.as_deref(), Some(QA));
    }

    /// A pinned task that its holder, a session in the position, gave up until the event
    /// `check:1` comes.
    async fn pinned_and_waiting(l: &mut Ledger) -> Task {
        let t = admitted(l, pinned("Announce", QA)).await;
        let c = claim_from(l, &t.id, "s1", seat(QA)).await.unwrap();
        l.wait_for(&t.id, held("s1", c.generation), "check:1", None)
            .await
            .unwrap();
        t
    }

    /// A pinned task with an external action that its holder, a session in the position,
    /// began and never settled. The lease of that holder ran out.
    async fn pinned_with_an_effect(l: &mut Ledger, now: &Mutex<OffsetDateTime>) -> Task {
        let t = admitted(l, pinned("Announce", QA)).await;
        let c = claim_from(l, &t.id, "s1", seat(QA)).await.unwrap();
        l.effect_begun(&t.id, held("s1", c.generation), "act-1")
            .await
            .unwrap();
        *now.lock().unwrap() = T0 + mins(31);
        t
    }

    /// A claimant in `seat` does not get task `id`: not as the next task, and not by
    /// name, where the refusal is that of a task nobody may claim now.
    async fn held_back_from(l: &mut Ledger, id: &str, seat: Option<Seat<'_>>) {
        assert!(next_from(l, "s2", seat).await.is_none());
        let asked = claim_from(l, id, "s2", seat).await;
        assert!(is_wrong_state(&asked));
    }

    /// Task `id` goes to a claimant that fills `QA`, and to no claimant outside it.
    async fn only_the_position_gets(l: &mut Ledger, id: &str) {
        assert!(next_from(l, "s3", None).await.is_none());
        let outside = claim_from(l, id, "s3", None).await;
        assert!(is_pinned_to(&outside, QA));
        let again = next_from(l, "s2", seat(QA)).await.unwrap();
        assert_eq!(again.task.id, id);
    }

    async fn a_replay_gives_the_same_task(l: &mut Ledger, id: &str) {
        let before = l.get(id).await.unwrap();
        l.rebuild().await.unwrap();
        assert_eq!(l.get(id).await.unwrap(), before);
    }

    async fn a_pinned_task_that_is_held_back_goes_to_nobody_and_keeps_its_pin(b: &Site) {
        let (mut l, _) = b.ledger().await;
        let t = pinned_and_waiting(&mut l).await;
        // it waits on an event: a session that fills its position does not get it
        held_back_from(&mut l, &t.id, seat(QA)).await;
        l.arrived("dom-a", "check:1").await.unwrap();
        // the event came, and the task is pinned as it was filed
        let back = l.get(&t.id).await.unwrap();
        assert_eq!(back.status, Status::Open);
        assert_eq!(
            back.spec, t.spec,
            "the wait changed nothing of what was filed"
        );
        only_the_position_gets(&mut l, &t.id).await;
        a_replay_gives_the_same_task(&mut l, &t.id).await;
    }

    async fn an_unpin_leaves_an_effect_that_is_not_settled_and_the_task_stays_held_back(b: &Site) {
        let (mut l, now) = b.ledger().await;
        let t = pinned_with_an_effect(&mut l, &now).await;
        let unpinned = l.unpin(&t.id, RETIRED).await.unwrap();
        assert_eq!(unpinned.spec.position, None);
        assert_eq!(unpinned.effects, ["act-1"], "the effect is still there");
        // no pin now, and nobody gets it: nobody knows what became of the action
        held_back_from(&mut l, &t.id, None).await;
        a_replay_gives_the_same_task(&mut l, &t.id).await;
        l.effect_settled(&t.id, "act-1", Outcome::NotApplied, ("human:a:x", None))
            .await
            .unwrap();
        let taken = next_from(&mut l, "s2", None).await.unwrap();
        assert_eq!(taken.task.id, t.id);
    }

    async fn the_event_of_an_unpin_names_no_principal(b: &Site) {
        let (mut l, _) = b.ledger().await;
        let t = admitted(&mut l, pinned("Announce", QA)).await;
        // held under a live lease when the pin leaves: the holder did not take it off
        claim_from(&mut l, &t.id, "s1", seat(QA)).await.unwrap();
        l.unpin(&t.id, RETIRED).await.unwrap();
        let events = l.events_after("dom-a", 0).await.unwrap();
        let of = |op: &str| -> Vec<&Logged> { events.iter().filter(|e| e.op == op).collect() };
        let unpinned = of("unpinned");
        assert_eq!(unpinned.len(), 1, "{events:?}");
        assert_eq!(unpinned[0].task, t.id);
        assert_eq!(unpinned[0].principal, None);
        assert_eq!(unpinned[0].attempt, None);
        // the claim made from the position names its holder, as any claim does
        let claimed = of("claimed");
        assert_eq!(claimed.len(), 1, "{events:?}");
        assert_eq!(claimed[0].principal.as_deref(), Some("s1"));
    }

    /// A pinned task that is done, one that is invalidated, a task never pinned and one
    /// unpinned already.
    async fn four_tasks_with_no_pin_to_take_off(l: &mut Ledger) -> [Task; 4] {
        let done = admitted(l, pinned("Done", QA)).await;
        claim_from(l, &done.id, "s1", seat(QA)).await.unwrap();
        l.complete_as(&done.id, "s1", "mem-a-h").await.unwrap();
        let invalid = admitted(l, pinned("Invalidated", QA)).await;
        l.invalidate(&invalid.id, "obsolete", None).await.unwrap();
        let never = admitted(l, spec("Never pinned")).await;
        let once = admitted(l, pinned("Unpinned once", QA)).await;
        l.unpin(&once.id, RETIRED).await.unwrap();
        [done, invalid, never, once]
    }

    async fn only_an_open_pinned_task_is_unpinned(b: &Site) {
        let (mut l, _) = b.ledger().await;
        let tasks = four_tasks_with_no_pin_to_take_off(&mut l).await;
        let events = l.event_count().await.unwrap();
        for t in &tasks {
            let refused = l.unpin(&t.id, RETIRED).await;
            assert!(is_wrong_state(&refused), "{}: {:?}", t.id, refused.err());
        }
        assert_eq!(l.event_count().await.unwrap(), events, "and no event");
        let done = l.get(&tasks[0].id).await.unwrap();
        assert_eq!(
            done.spec.position.as_deref(),
            Some(QA),
            "a done task keeps it"
        );
        assert_eq!(unpinned_events(b).await.len(), 1);
    }

    /// A task filed from `parent` under `origin`, which names how.
    fn filed_from_parent(origin: String, title: &str) -> TaskSpec {
        TaskSpec {
            origin: Some(origin),
            ..spec(title)
        }
    }

    async fn a_continuation_of_a_pinned_task_takes_its_pin(b: &Site) {
        let (mut l, _) = b.ledger().await;
        let separate = TaskSpec {
            pin_reason: Some(PinReason::Separation),
            ..pinned("Review", QA)
        };
        let parent = admitted(&mut l, separate).await;
        let cont = filed_from_parent(continues(&parent.id), "Resume");
        let cont = l.create(cont).await.unwrap().task;
        assert_eq!(cont.spec.position.as_deref(), Some(QA));
        assert_eq!(cont.spec.pin_reason, Some(PinReason::Separation));
        let own = TaskSpec {
            origin: Some(continues(&parent.id)),
            ..pinned("Names its own", DEV)
        };
        let own = l.create(own).await.unwrap().task;
        assert_eq!(own.spec.position.as_deref(), Some(DEV));
        assert_eq!(own.spec.pin_reason, Some(PinReason::Continuity));
        no_other_task_takes_a_pin(&mut l, &parent).await;
    }

    /// A subagent task and a retry of a pinned task, a continuation of an unpinned task,
    /// a continuation of a task the ledger does not hold, and a continuation of a pinned
    /// task that is filed in another domain: each is unpinned.
    async fn no_other_task_takes_a_pin(l: &mut Ledger, pinned_parent: &Task) {
        let plain = admitted(l, spec("Unpinned parent")).await;
        for origin in [
            format!("task:{}", pinned_parent.id),
            format!("retries:{}", pinned_parent.id),
            continues(&plain.id),
            continues("task-a-none"),
        ] {
            let filed = filed_from_parent(origin.clone(), "From a parent");
            let t = l.create(filed).await.unwrap().task;
            assert_eq!(
                (t.spec.position, t.spec.pin_reason),
                (None, None),
                "{origin}"
            );
        }
        // the position is one of the parent's domain: another domain holds none of that name
        let abroad = TaskSpec {
            domain: "dom-b".into(),
            ..filed_from_parent(continues(&pinned_parent.id), "Resume in another domain")
        };
        let abroad = l.create(abroad).await.unwrap().task;
        assert_eq!(
            (abroad.spec.position, abroad.spec.pin_reason),
            (None, None),
            "a continuation in another domain"
        );
    }

    async fn seated(l: &Ledger, position: &str) -> Vec<String> {
        l.seated("dom-a", position).await.unwrap()
    }

    async fn a_position_is_seated_while_a_claim_made_from_it_is_under_a_live_lease(b: &Site) {
        let (mut l, now) = b.ledger().await;
        let covers = ["review".to_string()];
        let qa = Some(Seat {
            position: QA,
            covers: &covers,
        });
        let t = admitted(&mut l, pinned("Review", QA)).await;
        let covered = admitted(&mut l, defined("Covered", "review@mem-x")).await;
        assert!(seated(&l, QA).await.is_empty(), "before any claim");
        claim_from(&mut l, &t.id, "s1", qa).await.unwrap();
        // a claimant in no position holds a task the charter covers: it is not seated
        claim_from(&mut l, &covered.id, "s2", None).await.unwrap();
        only_the_claimant_in_the_position_is_seated(&l).await;
        *now.lock().unwrap() = T0 + mins(30);
        assert_eq!(seated(&l, QA).await, ["s1"], "to the end of the lease");
        *now.lock().unwrap() = T0 + mins(31);
        assert!(seated(&l, QA).await.is_empty(), "the lease ran out");
        a_seat_is_left_at_a_release_and_at_a_completion(&mut l, &t, qa).await;
    }

    async fn only_the_claimant_in_the_position_is_seated(l: &Ledger) {
        assert_eq!(seated(l, QA).await, ["s1"]);
        assert!(seated(l, DEV).await.is_empty(), "another position");
        assert!(l.seated("dom-b", QA).await.unwrap().is_empty());
    }

    /// Two sessions in the position are both seated, each once; a release and a
    /// completion each take one away.
    async fn a_seat_is_left_at_a_release_and_at_a_completion(
        l: &mut Ledger,
        t: &Task,
        qa: Option<Seat<'_>>,
    ) {
        let more = admitted(l, pinned("More", QA)).await;
        let third = admitted(l, pinned("Third", QA)).await;
        claim_from(l, &t.id, "s4", qa).await.unwrap();
        claim_from(l, &more.id, "s3", qa).await.unwrap();
        claim_from(l, &third.id, "s3", qa).await.unwrap();
        assert_eq!(seated(l, QA).await, ["s3", "s4"], "sorted, each once");
        l.release_as(&t.id, "s4", None).await.unwrap();
        assert_eq!(seated(l, QA).await, ["s3"], "after a release");
        l.complete_as(&more.id, "s3", "mem-a-h").await.unwrap();
        l.complete_as(&third.id, "s3", "mem-a-h").await.unwrap();
        assert!(seated(l, QA).await.is_empty(), "after a completion");
    }

    /// The tasks pinned to `pos-a-qa` in `dom-a`, one in each state that is not closed,
    /// in the order the queue ranks them: open, claimed, blocked, proposed.
    async fn one_pinned_task_in_each_open_state(l: &mut Ledger) -> Vec<String> {
        let at = |priority: i64, title: &str| TaskSpec {
            priority,
            ..pinned(title, QA)
        };
        let proposed = l.create(at(1, "Proposed")).await.unwrap().task;
        let dep = admitted(l, spec("Not done")).await;
        let waits = TaskSpec {
            depends_on: vec![dep.id],
            ..at(2, "Blocked")
        };
        let blocked = admitted(l, waits).await;
        assert_eq!(blocked.status, Status::Blocked);
        let claimed = admitted(l, at(3, "Claimed")).await;
        claim_from(l, &claimed.id, "s1", seat(QA)).await.unwrap();
        let open = admitted(l, at(4, "Open")).await;
        vec![open.id, claimed.id, blocked.id, proposed.id]
    }

    /// What `pinned_to` must leave out: a done and an invalidated task of the position,
    /// and an unpinned task. Returns a task of another position and one of another domain.
    async fn tasks_that_are_not_the_position_s_open_ones(l: &mut Ledger) -> (Task, Task) {
        let done = admitted(l, pinned("Done", QA)).await;
        claim_from(l, &done.id, "s1", seat(QA)).await.unwrap();
        l.complete_as(&done.id, "s1", "mem-a-h").await.unwrap();
        let invalid = admitted(l, pinned("Invalidated", QA)).await;
        l.invalidate(&invalid.id, "obsolete", None).await.unwrap();
        admitted(l, spec("Unpinned")).await;
        let elsewhere = admitted(l, pinned("Another position", DEV)).await;
        let abroad = TaskSpec {
            domain: "dom-b".into(),
            ..pinned("Another domain", QA)
        };
        (elsewhere, admitted(l, abroad).await)
    }

    async fn pinned_ids(l: &Ledger, domain: &str, position: &str) -> Vec<String> {
        let tasks = l.pinned_to(domain, position).await.unwrap();
        tasks.into_iter().map(|t| t.id).collect()
    }

    async fn the_tasks_pinned_to_a_position_are_its_open_ones_in_queue_order(b: &Site) {
        let (mut l, _) = b.ledger().await;
        assert!(pinned_ids(&l, "dom-a", QA).await.is_empty());
        let expected = one_pinned_task_in_each_open_state(&mut l).await;
        let (elsewhere, abroad) = tasks_that_are_not_the_position_s_open_ones(&mut l).await;
        assert_eq!(pinned_ids(&l, "dom-a", QA).await, expected);
        assert_eq!(pinned_ids(&l, "dom-a", DEV).await, [elsewhere.id]);
        assert_eq!(pinned_ids(&l, "dom-b", QA).await, [abroad.id]);
    }

    /// A row as a ledger from before pins wrote it, with `extra` as given.
    fn row_from_before(extra: Option<&str>) -> RawTask {
        RawTask {
            id: "task-a-1".into(),
            domain: "dom-a".into(),
            title: "Old".into(),
            body: "Do it.\n".into(),
            requirements: "[]".into(),
            depends_on: "[]".into(),
            origin: None,
            upstream_event: None,
            created_by: "human:test:me".into(),
            label_rank: 0,
            priority: 0,
            created: "2026-10-01T10:00:00Z".into(),
            status: "open".into(),
            holder: None,
            lease_until: None,
            attempts: 0,
            handoff: None,
            reason: None,
            superseded_by: None,
            posted_to: None,
            extra: extra.map(str::to_string),
        }
    }

    #[test]
    fn a_task_and_a_claim_from_before_pins_read_as_unpinned() {
        let created = r#"{"kind":"created","id":"task-a-1","spec":{"domain":"dom-a","title":"Old","body":"Do it.\n","requirements":[],"depends_on":[],"origin":null,"upstream_event":null,"created_by":"human:test:me","classification":"public","priority":0,"definition":"review@mem-x"}}"#;
        let Event::Created { spec, .. } = Event::parse("task-a-1", created).unwrap() else {
            panic!("not a creation");
        };
        assert_eq!((spec.position, spec.pin_reason), (None, None));
        assert_eq!(spec.definition.as_deref(), Some("review@mem-x"));
        for extra in [
            None,
            Some(""),
            Some("{}"),
            Some(r#"{"definition":"review@mem-x"}"#),
        ] {
            let task = row_from_before(extra).into_task().unwrap();
            assert_eq!(
                (task.spec.position, task.spec.pin_reason),
                (None, None),
                "{extra:?}"
            );
        }
        let claimed =
            r#"{"kind":"claimed","holder":"s1","until":"2026-10-01T10:30:00Z","generation":1}"#;
        let Event::Claimed { position, .. } = Event::parse("task-a-1", claimed).unwrap() else {
            panic!("not a claim");
        };
        assert_eq!(position, None);
    }

    #[test]
    fn a_pin_reason_is_one_of_three_words() {
        for reason in [
            PinReason::Continuity,
            PinReason::Relationship,
            PinReason::Separation,
        ] {
            assert_eq!(reason.as_str().parse::<PinReason>().unwrap(), reason);
            let json = serde_json::to_string(&reason).unwrap();
            assert_eq!(json, format!("{:?}", reason.as_str()));
        }
        assert!(is_bad_pin(&"habit".parse::<PinReason>()));
    }
}
