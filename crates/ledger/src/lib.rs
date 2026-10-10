//! The task ledger: tasks, leases, handoffs, origin dedup.
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
//! Time comes from a clock the ledger owns, so tests can move it.

mod postgres;
mod sqlite;
#[cfg(any(test, feature = "pg-test"))]
pub mod testing;

use dm_core::Label;
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

impl Event {
    /// The `kind` column: what the event is, readable without the payload.
    fn kind(&self) -> &'static str {
        match self {
            Event::Created { .. } => "created",
            Event::Admitted => "admitted",
            Event::Claimed { .. } => "claimed",
            Event::Heartbeat { .. } => "heartbeat",
            Event::Released => "released",
            Event::HandoffLeft { .. } => "handoff_left",
            Event::Completed { .. } => "completed",
            Event::Invalidated { .. } => "invalidated",
            Event::Posted { .. } => "posted",
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

/// The columns of a task row, in the order both backends read them.
const TASK_COLUMNS: &str =
    "id, domain, title, body, requirements, depends_on, origin, upstream_event,
    created_by, label_rank, priority, created, status, holder, lease_until, attempts, handoff,
    reason, superseded_by, posted_to";

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
            },
            created: parse_time(&id, &self.created)?,
            status: Status::parse(&id, &self.status)?,
            holder: self.holder,
            lease_until: self.lease_until.map(|s| parse_time(&id, &s)).transpose()?,
            attempts: u32::try_from(self.attempts).unwrap_or(u32::MAX),
            handoff: self.handoff,
            reason: self.reason,
            superseded_by: self.superseded_by,
            posted_to: self.posted_to,
            id,
        })
    }
}

/// Whether a manifest may claim `task` now: it is admitted, or claimed under a lease
/// that has run out, and the manifest carries every requirement. Whether its
/// dependencies are complete is the backend's question, asked inside the transaction.
fn claimable(task: &Task, now: OffsetDateTime, manifest: &[String]) -> bool {
    let lease_live = task.lease_until.is_some_and(|t| t >= now);
    let state = task.status == Status::Admitted || (task.status == Status::Claimed && !lease_live);
    state && task.spec.requirements.iter().all(|r| manifest.contains(r))
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
        self.append(id, &Event::Admitted).await?;
        self.get(id).await
    }

    /// Claim the highest-ranked claimable task in a domain whose requirements the
    /// manifest satisfies and whose dependencies are complete. Claimable means admitted,
    /// or claimed under a lease that has expired. One transaction, so one holder.
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
    ) -> Result<Option<Claim>, Error> {
        let now = self.now();
        let until = now + lease;
        let claimed = on_backend!(&mut self.backend, db => db.claim_next(domain, now, holder, until, manifest))?;
        match claimed {
            Some(id) => Ok(Some(Claim {
                task: self.get(&id).await?,
                lease_until: until,
            })),
            None => Ok(None),
        }
    }

    /// Claim one named task, under the same rules as [`Ledger::claim_next`].
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`]; [`Error::WrongState`] when it is not claimable, including when
    /// another holder's lease is still live or a dependency is open.
    pub async fn claim(
        &mut self,
        id: &str,
        holder: &str,
        lease: Duration,
        manifest: &[String],
    ) -> Result<Claim, Error> {
        let now = self.now();
        let until = now + lease;
        on_backend!(&mut self.backend, db => db.claim(id, now, holder, until, manifest))?;
        Ok(Claim {
            task: self.get(id).await?,
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
    pub async fn heartbeat(
        &mut self,
        id: &str,
        holder: &str,
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
        check_holder(&task, holder, now)?;
        self.append_at(id, now, &Event::Heartbeat { until: now + lease })
            .await?;
        self.get(id).await
    }

    /// Close a task with the handoff entry that records what happened.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`], [`Error::NotHolder`], or [`Error::WrongState`].
    pub async fn complete(&mut self, id: &str, holder: &str, handoff: &str) -> Result<Task, Error> {
        let now = self.now();
        let task = self.get(id).await?;
        if task.status != Status::Claimed {
            return Err(Error::WrongState {
                id: id.to_string(),
                status: task.status,
                op: "complete",
            });
        }
        check_holder(&task, holder, now)?;
        self.append_at(
            id,
            now,
            &Event::Completed {
                handoff: handoff.to_string(),
            },
        )
        .await?;
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
        holder: &str,
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
        check_holder(&task, holder, now)?;
        if let Some(handoff) = handoff {
            self.append_at(
                id,
                now,
                &Event::HandoffLeft {
                    handoff: handoff.to_string(),
                },
            )
            .await?;
        }
        self.append_at(id, now, &Event::Released).await?;
        self.get(id).await
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
        self.get(id).await
    }

    /// One task.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`], or a row the ledger did not write.
    pub async fn get(&self, id: &str) -> Result<Task, Error> {
        on_backend!(&self.backend, db => db.get(id))
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
        if task.status != Status::Completed || task.posted_to.is_some() {
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

    /// Append one event and project it, in one transaction.
    async fn append_at(
        &mut self,
        id: &str,
        at: OffsetDateTime,
        event: &Event,
    ) -> Result<(), Error> {
        on_backend!(&mut self.backend, db => db.append(id, at, event))
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
        a_task_is_proposed_until_admitted_and_then_claimable_once,
        heartbeat_renews_only_for_the_holder_and_complete_closes,
        an_unrenewed_lease_is_lost_at_expiry_for_both_sides,
        requirements_match_the_manifest_and_priority_orders_the_queue,
        dependencies_block_until_complete,
        release_returns_to_pool_and_invalidate_is_terminal,
        origin_and_upstream_event_dedup_a_second_filing,
        what_was_filed_from_a_task_is_found_by_its_origin,
        every_operation_appends_one_event_and_replay_rebuilds_the_projection,
        not_found_and_persistence,
        a_completed_task_from_upstream_is_posted_back_once,
        two_claimers_on_two_connections_get_two_tasks,
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

    async fn admitted(l: &mut Ledger, s: TaskSpec) -> Task {
        let t = l.create(s).await.unwrap().task;
        l.admit(&t.id).await.unwrap()
    }

    fn mins(n: i64) -> Duration {
        Duration::minutes(n)
    }

    /// `claim_next` in `dom-a` with an empty manifest.
    async fn next(l: &mut Ledger, holder: &str, lease: Duration) -> Option<Claim> {
        l.claim_next("dom-a", holder, lease, &[]).await.unwrap()
    }

    /// `claim` with an empty manifest.
    async fn claim(
        l: &mut Ledger,
        id: &str,
        holder: &str,
        lease: Duration,
    ) -> Result<Claim, Error> {
        l.claim(id, holder, lease, &[]).await
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
        assert_eq!(c.task.attempts, 1);
        assert!(next(l, "s2", mins(30)).await.is_none(), "one holder");
        assert!(is_wrong_state(&claim(l, &t.id, "s2", mins(30)).await));
    }

    async fn heartbeat_renews_only_for_the_holder_and_complete_closes(b: &Site) {
        let (mut l, now) = b.ledger().await;
        let t = admitted(&mut l, spec("Work")).await;
        claim(&mut l, &t.id, "s1", mins(30)).await.unwrap();
        *now.lock().unwrap() = T0 + mins(20);
        let t = l.heartbeat(&t.id, "s1", mins(30)).await.unwrap();
        assert_eq!(t.lease_until, Some(T0 + mins(50)));
        assert!(is_not_holder(&l.heartbeat(&t.id, "s2", mins(30)).await));
        assert!(is_not_holder(&l.complete(&t.id, "s2", "mem-a-h").await));
        complete_closes(&mut l, &t).await;
    }

    async fn complete_closes(l: &mut Ledger, t: &Task) {
        let done = l.complete(&t.id, "s1", "mem-a-handoff").await.unwrap();
        assert_eq!(done.status, Status::Completed);
        assert_eq!(done.handoff.as_deref(), Some("mem-a-handoff"));
        assert!(done.lease_until.is_none());
        assert!(is_wrong_state(&l.heartbeat(&t.id, "s1", mins(30)).await));
        assert!(is_wrong_state(&l.complete(&t.id, "s1", "again").await));
        assert_eq!(l.list("dom-a", Status::Completed).await.unwrap().len(), 1);
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
        assert_eq!(c.task.attempts, 2);
        stale_holder_is_refused(&mut l, &t, &now).await;
    }

    async fn stale_holder_is_refused(l: &mut Ledger, t: &Task, now: &Mutex<OffsetDateTime>) {
        assert!(
            is_not_holder(&l.heartbeat(&t.id, "s1", mins(30)).await),
            "stale holder"
        );
        assert!(is_not_holder(&l.complete(&t.id, "s1", "mem-a-h").await));
        assert!(is_not_holder(&l.release(&t.id, "s1", None).await));
        // s2's own lease also expires if never renewed
        *now.lock().unwrap() = T0 + mins(62);
        assert!(is_not_holder(&l.heartbeat(&t.id, "s2", mins(30)).await));
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
            .claim_next("dom-a", "s1", mins(5), &["tool:github".into()])
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
        l.complete(&parent.id, "s1", "mem-a-h1").await.unwrap();
        let c = next(&mut l, "s1", mins(5)).await.unwrap();
        assert_eq!(c.task.id, child.id);
    }

    async fn release_returns_to_pool_and_invalidate_is_terminal(b: &Site) {
        let (mut l, _) = b.ledger().await;
        let t = admitted(&mut l, spec("Give back")).await;
        claim(&mut l, &t.id, "s1", mins(5)).await.unwrap();
        let t = l.release(&t.id, "s1", Some("mem-x-left")).await.unwrap();
        assert_eq!(
            t.handoff.as_deref(),
            Some("mem-x-left"),
            "the handoff rides with the released task"
        );
        assert_eq!(t.status, Status::Admitted);
        assert!(t.holder.is_none());
        let c = next(&mut l, "s2", mins(5)).await.unwrap();
        assert_eq!(c.task.attempts, 2);
        invalidate_is_terminal(&mut l, &t).await;
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
            is_wrong_state(&l.heartbeat(&t.id, "s2", mins(5)).await),
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
        assert_eq!(after[0].attempts, 2);
        assert_eq!(after[0].status, Status::Completed);
        assert_eq!(after[1].status, Status::Invalidated);
    }

    /// One task through every operation and a second one invalidated: nine events.
    async fn nine_events(l: &mut Ledger, now: &Mutex<OffsetDateTime>) -> (Task, Task) {
        let a = l.create(spec("A")).await.unwrap().task;
        l.admit(&a.id).await.unwrap();
        claim(l, &a.id, "s1", mins(5)).await.unwrap();
        *now.lock().unwrap() = T0 + mins(2);
        l.heartbeat(&a.id, "s1", mins(5)).await.unwrap();
        l.release(&a.id, "s1", None).await.unwrap();
        claim(l, &a.id, "s2", mins(5)).await.unwrap();
        l.complete(&a.id, "s2", "mem-a-h").await.unwrap();
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
        assert_eq!(l.get(&id).await.unwrap().status, Status::Admitted);
        assert_eq!(l.list("dom-a", Status::Admitted).await.unwrap().len(), 1);
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
        l.complete(&from_upstream.id, "s1", "mem-a-h1")
            .await
            .unwrap();
        l.complete(&local.id, "s1", "mem-a-h2").await.unwrap();
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
            l1.claim_next("dom-a", "s1", mins(5), &[]),
            l2.claim_next("dom-a", "s2", mins(5), &[]),
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
}
