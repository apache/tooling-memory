//! The task ledger: tasks, leases, handoffs, origin dedup.
//!
//! The ledger is event-sourced. Every operation appends one event to an append-only
//! table and updates a projection of current task state in the same transaction. The
//! projection is a cache: [`Ledger::rebuild`] drops it and replays the events, and a test
//! shows the result identical. That is the paper's "projection plus lease cache,
//! rebuildable from the log", with SQLite standing in for the log on a laptop.
//!
//! Three rules hold:
//!
//! - **A task is born proposed and becomes claimable only once admitted.** Anything may
//!   file a task; admission is a separate act.
//! - **One holder at a time.** A claim is one transaction over the ledger, so two callers
//!   racing for the same task cannot both win.
//! - **A lease that cannot be renewed is lost at its expiry**, for the holder and the
//!   ledger alike. An expired lease makes the task claimable again; the stale holder's
//!   heartbeat, release, and complete are refused.
//!
//! Time comes from a clock the ledger owns, so tests can move it.

use dm_core::Label;
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use std::path::Path;
use time::{format_description::well_known::Rfc3339, Duration, OffsetDateTime};

/// What can go wrong at the ledger.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// SQLite refused or failed.
    #[error("ledger: {0}")]
    Db(#[from] rusqlite::Error),
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
    /// Admitted and waiting for a holder.
    Admitted,
    /// Held under a lease.
    Claimed,
    /// Done, with a handoff.
    Completed,
    /// Closed without being done: obsolete, invalid, or a duplicate.
    Invalidated,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Proposed => "proposed",
            Status::Admitted => "admitted",
            Status::Claimed => "claimed",
            Status::Completed => "completed",
            Status::Invalidated => "invalidated",
        }
    }

    fn parse(id: &str, s: &str) -> Result<Status, Error> {
        Ok(match s {
            "proposed" => Status::Proposed,
            "admitted" => Status::Admitted,
            "claimed" => Status::Claimed,
            "completed" => Status::Completed,
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
        matches!(self, Status::Completed | Status::Invalidated)
    }
}

/// What a caller files. Everything else on a [`Task`] the ledger fills in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
    /// How many times it has been claimed.
    pub attempts: u32,
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
}

type Clock = Box<dyn Fn() -> OffsetDateTime + Send>;

/// The ledger, over one SQLite connection.
pub struct Ledger {
    conn: Connection,
    clock: Clock,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS events (
    seq      INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id  TEXT NOT NULL,
    kind     TEXT NOT NULL,
    at       TEXT NOT NULL,
    payload  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS events_task ON events (task_id, seq);
CREATE TABLE IF NOT EXISTS tasks (
    id             TEXT PRIMARY KEY,
    domain         TEXT NOT NULL,
    title          TEXT NOT NULL,
    body           TEXT NOT NULL,
    requirements   TEXT NOT NULL,
    depends_on     TEXT NOT NULL,
    origin         TEXT,
    upstream_event TEXT,
    created_by     TEXT NOT NULL,
    label_rank     INTEGER NOT NULL,
    priority       INTEGER NOT NULL,
    created        TEXT NOT NULL,
    status         TEXT NOT NULL,
    holder         TEXT,
    lease_until    TEXT,
    attempts       INTEGER NOT NULL DEFAULT 0,
    handoff        TEXT,
    reason         TEXT,
    superseded_by  TEXT,
    posted_to      TEXT
);
CREATE INDEX IF NOT EXISTS tasks_queue ON tasks (domain, status, priority DESC, created);
DROP INDEX IF EXISTS tasks_origin;
CREATE UNIQUE INDEX IF NOT EXISTS tasks_domain_origin ON tasks (domain, origin, upstream_event)
    WHERE origin IS NOT NULL AND upstream_event IS NOT NULL;
";

/// One appended event, as replay reads it.
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum Event {
    Created {
        id: String,
        spec: TaskSpec,
    },
    Admitted,
    Claimed {
        holder: String,
        #[serde(with = "time::serde::rfc3339")]
        until: OffsetDateTime,
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

const TASK_COLUMNS: &str =
    "id, domain, title, body, requirements, depends_on, origin, upstream_event,
    created_by, label_rank, priority, created, status, holder, lease_until, attempts, handoff,
    reason, superseded_by, posted_to";

fn task_from_row(row: &Row<'_>) -> Result<Task, Error> {
    let id: String = row.get(0)?;
    let json_list = |i: usize, what: &str| -> Result<Vec<String>, Error> {
        let s: String = row.get(i)?;
        serde_json::from_str(&s).map_err(|_| Error::BadRow {
            id: id.clone(),
            what: format!("{what} {s:?}"),
        })
    };
    let requirements = json_list(4, "requirements")?;
    let depends_on = json_list(5, "depends_on")?;
    let created: String = row.get(11)?;
    let status: String = row.get(12)?;
    let lease_until: Option<String> = row.get(14)?;
    let attempts: i64 = row.get(15)?;
    Ok(Task {
        spec: TaskSpec {
            domain: row.get(1)?,
            title: row.get(2)?,
            body: row.get(3)?,
            requirements,
            depends_on,
            origin: row.get(6)?,
            upstream_event: row.get(7)?,
            created_by: row.get(8)?,
            classification: label_from_rank(&id, row.get(9)?)?,
            priority: row.get(10)?,
        },
        created: parse_time(&id, &created)?,
        status: Status::parse(&id, &status)?,
        holder: row.get(13)?,
        lease_until: lease_until.map(|s| parse_time(&id, &s)).transpose()?,
        attempts: u32::try_from(attempts).unwrap_or(u32::MAX),
        handoff: row.get(16)?,
        reason: row.get(17)?,
        superseded_by: row.get(18)?,
        posted_to: row.get(19)?,
        id,
    })
}

impl Ledger {
    /// Open or create the ledger file.
    ///
    /// # Errors
    ///
    /// When the file cannot be opened or the schema cannot be created.
    pub fn open(path: &Path) -> Result<Ledger, Error> {
        Ledger::init(Connection::open(path)?)
    }

    /// A ledger that lives only in memory.
    ///
    /// # Errors
    ///
    /// When SQLite cannot create the schema, which it can always do in memory.
    pub fn in_memory() -> Result<Ledger, Error> {
        Ledger::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Ledger, Error> {
        conn.execute_batch("PRAGMA journal_mode = WAL;")?;
        conn.execute_batch(SCHEMA)?;
        // a ledger from before handoffs were posted back has no column for it
        let has_posted: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('tasks') WHERE name = 'posted_to'",
            [],
            |r| r.get(0),
        )?;
        if has_posted == 0 {
            conn.execute_batch("ALTER TABLE tasks ADD COLUMN posted_to TEXT;")?;
        }
        Ok(Ledger {
            conn,
            clock: Box::new(OffsetDateTime::now_utc),
        })
    }

    /// Replace the clock. Tests use this to let leases expire without waiting.
    pub fn set_clock(&mut self, clock: impl Fn() -> OffsetDateTime + Send + 'static) {
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
    pub fn create(&mut self, spec: TaskSpec) -> Result<Created, Error> {
        if let (Some(o), Some(u)) = (&spec.origin, &spec.upstream_event) {
            let existing: Option<String> = self
                .conn
                .query_row(
                    "SELECT id FROM tasks WHERE domain = ?1 AND origin = ?2 AND upstream_event = ?3",
                    params![spec.domain, o, u],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(id) = existing {
                return Ok(Created {
                    task: self.get(&id)?,
                    deduplicated: true,
                });
            }
        }
        let id = dm_core::id::new_task_id(&spec.domain);
        let now = self.now();
        let tx = self.conn.transaction()?;
        Ledger::apply(
            &tx,
            &id,
            now,
            &Event::Created {
                id: id.clone(),
                spec,
            },
        )?;
        tx.commit()?;
        Ok(Created {
            task: self.get(&id)?,
            deduplicated: false,
        })
    }

    /// Admit a proposed task, making it claimable.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`], or [`Error::WrongState`] if it is not proposed.
    pub fn admit(&mut self, id: &str) -> Result<Task, Error> {
        let task = self.get(id)?;
        if task.status != Status::Proposed {
            return Err(Error::WrongState {
                id: id.to_string(),
                status: task.status,
                op: "admit",
            });
        }
        self.append(id, &Event::Admitted)?;
        self.get(id)
    }

    /// Claim the highest-ranked claimable task in a domain whose requirements the
    /// manifest satisfies and whose dependencies are complete. Claimable means admitted,
    /// or claimed under a lease that has expired. One transaction, so one holder.
    ///
    /// # Errors
    ///
    /// A database error.
    pub fn claim_next(
        &mut self,
        domain: &str,
        holder: &str,
        lease: Duration,
        manifest: &[String],
    ) -> Result<Option<Claim>, Error> {
        let now = self.now();
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let candidates: Vec<Task> = {
            let mut stmt = tx.prepare(&format!(
                "SELECT {TASK_COLUMNS} FROM tasks
                 WHERE domain = ?1 AND (status = 'admitted' OR (status = 'claimed' AND lease_until < ?2))
                 ORDER BY priority DESC, created"
            ))?;
            let rows = stmt.query_map(params![domain, fmt_time(now)], |r| Ok(task_from_row(r)))?;
            rows.map(|r| r?).collect::<Result<_, _>>()?
        };
        for task in candidates {
            if !task.spec.requirements.iter().all(|r| manifest.contains(r)) {
                continue;
            }
            if !Ledger::deps_complete(&tx, &task.spec.depends_on)? {
                continue;
            }
            let until = now + lease;
            Ledger::apply(
                &tx,
                &task.id,
                now,
                &Event::Claimed {
                    holder: holder.to_string(),
                    until,
                },
            )?;
            tx.commit()?;
            return Ok(Some(Claim {
                task: self.get(&task.id)?,
                lease_until: until,
            }));
        }
        Ok(None)
    }

    /// Claim one named task, under the same rules as [`Ledger::claim_next`].
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`]; [`Error::WrongState`] when it is not claimable, including when
    /// another holder's lease is still live or a dependency is open.
    pub fn claim(
        &mut self,
        id: &str,
        holder: &str,
        lease: Duration,
        manifest: &[String],
    ) -> Result<Claim, Error> {
        let now = self.now();
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let task = Ledger::get_in(&tx, id)?;
        let lease_live = task.lease_until.is_some_and(|t| t >= now);
        let claimable =
            task.status == Status::Admitted || (task.status == Status::Claimed && !lease_live);
        let satisfied = task.spec.requirements.iter().all(|r| manifest.contains(r))
            && Ledger::deps_complete(&tx, &task.spec.depends_on)?;
        if !claimable || !satisfied {
            return Err(Error::WrongState {
                id: id.to_string(),
                status: task.status,
                op: "claim",
            });
        }
        let until = now + lease;
        Ledger::apply(
            &tx,
            id,
            now,
            &Event::Claimed {
                holder: holder.to_string(),
                until,
            },
        )?;
        tx.commit()?;
        Ok(Claim {
            task: self.get(id)?,
            lease_until: until,
        })
    }

    /// Renew a lease. Refused when the caller does not hold the task or the lease has
    /// already expired, in which case the task is back in the pool and the caller must
    /// stop. Returns the task so a caller can see an invalidation.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`], [`Error::NotHolder`], or [`Error::WrongState`] when terminal.
    pub fn heartbeat(&mut self, id: &str, holder: &str, lease: Duration) -> Result<Task, Error> {
        let now = self.now();
        let task = self.get(id)?;
        if task.status.is_terminal() {
            return Err(Error::WrongState {
                id: id.to_string(),
                status: task.status,
                op: "heartbeat",
            });
        }
        Ledger::check_holder(&task, holder, now)?;
        self.append_at(id, now, &Event::Heartbeat { until: now + lease })?;
        self.get(id)
    }

    /// Close a task with the handoff entry that records what happened.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`], [`Error::NotHolder`], or [`Error::WrongState`].
    pub fn complete(&mut self, id: &str, holder: &str, handoff: &str) -> Result<Task, Error> {
        let now = self.now();
        let task = self.get(id)?;
        if task.status != Status::Claimed {
            return Err(Error::WrongState {
                id: id.to_string(),
                status: task.status,
                op: "complete",
            });
        }
        Ledger::check_holder(&task, holder, now)?;
        self.append_at(
            id,
            now,
            &Event::Completed {
                handoff: handoff.to_string(),
            },
        )?;
        self.get(id)
    }

    /// Give a task back to the pool.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`], [`Error::NotHolder`], or [`Error::WrongState`].
    pub fn release(
        &mut self,
        id: &str,
        holder: &str,
        handoff: Option<&str>,
    ) -> Result<Task, Error> {
        let now = self.now();
        let task = self.get(id)?;
        if task.status != Status::Claimed {
            return Err(Error::WrongState {
                id: id.to_string(),
                status: task.status,
                op: "release",
            });
        }
        Ledger::check_holder(&task, holder, now)?;
        if let Some(handoff) = handoff {
            self.append_at(
                id,
                now,
                &Event::HandoffLeft {
                    handoff: handoff.to_string(),
                },
            )?;
        }
        self.append_at(id, now, &Event::Released)?;
        self.get(id)
    }

    /// Close a task without doing it. A live holder learns of it at the next heartbeat.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`], or [`Error::WrongState`] when already terminal.
    pub fn invalidate(
        &mut self,
        id: &str,
        reason: &str,
        superseded_by: Option<&str>,
    ) -> Result<Task, Error> {
        let task = self.get(id)?;
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
        )?;
        self.get(id)
    }

    /// One task.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`], or a row the ledger did not write.
    pub fn get(&self, id: &str) -> Result<Task, Error> {
        Ledger::get_in(&self.conn, id)
    }

    /// Tasks in a domain with the given status, ranked as the queue ranks them.
    ///
    /// # Errors
    ///
    /// A database error, or a row the ledger did not write.
    pub fn list(&self, domain: &str, status: Status) -> Result<Vec<Task>, Error> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {TASK_COLUMNS} FROM tasks WHERE domain = ?1 AND status = ?2
             ORDER BY priority DESC, created"
        ))?;
        let rows = stmt.query_map(params![domain, status.as_str()], |r| Ok(task_from_row(r)))?;
        rows.map(|r| r?).collect()
    }

    /// Record that a completed task's handoff was posted back where the task came from,
    /// at `to`, so it is not posted twice.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`], or [`Error::WrongState`] when the task is not completed or
    /// was posted already.
    pub fn post(&mut self, id: &str, to: &str) -> Result<Task, Error> {
        let task = self.get(id)?;
        if task.status != Status::Completed || task.posted_to.is_some() {
            return Err(Error::WrongState {
                id: id.to_string(),
                status: task.status,
                op: "post",
            });
        }
        let now = self.now();
        self.append_at(id, now, &Event::Posted { to: to.to_string() })?;
        self.get(id)
    }

    /// Completed tasks in a domain that came from upstream and whose handoff has not
    /// been posted back, oldest first.
    ///
    /// # Errors
    ///
    /// A database error, or a row the ledger did not write.
    pub fn unposted(&self, domain: &str) -> Result<Vec<Task>, Error> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {TASK_COLUMNS} FROM tasks
             WHERE domain = ?1 AND status = 'completed' AND origin IS NOT NULL
               AND posted_to IS NULL
             ORDER BY created"
        ))?;
        let rows = stmt.query_map(params![domain], |r| Ok(task_from_row(r)))?;
        rows.map(|r| r?).collect()
    }

    /// Tasks that depend on this one.
    ///
    /// # Errors
    ///
    /// A database error, or a row the ledger did not write.
    pub fn dependents(&self, id: &str) -> Result<Vec<Task>, Error> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {TASK_COLUMNS} FROM tasks WHERE id IN (
                 SELECT t.id FROM tasks t, json_each(t.depends_on) AS d WHERE d.value = ?1)
             ORDER BY created"
        ))?;
        let rows = stmt.query_map(params![id], |r| Ok(task_from_row(r)))?;
        rows.map(|r| r?).collect()
    }

    /// How many events the ledger holds.
    ///
    /// # Errors
    ///
    /// A database error.
    pub fn event_count(&self) -> Result<u64, Error> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))?;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    /// Drop the projection and replay every event into a fresh one. Returns the number
    /// of events replayed.
    ///
    /// # Errors
    ///
    /// A database error; the projection is left as it was.
    pub fn rebuild(&mut self) -> Result<u64, Error> {
        let tx = self.conn.transaction()?;
        tx.execute_batch("DELETE FROM tasks;")?;
        let events: Vec<(String, String, String)> = {
            let mut stmt = tx.prepare("SELECT task_id, at, payload FROM events ORDER BY seq")?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            rows.collect::<Result<_, _>>()?
        };
        let mut n = 0;
        for (task_id, at, payload) in events {
            let at = parse_time(&task_id, &at)?;
            let event: Event = serde_json::from_str(&payload).map_err(|e| Error::BadRow {
                id: task_id.clone(),
                what: format!("event payload: {e}"),
            })?;
            Ledger::project(&tx, &task_id, at, &event)?;
            n += 1;
        }
        tx.commit()?;
        Ok(n)
    }

    fn check_holder(task: &Task, holder: &str, now: OffsetDateTime) -> Result<(), Error> {
        let held = task.holder.as_deref() == Some(holder);
        let live = task.lease_until.is_some_and(|t| t >= now);
        if held && live {
            Ok(())
        } else {
            Err(Error::NotHolder {
                id: task.id.clone(),
                holder: holder.to_string(),
            })
        }
    }

    fn deps_complete(conn: &Connection, deps: &[String]) -> Result<bool, Error> {
        for d in deps {
            let status: Option<String> = conn
                .query_row("SELECT status FROM tasks WHERE id = ?1", params![d], |r| {
                    r.get(0)
                })
                .optional()?;
            if status.as_deref() != Some("completed") {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn get_in(conn: &Connection, id: &str) -> Result<Task, Error> {
        conn.query_row(
            &format!("SELECT {TASK_COLUMNS} FROM tasks WHERE id = ?1"),
            params![id],
            |r| Ok(task_from_row(r)),
        )
        .optional()?
        .ok_or_else(|| Error::NotFound(id.to_string()))?
    }

    fn append(&mut self, id: &str, event: &Event) -> Result<(), Error> {
        let now = self.now();
        self.append_at(id, now, event)
    }

    fn append_at(&mut self, id: &str, at: OffsetDateTime, event: &Event) -> Result<(), Error> {
        let tx = self.conn.transaction()?;
        Ledger::apply(&tx, id, at, event)?;
        tx.commit()?;
        Ok(())
    }

    /// Append the event and project it, in the caller's transaction.
    fn apply(conn: &Connection, id: &str, at: OffsetDateTime, event: &Event) -> Result<(), Error> {
        let payload = serde_json::to_string(event).map_err(|e| Error::BadRow {
            id: id.to_string(),
            what: format!("event serialises: {e}"),
        })?;
        let kind = match event {
            Event::Created { .. } => "created",
            Event::Admitted => "admitted",
            Event::Claimed { .. } => "claimed",
            Event::Heartbeat { .. } => "heartbeat",
            Event::Released => "released",
            Event::HandoffLeft { .. } => "handoff_left",
            Event::Completed { .. } => "completed",
            Event::Invalidated { .. } => "invalidated",
            Event::Posted { .. } => "posted",
        };
        conn.execute(
            "INSERT INTO events (task_id, kind, at, payload) VALUES (?1, ?2, ?3, ?4)",
            params![id, kind, fmt_time(at), payload],
        )?;
        Ledger::project(conn, id, at, event)
    }

    /// Update the projection for one event. Replay calls this and nothing else.
    /// The row a `Created` event projects to: a task born proposed.
    fn insert_task(
        conn: &Connection,
        id: &str,
        at: OffsetDateTime,
        spec: &TaskSpec,
    ) -> Result<(), Error> {
        let requirements = serde_json::to_string(&spec.requirements).unwrap_or_default();
        let depends_on = serde_json::to_string(&spec.depends_on).unwrap_or_default();
        conn.execute(
            "INSERT INTO tasks (id, domain, title, body, requirements, depends_on, origin,
                upstream_event, created_by, label_rank, priority, created, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 'proposed')",
            params![
                id,
                spec.domain,
                spec.title,
                spec.body,
                requirements,
                depends_on,
                spec.origin,
                spec.upstream_event,
                spec.created_by,
                spec.classification as i64,
                spec.priority,
                fmt_time(at),
            ],
        )?;
        Ok(())
    }

    fn project(
        conn: &Connection,
        id: &str,
        at: OffsetDateTime,
        event: &Event,
    ) -> Result<(), Error> {
        match event {
            Event::Created { id: new_id, spec } => Ledger::insert_task(conn, new_id, at, spec)?,
            Event::Admitted => {
                conn.execute(
                    "UPDATE tasks SET status = 'admitted' WHERE id = ?1",
                    params![id],
                )?;
            }
            Event::Claimed { holder, until } => {
                conn.execute(
                    "UPDATE tasks SET status = 'claimed', holder = ?2, lease_until = ?3,
                        attempts = attempts + 1 WHERE id = ?1",
                    params![id, holder, fmt_time(*until)],
                )?;
            }
            Event::Heartbeat { until } => {
                conn.execute(
                    "UPDATE tasks SET lease_until = ?2 WHERE id = ?1",
                    params![id, fmt_time(*until)],
                )?;
            }
            Event::Released => {
                conn.execute(
                    "UPDATE tasks SET status = 'admitted', holder = NULL, lease_until = NULL
                     WHERE id = ?1",
                    params![id],
                )?;
            }
            Event::HandoffLeft { handoff } => {
                conn.execute(
                    "UPDATE tasks SET handoff = ?2 WHERE id = ?1",
                    params![id, handoff],
                )?;
            }
            Event::Completed { handoff } => {
                conn.execute(
                    "UPDATE tasks SET status = 'completed', handoff = ?2, lease_until = NULL
                     WHERE id = ?1",
                    params![id, handoff],
                )?;
            }
            Event::Invalidated {
                reason,
                superseded_by,
            } => {
                conn.execute(
                    "UPDATE tasks SET status = 'invalidated', reason = ?2, superseded_by = ?3,
                        lease_until = NULL WHERE id = ?1",
                    params![id, reason, superseded_by],
                )?;
            }
            Event::Posted { to } => {
                conn.execute(
                    "UPDATE tasks SET posted_to = ?2 WHERE id = ?1",
                    params![id, to],
                )?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use time::macros::datetime;

    const T0: OffsetDateTime = datetime!(2026-10-01 10:00 UTC);

    fn ledger() -> (Ledger, Arc<Mutex<OffsetDateTime>>) {
        let now = Arc::new(Mutex::new(T0));
        let mut l = Ledger::in_memory().unwrap();
        let c = now.clone();
        l.set_clock(move || *c.lock().unwrap());
        (l, now)
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
        }
    }

    fn admitted(l: &mut Ledger, s: TaskSpec) -> Task {
        let t = l.create(s).unwrap().task;
        l.admit(&t.id).unwrap()
    }

    fn mins(n: i64) -> Duration {
        Duration::minutes(n)
    }

    #[test]
    fn a_task_is_proposed_until_admitted_and_then_claimable_once() {
        let (mut l, _) = ledger();
        let t = l.create(spec("Fix the flaky test")).unwrap().task;
        assert_eq!(t.status, Status::Proposed);
        assert!(l
            .claim_next("dom-a", "s1", mins(30), &[])
            .unwrap()
            .is_none());
        l.admit(&t.id).unwrap();
        assert!(matches!(l.admit(&t.id), Err(Error::WrongState { .. })));
        let c = l.claim_next("dom-a", "s1", mins(30), &[]).unwrap().unwrap();
        assert_eq!(c.task.id, t.id);
        assert_eq!(c.task.status, Status::Claimed);
        assert_eq!(c.task.holder.as_deref(), Some("s1"));
        assert_eq!(c.lease_until, T0 + mins(30));
        assert_eq!(c.task.attempts, 1);
        assert!(
            l.claim_next("dom-a", "s2", mins(30), &[])
                .unwrap()
                .is_none(),
            "one holder"
        );
        assert!(matches!(
            l.claim(&t.id, "s2", mins(30), &[]),
            Err(Error::WrongState { .. })
        ));
    }

    #[test]
    fn heartbeat_renews_only_for_the_holder_and_complete_closes() {
        let (mut l, now) = ledger();
        let t = admitted(&mut l, spec("Work"));
        l.claim(&t.id, "s1", mins(30), &[]).unwrap();
        *now.lock().unwrap() = T0 + mins(20);
        let t = l.heartbeat(&t.id, "s1", mins(30)).unwrap();
        assert_eq!(t.lease_until, Some(T0 + mins(50)));
        assert!(matches!(
            l.heartbeat(&t.id, "s2", mins(30)),
            Err(Error::NotHolder { .. })
        ));
        assert!(matches!(
            l.complete(&t.id, "s2", "mem-a-h"),
            Err(Error::NotHolder { .. })
        ));
        let done = l.complete(&t.id, "s1", "mem-a-handoff").unwrap();
        assert_eq!(done.status, Status::Completed);
        assert_eq!(done.handoff.as_deref(), Some("mem-a-handoff"));
        assert!(done.lease_until.is_none());
        assert!(matches!(
            l.heartbeat(&t.id, "s1", mins(30)),
            Err(Error::WrongState { .. })
        ));
        assert!(matches!(
            l.complete(&t.id, "s1", "again"),
            Err(Error::WrongState { .. })
        ));
        assert_eq!(l.list("dom-a", Status::Completed).unwrap().len(), 1);
    }

    #[test]
    fn an_unrenewed_lease_is_lost_at_expiry_for_both_sides() {
        let (mut l, now) = ledger();
        let t = admitted(&mut l, spec("Long job"));
        l.claim(&t.id, "s1", mins(30), &[]).unwrap();
        *now.lock().unwrap() = T0 + mins(29);
        assert!(
            l.claim_next("dom-a", "s2", mins(30), &[])
                .unwrap()
                .is_none(),
            "still held"
        );
        *now.lock().unwrap() = T0 + mins(31);
        let c = l.claim_next("dom-a", "s2", mins(30), &[]).unwrap().unwrap();
        assert_eq!(c.task.id, t.id);
        assert_eq!(c.task.holder.as_deref(), Some("s2"));
        assert_eq!(c.task.attempts, 2);
        assert!(
            matches!(
                l.heartbeat(&t.id, "s1", mins(30)),
                Err(Error::NotHolder { .. })
            ),
            "stale holder"
        );
        assert!(matches!(
            l.complete(&t.id, "s1", "mem-a-h"),
            Err(Error::NotHolder { .. })
        ));
        assert!(matches!(
            l.release(&t.id, "s1", None),
            Err(Error::NotHolder { .. })
        ));
        // s2's own lease also expires if never renewed
        *now.lock().unwrap() = T0 + mins(62);
        assert!(matches!(
            l.heartbeat(&t.id, "s2", mins(30)),
            Err(Error::NotHolder { .. })
        ));
    }

    #[test]
    fn requirements_match_the_manifest_and_priority_orders_the_queue() {
        let (mut l, _) = ledger();
        let mut needs_tool = spec("Needs github");
        needs_tool.requirements = vec!["tool:github".into()];
        needs_tool.priority = 10;
        let needs_tool = admitted(&mut l, needs_tool);
        let mut low = spec("Low");
        low.priority = 1;
        let low = admitted(&mut l, low);
        let mut high = spec("High");
        high.priority = 5;
        let high = admitted(&mut l, high);

        let c = l.claim_next("dom-a", "s1", mins(5), &[]).unwrap().unwrap();
        assert_eq!(c.task.id, high.id, "highest priority the manifest can take");
        let c = l
            .claim_next("dom-a", "s1", mins(5), &["tool:github".into()])
            .unwrap()
            .unwrap();
        assert_eq!(c.task.id, needs_tool.id);
        let c = l.claim_next("dom-a", "s1", mins(5), &[]).unwrap().unwrap();
        assert_eq!(c.task.id, low.id);
        assert!(matches!(
            l.claim(&needs_tool.id, "s3", mins(5), &[]),
            Err(Error::WrongState { .. })
        ));
    }

    #[test]
    fn dependencies_block_until_complete() {
        let (mut l, _) = ledger();
        let parent = admitted(&mut l, spec("Parent"));
        let mut child_spec = spec("Child");
        child_spec.depends_on = vec![parent.id.clone()];
        child_spec.priority = 100;
        let child = admitted(&mut l, child_spec);
        assert_eq!(l.dependents(&parent.id).unwrap()[0].id, child.id);
        let c = l.claim_next("dom-a", "s1", mins(5), &[]).unwrap().unwrap();
        assert_eq!(c.task.id, parent.id, "the child outranks it but is blocked");
        assert!(matches!(
            l.claim(&child.id, "s2", mins(5), &[]),
            Err(Error::WrongState { .. })
        ));
        l.complete(&parent.id, "s1", "mem-a-h1").unwrap();
        let c = l.claim_next("dom-a", "s1", mins(5), &[]).unwrap().unwrap();
        assert_eq!(c.task.id, child.id);
    }

    #[test]
    fn release_returns_to_pool_and_invalidate_is_terminal() {
        let (mut l, _) = ledger();
        let t = admitted(&mut l, spec("Give back"));
        l.claim(&t.id, "s1", mins(5), &[]).unwrap();
        let t = l.release(&t.id, "s1", Some("mem-x-left")).unwrap();
        assert_eq!(
            t.handoff.as_deref(),
            Some("mem-x-left"),
            "the handoff rides with the released task"
        );
        assert_eq!(t.status, Status::Admitted);
        assert!(t.holder.is_none());
        let c = l.claim_next("dom-a", "s2", mins(5), &[]).unwrap().unwrap();
        assert_eq!(c.task.attempts, 2);
        let t = l
            .invalidate(&t.id, "duplicate", Some("task-a-other"))
            .unwrap();
        assert_eq!(t.status, Status::Invalidated);
        assert_eq!(t.reason.as_deref(), Some("duplicate"));
        assert_eq!(t.superseded_by.as_deref(), Some("task-a-other"));
        assert!(
            matches!(
                l.heartbeat(&t.id, "s2", mins(5)),
                Err(Error::WrongState { .. })
            ),
            "holder learns at heartbeat"
        );
        assert!(matches!(
            l.invalidate(&t.id, "again", None),
            Err(Error::WrongState { .. })
        ));
        assert!(l.claim_next("dom-a", "s3", mins(5), &[]).unwrap().is_none());
    }

    #[test]
    fn origin_and_upstream_event_dedup_a_second_filing() {
        let (mut l, _) = ledger();
        let mut s = spec("From an issue");
        s.origin = Some("github:apache/x#12".into());
        s.upstream_event = Some("labeled:2026-10-01T09:00:00Z".into());
        let first = l.create(s.clone()).unwrap();
        assert!(!first.deduplicated);
        let again = l.create(s.clone()).unwrap();
        assert!(again.deduplicated);
        assert_eq!(again.task.id, first.task.id);
        s.upstream_event = Some("labeled:2026-10-02T09:00:00Z".into());
        let other = l.create(s).unwrap();
        assert!(!other.deduplicated, "a new upstream event is a new task");
        assert_ne!(other.task.id, first.task.id);
        assert_eq!(
            l.event_count().unwrap(),
            2,
            "the duplicate appended nothing"
        );
        // the same issue filed into another domain is that domain's task: dedup is
        // scoped to the domain, since one domain's ledger says nothing about another's
        let mut elsewhere = spec("From the same issue, elsewhere");
        elsewhere.domain = "dom-b".into();
        elsewhere.origin = Some("github:apache/x#12".into());
        elsewhere.upstream_event = Some("labeled:2026-10-01T09:00:00Z".into());
        let theirs = l.create(elsewhere.clone()).unwrap();
        assert!(
            !theirs.deduplicated,
            "another domain's filing is not a duplicate"
        );
        assert_ne!(theirs.task.id, first.task.id);
        assert_eq!(theirs.task.spec.domain, "dom-b");
        assert!(l.create(elsewhere).unwrap().deduplicated);
        assert_eq!(l.event_count().unwrap(), 3);
    }

    #[test]
    fn every_operation_appends_one_event_and_replay_rebuilds_the_projection() {
        let (mut l, now) = ledger();
        let a = l.create(spec("A")).unwrap().task;
        l.admit(&a.id).unwrap();
        l.claim(&a.id, "s1", mins(5), &[]).unwrap();
        *now.lock().unwrap() = T0 + mins(2);
        l.heartbeat(&a.id, "s1", mins(5)).unwrap();
        l.release(&a.id, "s1", None).unwrap();
        l.claim(&a.id, "s2", mins(5), &[]).unwrap();
        l.complete(&a.id, "s2", "mem-a-h").unwrap();
        let b = l.create(spec("B")).unwrap().task;
        l.invalidate(&b.id, "obsolete", None).unwrap();
        assert_eq!(l.event_count().unwrap(), 9);

        let before: Vec<Task> = [&a.id, &b.id].iter().map(|id| l.get(id).unwrap()).collect();
        assert_eq!(l.rebuild().unwrap(), 9);
        let after: Vec<Task> = [&a.id, &b.id].iter().map(|id| l.get(id).unwrap()).collect();
        assert_eq!(
            before, after,
            "the projection is a pure function of the events"
        );
        assert_eq!(after[0].attempts, 2);
        assert_eq!(after[0].status, Status::Completed);
        assert_eq!(after[1].status, Status::Invalidated);
    }

    #[test]
    fn not_found_and_persistence() {
        let (l, _) = ledger();
        assert!(matches!(l.get("task-a-none"), Err(Error::NotFound(_))));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");
        let id = {
            let mut l = Ledger::open(&path).unwrap();
            admitted(&mut l, spec("Persist")).id
        };
        let l = Ledger::open(&path).unwrap();
        assert_eq!(l.get(&id).unwrap().status, Status::Admitted);
        assert_eq!(l.list("dom-a", Status::Admitted).unwrap().len(), 1);
    }

    #[test]
    fn a_completed_task_from_upstream_is_posted_back_once() {
        let (mut l, _) = ledger();
        let mut s = spec("From an issue");
        s.origin = Some("github:acme/widgets#12".into());
        s.upstream_event = Some("open:2026-10-01T09:00:00Z".into());
        let from_upstream = l.create(s).unwrap().task;
        let local = l.create(spec("Filed by hand")).unwrap().task;
        for t in [&from_upstream, &local] {
            l.admit(&t.id).unwrap();
            l.claim(&t.id, "s1", mins(5), &[]).unwrap();
        }
        assert!(
            l.unposted("dom-a").unwrap().is_empty(),
            "nothing is done yet"
        );
        assert!(matches!(
            l.post(&from_upstream.id, "https://x/1"),
            Err(Error::WrongState { op: "post", .. })
        ));
        l.complete(&from_upstream.id, "s1", "mem-a-h1").unwrap();
        l.complete(&local.id, "s1", "mem-a-h2").unwrap();
        let due: Vec<String> = l
            .unposted("dom-a")
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(
            due,
            vec![from_upstream.id.clone()],
            "only what came from upstream"
        );
        assert!(l.unposted("dom-b").unwrap().is_empty());
        let posted = l
            .post(
                &from_upstream.id,
                "https://github.com/acme/widgets/issues/12#c1",
            )
            .unwrap();
        assert_eq!(
            posted.posted_to.as_deref(),
            Some("https://github.com/acme/widgets/issues/12#c1")
        );
        assert!(l.unposted("dom-a").unwrap().is_empty());
        assert!(matches!(
            l.post(&from_upstream.id, "https://x/2"),
            Err(Error::WrongState { op: "post", .. })
        ));
        let events = l.event_count().unwrap();
        l.rebuild().unwrap();
        assert_eq!(l.event_count().unwrap(), events);
        assert_eq!(l.get(&from_upstream.id).unwrap(), posted, "replay keeps it");
    }
}
