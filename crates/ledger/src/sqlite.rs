//! The SQLite backend: one connection, one file, the laptop's ledger.
//!
//! The connection sits behind a mutex so the ledger is `Sync`; the server holds the whole
//! ledger under an async lock, so the mutex is never contended.

use crate::{
    claimable, parse_time, Error, Event, PromotionRecord, PromotionSpec, RawPromotion, RawTask,
    Redaction, Status, Task, TaskSpec, PROMOTION_COLUMNS, TASK_COLUMNS,
};
use dm_core::Label;
use rusqlite::{params, Connection, OptionalExtension, Row};
use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};
use time::OffsetDateTime;

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
CREATE TABLE IF NOT EXISTS promotions (
    seq        INTEGER PRIMARY KEY AUTOINCREMENT,
    domain     TEXT NOT NULL,
    entry      TEXT NOT NULL,
    tier       TEXT NOT NULL,
    promoter   TEXT NOT NULL,
    author     TEXT NOT NULL,
    why        TEXT NOT NULL,
    label_rank INTEGER NOT NULL,
    digest     TEXT NOT NULL,
    new_digest TEXT NOT NULL,
    at         TEXT NOT NULL,
    why_erased INTEGER NOT NULL DEFAULT 0
);
CREATE UNIQUE INDEX IF NOT EXISTS promotions_version
    ON promotions (domain, entry, tier, digest, promoter);
";

fn task_from_row(row: &Row<'_>) -> Result<Task, Error> {
    RawTask {
        id: row.get(0)?,
        domain: row.get(1)?,
        title: row.get(2)?,
        body: row.get(3)?,
        requirements: row.get(4)?,
        depends_on: row.get(5)?,
        origin: row.get(6)?,
        upstream_event: row.get(7)?,
        created_by: row.get(8)?,
        label_rank: row.get(9)?,
        priority: row.get(10)?,
        created: row.get(11)?,
        status: row.get(12)?,
        holder: row.get(13)?,
        lease_until: row.get(14)?,
        attempts: row.get(15)?,
        handoff: row.get(16)?,
        reason: row.get(17)?,
        superseded_by: row.get(18)?,
        posted_to: row.get(19)?,
    }
    .into_task()
}

/// The ledger's tables in one SQLite file.
pub(crate) struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    pub(crate) fn open(path: &Path) -> Result<Db, Error> {
        Db::init(Connection::open(path)?)
    }

    pub(crate) fn in_memory() -> Result<Db, Error> {
        Db::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Db, Error> {
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
        migrate_promotions(&conn)?;
        Ok(Db {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn get(&self, id: &str) -> Result<Task, Error> {
        get_in(&self.conn(), id)
    }

    pub(crate) fn find_origin(
        &self,
        domain: &str,
        origin: &str,
        upstream_event: &str,
    ) -> Result<Option<String>, Error> {
        Ok(self
            .conn()
            .query_row(
                "SELECT id FROM tasks WHERE domain = ?1 AND origin = ?2 AND upstream_event = ?3",
                params![domain, origin, upstream_event],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Append the event and project it, in one transaction.
    pub(crate) fn append(
        &mut self,
        id: &str,
        at: OffsetDateTime,
        event: &Event,
    ) -> Result<(), Error> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        apply(&tx, id, at, event)?;
        tx.commit()?;
        Ok(())
    }

    /// Record a promotion unless that promoter promoted that version of the entry to that
    /// tier already. The check and the append are one transaction, taken for writing first.
    pub(crate) fn record_promotion(
        &mut self,
        at: OffsetDateTime,
        spec: &PromotionSpec,
        why_erased: bool,
    ) -> Result<bool, Error> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let seen = tx
            .query_row(
                "SELECT 1 FROM promotions
                 WHERE domain = ?1 AND entry = ?2 AND tier = ?3 AND digest = ?4
                   AND promoter = ?5",
                params![
                    spec.domain,
                    spec.entry,
                    spec.tier.to_string(),
                    spec.digest,
                    spec.by
                ],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if seen {
            return Ok(false);
        }
        let key = crate::promotion_key(&spec.entry);
        apply(
            &tx,
            &key,
            at,
            &Event::Promoted {
                spec: spec.clone(),
                why_erased,
            },
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Remove the why from an entry's promotion events and rows in one transaction, with
    /// the space they leave overwritten, then rebuild the file without that space and
    /// fold the log into it, so the old text is in neither. Returns how many records of
    /// the entry hold no why now.
    pub(crate) fn scrub_promotions(&mut self, domain: &str, entry: &str) -> Result<usize, Error> {
        let (n, changed) = self.scrubbing(|tx| scrub_promotions_in(tx, domain, entry))?;
        self.compact_if(changed)?;
        Ok(n)
    }

    /// Rewrite the tasks filed from `origin` to the redaction, in their events and their
    /// rows, and invalidate the open ones, in one transaction with the space the old text
    /// leaves overwritten; then rebuild the file and fold the log into it. Returns how
    /// many tasks from that origin there are.
    pub(crate) fn scrub_tasks(
        &mut self,
        at: OffsetDateTime,
        domain: &str,
        origin: &str,
        redaction: &Redaction,
    ) -> Result<usize, Error> {
        let (n, changed) =
            self.scrubbing(|tx| scrub_tasks_in(tx, at, domain, origin, redaction))?;
        self.compact_if(changed)?;
        Ok(n)
    }

    /// Both scrubs of an erasure in one transaction, and one rebuild of the file after
    /// them when either rewrote anything. Returns the records that hold no why now and
    /// the tasks from the origin.
    pub(crate) fn scrub_entry(
        &mut self,
        at: OffsetDateTime,
        domain: &str,
        entry: &str,
        origin: &str,
        redaction: &Redaction,
    ) -> Result<(usize, usize), Error> {
        let ((records, tasks), changed) = self.scrubbing(|tx| {
            let (records, changed_records) = scrub_promotions_in(tx, domain, entry)?;
            let (tasks, changed_tasks) = scrub_tasks_in(tx, at, domain, origin, redaction)?;
            Ok(((records, tasks), changed_records + changed_tasks))
        })?;
        self.compact_if(changed)?;
        Ok((records, tasks))
    }

    /// Run `scrub` in one transaction taken for writing, with what an update frees
    /// overwritten with zeros, here and nowhere else: the rest of the ledger's writes need
    /// not pay for it. Returns what `scrub` returned and how many rows it rewrote.
    fn scrubbing<T>(
        &mut self,
        scrub: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<(T, usize), Error>,
    ) -> Result<(T, usize), Error> {
        let mut conn = self.conn();
        conn.pragma_update(None, "secure_delete", true)?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let out = scrub(&tx)?;
        tx.commit()?;
        conn.pragma_update(None, "secure_delete", false)?;
        Ok(out)
    }

    /// Rebuild the file and fold the log into it when a scrub rewrote anything; a scrub
    /// that found everything rewritten already leaves the file as it is.
    fn compact_if(&self, changed: usize) -> Result<(), Error> {
        if changed == 0 {
            return Ok(());
        }
        compact(&self.conn())
    }

    pub(crate) fn promotions(
        &self,
        domain: &str,
        entry: &str,
    ) -> Result<Vec<PromotionRecord>, Error> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {PROMOTION_COLUMNS} FROM promotions WHERE domain = ?1 AND entry = ?2
             ORDER BY seq"
        ))?;
        let rows = stmt.query_map(params![domain, entry], |r| {
            Ok(RawPromotion {
                domain: r.get(0)?,
                entry: r.get(1)?,
                tier: r.get(2)?,
                promoter: r.get(3)?,
                author: r.get(4)?,
                why: r.get(5)?,
                label_rank: r.get(6)?,
                digest: r.get(7)?,
                new_digest: r.get(8)?,
                at: r.get(9)?,
                why_erased: r.get(10)?,
            })
        })?;
        rows.map(|r| r?.into_record()).collect()
    }

    /// The id of the task claimed, if any candidate qualified.
    pub(crate) fn claim_next(
        &mut self,
        domain: &str,
        now: OffsetDateTime,
        holder: &str,
        until: OffsetDateTime,
        manifest: &[String],
        handling: Label,
    ) -> Result<Option<String>, Error> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let candidates: Vec<Task> = {
            let mut stmt = tx.prepare(&format!(
                "SELECT {TASK_COLUMNS} FROM tasks
                 WHERE domain = ?1 AND (status = 'admitted' OR (status = 'claimed' AND lease_until < ?2))
                 ORDER BY priority DESC, created"
            ))?;
            let rows = stmt.query_map(params![domain, crate::fmt_time(now)], |r| {
                Ok(task_from_row(r))
            })?;
            rows.map(|r| r?).collect::<Result<_, _>>()?
        };
        for task in candidates {
            if !claimable(&task, now, manifest, handling)
                || !deps_complete(&tx, &task.spec.depends_on)?
                || awaits_continuation(&tx, &task)?
            {
                continue;
            }
            apply(
                &tx,
                &task.id,
                now,
                &Event::Claimed {
                    holder: holder.to_string(),
                    until,
                },
            )?;
            tx.commit()?;
            return Ok(Some(task.id));
        }
        Ok(None)
    }

    pub(crate) fn claim(
        &mut self,
        id: &str,
        now: OffsetDateTime,
        holder: &str,
        until: OffsetDateTime,
        manifest: &[String],
        handling: Label,
    ) -> Result<(), Error> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let task = get_in(&tx, id)?;
        if !claimable(&task, now, manifest, handling)
            || !deps_complete(&tx, &task.spec.depends_on)?
            || awaits_continuation(&tx, &task)?
        {
            return Err(Error::WrongState {
                id: id.to_string(),
                status: task.status,
                op: "claim",
            });
        }
        apply(
            &tx,
            id,
            now,
            &Event::Claimed {
                holder: holder.to_string(),
                until,
            },
        )?;
        tx.commit()?;
        Ok(())
    }

    fn select(&self, sql: &str, args: &[&dyn rusqlite::ToSql]) -> Result<Vec<Task>, Error> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!("SELECT {TASK_COLUMNS} FROM tasks {sql}"))?;
        let rows = stmt.query_map(args, |r| Ok(task_from_row(r)))?;
        rows.map(|r| r?).collect()
    }

    pub(crate) fn filed_from(&self, parent: &str) -> Result<Vec<Task>, Error> {
        self.select(
            "WHERE origin IN (?1, ?2, ?3) ORDER BY created",
            &[
                &format!("task:{parent}"),
                &format!("continues:{parent}"),
                &format!("retries:{parent}"),
            ],
        )
    }

    pub(crate) fn with_origin(&self, domain: &str, origin: &str) -> Result<Vec<Task>, Error> {
        self.select(
            "WHERE domain = ?1 AND origin = ?2 ORDER BY created",
            &[&domain, &origin],
        )
    }

    pub(crate) fn list(&self, domain: &str, status: Status) -> Result<Vec<Task>, Error> {
        self.select(
            "WHERE domain = ?1 AND status = ?2 ORDER BY priority DESC, created",
            &[&domain, &status.as_str()],
        )
    }

    pub(crate) fn unposted(&self, domain: &str) -> Result<Vec<Task>, Error> {
        self.select(
            "WHERE domain = ?1 AND status = 'completed' AND origin IS NOT NULL
               AND posted_to IS NULL
             ORDER BY created",
            &[&domain],
        )
    }

    pub(crate) fn dependents(&self, id: &str) -> Result<Vec<Task>, Error> {
        self.select(
            "WHERE id IN (
                 SELECT t.id FROM tasks t, json_each(t.depends_on) AS d WHERE d.value = ?1)
             ORDER BY created",
            &[&id],
        )
    }

    pub(crate) fn created_payload(&self, id: &str) -> Result<String, Error> {
        self.conn()
            .query_row(
                "SELECT payload FROM events WHERE task_id = ?1 AND kind = 'created' ORDER BY seq LIMIT 1",
                [id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(id.to_string()))
    }

    pub(crate) fn event_count(&self) -> Result<u64, Error> {
        let n: i64 = self
            .conn()
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))?;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    pub(crate) fn rebuild(&mut self) -> Result<u64, Error> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute_batch("DELETE FROM tasks; DELETE FROM promotions;")?;
        let events: Vec<(String, String, String)> = {
            let mut stmt = tx.prepare("SELECT task_id, at, payload FROM events ORDER BY seq")?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            rows.collect::<Result<_, _>>()?
        };
        let mut n = 0;
        for (task_id, at, payload) in events {
            let at = parse_time(&task_id, &at)?;
            let event = Event::parse(&task_id, &payload)?;
            project(&tx, &task_id, at, &event)?;
            n += 1;
        }
        tx.commit()?;
        Ok(n)
    }
}

/// A ledger whose promotions table 41e0bc5 made has neither `new_digest` nor
/// `why_erased`, and its index lacks the promoter, so a second promoter of a version would
/// be refused; one 088f890 made has `new_digest` and the promoter, and no `why_erased`.
/// Each column that is missing is added, empty or false, and the index is made again
/// when it lacks the promoter.
fn migrate_promotions(conn: &Connection) -> Result<(), Error> {
    let has = |column: &str| -> Result<bool, Error> {
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('promotions') WHERE name = ?1",
            [column],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    };
    // each column on its own: a ledger from between the two shapes has one and not the
    // other
    if !has("new_digest")? {
        conn.execute_batch(
            "ALTER TABLE promotions ADD COLUMN new_digest TEXT NOT NULL DEFAULT '';",
        )?;
    }
    if !has("why_erased")? {
        conn.execute_batch(
            "ALTER TABLE promotions ADD COLUMN why_erased INTEGER NOT NULL DEFAULT 0;",
        )?;
    }
    let keyed_by_promoter: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_index_info('promotions_version') WHERE name = 'promoter'",
        [],
        |r| r.get(0),
    )?;
    if keyed_by_promoter == 0 {
        conn.execute_batch(
            "BEGIN;
             DROP INDEX IF EXISTS promotions_version;
             CREATE UNIQUE INDEX promotions_version
                 ON promotions (domain, entry, tier, digest, promoter);
             COMMIT;",
        )?;
    }
    Ok(())
}

/// The scrub of an entry's promotions, in the caller's transaction: the why leaves its
/// events and its rows. Returns how many records of the entry hold no why now, and how
/// many rows this call rewrote.
fn scrub_promotions_in(
    tx: &Connection,
    domain: &str,
    entry: &str,
) -> Result<(usize, usize), Error> {
    let key = crate::promotion_key(entry);
    let events: Vec<(i64, String)> = {
        let mut stmt =
            tx.prepare("SELECT seq, payload FROM events WHERE task_id = ?1 AND kind = 'promoted'")?;
        let rows = stmt.query_map(params![key], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<Result<_, _>>()?
    };
    let mut changed = 0;
    for (seq, payload) in events {
        if let Some(scrubbed) = crate::scrub_payload(&key, domain, &payload)? {
            changed += tx.execute(
                "UPDATE events SET payload = ?2 WHERE seq = ?1",
                params![seq, scrubbed],
            )?;
        }
    }
    changed += tx.execute(
        "UPDATE promotions SET why = '', why_erased = 1
         WHERE domain = ?1 AND entry = ?2 AND (why_erased = 0 OR why != '')",
        params![domain, entry],
    )?;
    let n: i64 = tx.query_row(
        "SELECT COUNT(*) FROM promotions WHERE domain = ?1 AND entry = ?2 AND why_erased = 1",
        params![domain, entry],
        |r| r.get(0),
    )?;
    Ok((usize::try_from(n).unwrap_or(0), changed))
}

/// The scrub of the tasks filed from `origin`, in the caller's transaction: each is
/// rewritten to the redaction and the open ones are invalidated. Returns how many tasks
/// there are from that origin, and how many rows this call rewrote.
fn scrub_tasks_in(
    tx: &Connection,
    at: OffsetDateTime,
    domain: &str,
    origin: &str,
    redaction: &Redaction,
) -> Result<(usize, usize), Error> {
    let tasks: Vec<(String, bool)> = {
        let mut stmt = tx.prepare(
            "SELECT id, status IN ('completed', 'invalidated') FROM tasks
             WHERE domain = ?1 AND origin = ?2 ORDER BY created",
        )?;
        let rows = stmt.query_map(params![domain, origin], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<Result<_, _>>()?
    };
    let mut changed = 0;
    for (id, terminal) in &tasks {
        if !terminal {
            let closed = Event::Invalidated {
                reason: redaction.reason.clone(),
                superseded_by: None,
            };
            apply(tx, id, at, &closed)?;
            changed += 1;
        }
        changed += redact_task(tx, id, redaction)?;
    }
    Ok((tasks.len(), changed))
}

/// One task's events and row, rewritten to the redaction: the title and body it was
/// filed with, the reason it was invalidated for, and the handoffs left on it or that
/// closed it, each free text an approver may have put the entry's text in. Returns how
/// many rows changed.
fn redact_task(conn: &Connection, id: &str, redaction: &Redaction) -> Result<usize, Error> {
    let events: Vec<(i64, String)> = {
        let mut stmt = conn.prepare("SELECT seq, payload FROM events WHERE task_id = ?1")?;
        let rows = stmt.query_map(params![id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<Result<_, _>>()?
    };
    let mut changed = 0;
    for (seq, payload) in events {
        if let Some(redacted) = crate::redact_event(id, &payload, redaction)? {
            changed += conn.execute(
                "UPDATE events SET payload = ?2 WHERE seq = ?1",
                params![seq, redacted],
            )?;
        }
    }
    changed += conn.execute(
        "UPDATE tasks SET title = ?2, body = ?3,
            reason = CASE WHEN reason IS NULL THEN NULL ELSE ?4 END,
            handoff = CASE WHEN handoff IS NULL THEN NULL ELSE ?4 END
         WHERE id = ?1 AND (title != ?2 OR body != ?3
            OR reason IS NOT ?4 AND reason IS NOT NULL
            OR handoff IS NOT ?4 AND handoff IS NOT NULL)",
        params![id, redaction.title, redaction.body, redaction.reason],
    )?;
    Ok(changed)
}

/// Rebuild the file without the free space old text sat in, and fold the log into it:
/// the log first, so the rebuild reads the tables as they stand; then again, since the
/// rebuild itself went through the log. `secure_delete` alone is not enough: a why long
/// enough for overflow pages, with records written after it, left its text in the file.
fn compact(conn: &Connection) -> Result<(), Error> {
    checkpoint(conn)?;
    conn.execute_batch("VACUUM;")?;
    checkpoint(conn)
}

/// Fold the write-ahead log into the file and truncate it, waiting a little for a reader
/// mid-query; [`Error::Held`] when it still could not, since the log then still holds
/// what the tables no longer do.
fn checkpoint(conn: &Connection) -> Result<(), Error> {
    conn.busy_timeout(std::time::Duration::from_millis(200))?;
    let mut held = true;
    for _ in 0..5 {
        let busy: i64 = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))?;
        if busy == 0 {
            held = false;
            break;
        }
    }
    // rusqlite's own default, for every other statement on this connection
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    if held {
        return Err(Error::Held("the write-ahead log".into()));
    }
    Ok(())
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

/// Whether a continuation of `task` is still open: its work goes on there, and the
/// continuation's completion closes it, so it is nobody's to claim meanwhile.
fn awaits_continuation(conn: &Connection, task: &Task) -> Result<bool, Error> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM tasks WHERE domain = ?1 AND origin = ?2
             AND status NOT IN ('completed', 'invalidated') LIMIT 1",
            params![task.spec.domain, crate::continues(&task.id)],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Append the event and project it, in the caller's transaction.
fn apply(conn: &Connection, id: &str, at: OffsetDateTime, event: &Event) -> Result<(), Error> {
    conn.execute(
        "INSERT INTO events (task_id, kind, at, payload) VALUES (?1, ?2, ?3, ?4)",
        params![id, event.kind(), crate::fmt_time(at), event.payload(id)?],
    )?;
    project(conn, id, at, event)
}

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
            crate::fmt_time(at),
        ],
    )?;
    Ok(())
}

/// The row a `Promoted` event projects to.
fn insert_promotion(
    conn: &Connection,
    at: OffsetDateTime,
    p: &PromotionSpec,
    why_erased: bool,
) -> Result<(), Error> {
    conn.execute(
        "INSERT INTO promotions (domain, entry, tier, promoter, author, why, label_rank,
            digest, new_digest, at, why_erased)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            p.domain,
            p.entry,
            p.tier.to_string(),
            p.by,
            p.author,
            p.why,
            p.classification as i64,
            p.digest,
            p.new_digest,
            crate::fmt_time(at),
            i64::from(why_erased),
        ],
    )?;
    Ok(())
}

/// Update the projection for one event. Replay calls this and nothing else.
fn project(conn: &Connection, id: &str, at: OffsetDateTime, event: &Event) -> Result<(), Error> {
    match event {
        Event::Promoted { spec, why_erased } => insert_promotion(conn, at, spec, *why_erased)?,
        Event::Created { id: new_id, spec } => insert_task(conn, new_id, at, spec)?,
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
                params![id, holder, crate::fmt_time(*until)],
            )?;
        }
        Event::Heartbeat { until } => {
            conn.execute(
                "UPDATE tasks SET lease_until = ?2 WHERE id = ?1",
                params![id, crate::fmt_time(*until)],
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
        Event::Rescored { priority, .. } => {
            conn.execute(
                "UPDATE tasks SET priority = ?2 WHERE id = ?1",
                params![id, priority],
            )?;
        }
    }
    Ok(())
}
