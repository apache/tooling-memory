//! The PostgreSQL backend: the same two tables and the same events, on a server several
//! processes can share. One connection, driven by a task of its own.
//!
//! Times are stored as the RFC 3339 text the SQLite backend stores, so a row reads back
//! exactly as it was written and a replay produces the same projection on either side.
//! A claim takes its candidates `FOR UPDATE SKIP LOCKED`, so two claimers on two
//! connections cannot both win the same task.

use crate::{claimable, parse_time, Error, Event, RawTask, Status, Task, TaskSpec, TASK_COLUMNS};
use time::OffsetDateTime;
use tokio_postgres::types::ToSql;
use tokio_postgres::{Client, GenericClient, NoTls, Row};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS events (
    seq      BIGSERIAL PRIMARY KEY,
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
    label_rank     BIGINT NOT NULL,
    priority       BIGINT NOT NULL,
    created        TEXT NOT NULL,
    status         TEXT NOT NULL,
    holder         TEXT,
    lease_until    TEXT,
    attempts       BIGINT NOT NULL DEFAULT 0,
    handoff        TEXT,
    reason         TEXT,
    superseded_by  TEXT,
    posted_to      TEXT
);
CREATE INDEX IF NOT EXISTS tasks_queue ON tasks (domain, status, priority DESC, created);
CREATE UNIQUE INDEX IF NOT EXISTS tasks_domain_origin ON tasks (domain, origin, upstream_event)
    WHERE origin IS NOT NULL AND upstream_event IS NOT NULL;
";

fn task_from_row(row: &Row) -> Result<Task, Error> {
    RawTask {
        id: row.try_get(0)?,
        domain: row.try_get(1)?,
        title: row.try_get(2)?,
        body: row.try_get(3)?,
        requirements: row.try_get(4)?,
        depends_on: row.try_get(5)?,
        origin: row.try_get(6)?,
        upstream_event: row.try_get(7)?,
        created_by: row.try_get(8)?,
        label_rank: row.try_get(9)?,
        priority: row.try_get(10)?,
        created: row.try_get(11)?,
        status: row.try_get(12)?,
        holder: row.try_get(13)?,
        lease_until: row.try_get(14)?,
        attempts: row.try_get(15)?,
        handoff: row.try_get(16)?,
        reason: row.try_get(17)?,
        superseded_by: row.try_get(18)?,
        posted_to: row.try_get(19)?,
    }
    .into_task()
}

/// The ledger's tables on one PostgreSQL connection.
pub(crate) struct Db {
    client: Client,
}

impl Db {
    /// Connect, spawn the task that drives the connection, and create the schema.
    pub(crate) async fn connect(url: &str) -> Result<Db, Error> {
        let (client, connection) = tokio_postgres::connect(url, NoTls).await?;
        // The connection ends when the client is dropped; an error on the way surfaces
        // on the next query as its own error, so nothing is lost by not logging it here.
        tokio::spawn(async move {
            let _ = connection.await;
        });
        client.batch_execute(SCHEMA).await?;
        Ok(Db { client })
    }

    pub(crate) async fn get(&self, id: &str) -> Result<Task, Error> {
        get_in(&self.client, id).await
    }

    pub(crate) async fn find_origin(
        &self,
        domain: &str,
        origin: &str,
        upstream_event: &str,
    ) -> Result<Option<String>, Error> {
        let row = self
            .client
            .query_opt(
                "SELECT id FROM tasks WHERE domain = $1 AND origin = $2 AND upstream_event = $3",
                &[&domain, &origin, &upstream_event],
            )
            .await?;
        Ok(row.map(|r| r.try_get(0)).transpose()?)
    }

    /// Append the event and project it, in one transaction.
    pub(crate) async fn append(
        &mut self,
        id: &str,
        at: OffsetDateTime,
        event: &Event,
    ) -> Result<(), Error> {
        let tx = self.client.transaction().await?;
        apply(&tx, id, at, event).await?;
        tx.commit().await?;
        Ok(())
    }

    /// The id of the task claimed, if any candidate qualified. The candidates are read
    /// in queue order; the first the manifest can take is then locked on its own, `FOR
    /// UPDATE SKIP LOCKED`, and read again as it now stands, so a row another claimer
    /// holds or has just taken is passed over rather than claimed twice.
    pub(crate) async fn claim_next(
        &mut self,
        domain: &str,
        now: OffsetDateTime,
        holder: &str,
        until: OffsetDateTime,
        manifest: &[String],
    ) -> Result<Option<String>, Error> {
        let claimable_now = "(status = 'admitted' OR (status = 'claimed' AND lease_until < $2))";
        let now_text = crate::fmt_time(now);
        let tx = self.client.transaction().await?;
        let rows = tx
            .query(
                &format!(
                    "SELECT {TASK_COLUMNS} FROM tasks WHERE domain = $1 AND {claimable_now}
                     ORDER BY priority DESC, created"
                ),
                &[&domain, &now_text],
            )
            .await?;
        for row in &rows {
            let seen = task_from_row(row)?;
            if !claimable(&seen, now, manifest) {
                continue;
            }
            let Some(locked) = tx
                .query_opt(
                    &format!(
                        "SELECT {TASK_COLUMNS} FROM tasks WHERE id = $1 AND {claimable_now}
                         FOR UPDATE SKIP LOCKED"
                    ),
                    &[&seen.id, &now_text],
                )
                .await?
            else {
                continue;
            };
            let task = task_from_row(&locked)?;
            if !claimable(&task, now, manifest)
                || !deps_complete(&tx, &task.spec.depends_on).await?
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
            )
            .await?;
            tx.commit().await?;
            return Ok(Some(task.id));
        }
        Ok(None)
    }

    pub(crate) async fn claim(
        &mut self,
        id: &str,
        now: OffsetDateTime,
        holder: &str,
        until: OffsetDateTime,
        manifest: &[String],
    ) -> Result<(), Error> {
        let tx = self.client.transaction().await?;
        let row = tx
            .query_opt(
                &format!("SELECT {TASK_COLUMNS} FROM tasks WHERE id = $1 FOR UPDATE"),
                &[&id],
            )
            .await?
            .ok_or_else(|| Error::NotFound(id.to_string()))?;
        let task = task_from_row(&row)?;
        if !claimable(&task, now, manifest) || !deps_complete(&tx, &task.spec.depends_on).await? {
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
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn select(&self, sql: &str, args: &[&(dyn ToSql + Sync)]) -> Result<Vec<Task>, Error> {
        let rows = self
            .client
            .query(&format!("SELECT {TASK_COLUMNS} FROM tasks {sql}"), args)
            .await?;
        rows.iter().map(task_from_row).collect()
    }

    pub(crate) async fn filed_from(&self, parent: &str) -> Result<Vec<Task>, Error> {
        self.select(
            "WHERE origin IN ($1, $2, $3) ORDER BY created",
            &[
                &format!("task:{parent}"),
                &format!("continues:{parent}"),
                &format!("retries:{parent}"),
            ],
        )
        .await
    }

    pub(crate) async fn list(&self, domain: &str, status: Status) -> Result<Vec<Task>, Error> {
        self.select(
            "WHERE domain = $1 AND status = $2 ORDER BY priority DESC, created",
            &[&domain, &status.as_str()],
        )
        .await
    }

    pub(crate) async fn unposted(&self, domain: &str) -> Result<Vec<Task>, Error> {
        self.select(
            "WHERE domain = $1 AND status = 'completed' AND origin IS NOT NULL
               AND posted_to IS NULL
             ORDER BY created",
            &[&domain],
        )
        .await
    }

    pub(crate) async fn dependents(&self, id: &str) -> Result<Vec<Task>, Error> {
        self.select(
            "WHERE id IN (
                 SELECT t.id FROM tasks t, jsonb_array_elements_text(t.depends_on::jsonb) AS d(value)
                 WHERE d.value = $1)
             ORDER BY created",
            &[&id],
        )
        .await
    }

    pub(crate) async fn event_count(&self) -> Result<u64, Error> {
        let row = self
            .client
            .query_one("SELECT COUNT(*) FROM events", &[])
            .await?;
        let n: i64 = row.try_get(0)?;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    pub(crate) async fn rebuild(&mut self) -> Result<u64, Error> {
        let tx = self.client.transaction().await?;
        tx.batch_execute("DELETE FROM tasks").await?;
        let rows = tx
            .query("SELECT task_id, at, payload FROM events ORDER BY seq", &[])
            .await?;
        let mut n = 0;
        for row in &rows {
            let task_id: String = row.try_get(0)?;
            let at: String = row.try_get(1)?;
            let payload: String = row.try_get(2)?;
            let at = parse_time(&task_id, &at)?;
            let event = Event::parse(&task_id, &payload)?;
            project(&tx, &task_id, at, &event).await?;
            n += 1;
        }
        tx.commit().await?;
        Ok(n)
    }
}

async fn get_in<C: GenericClient>(conn: &C, id: &str) -> Result<Task, Error> {
    let row = conn
        .query_opt(
            &format!("SELECT {TASK_COLUMNS} FROM tasks WHERE id = $1"),
            &[&id],
        )
        .await?
        .ok_or_else(|| Error::NotFound(id.to_string()))?;
    task_from_row(&row)
}

async fn deps_complete<C: GenericClient>(conn: &C, deps: &[String]) -> Result<bool, Error> {
    for d in deps {
        let row = conn
            .query_opt("SELECT status FROM tasks WHERE id = $1", &[&d])
            .await?;
        let status: Option<String> = row.map(|r| r.try_get(0)).transpose()?;
        if status.as_deref() != Some("completed") {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Append the event and project it, in the caller's transaction.
async fn apply<C: GenericClient>(
    conn: &C,
    id: &str,
    at: OffsetDateTime,
    event: &Event,
) -> Result<(), Error> {
    conn.execute(
        "INSERT INTO events (task_id, kind, at, payload) VALUES ($1, $2, $3, $4)",
        &[
            &id,
            &event.kind(),
            &crate::fmt_time(at),
            &event.payload(id)?,
        ],
    )
    .await?;
    project(conn, id, at, event).await
}

/// The row a `Created` event projects to: a task born proposed.
async fn insert_task<C: GenericClient>(
    conn: &C,
    id: &str,
    at: OffsetDateTime,
    spec: &TaskSpec,
) -> Result<(), Error> {
    let requirements = serde_json::to_string(&spec.requirements).unwrap_or_default();
    let depends_on = serde_json::to_string(&spec.depends_on).unwrap_or_default();
    conn.execute(
        "INSERT INTO tasks (id, domain, title, body, requirements, depends_on, origin,
            upstream_event, created_by, label_rank, priority, created, status)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, 'proposed')",
        &[
            &id,
            &spec.domain,
            &spec.title,
            &spec.body,
            &requirements,
            &depends_on,
            &spec.origin,
            &spec.upstream_event,
            &spec.created_by,
            &(spec.classification as i64),
            &spec.priority,
            &crate::fmt_time(at),
        ],
    )
    .await?;
    Ok(())
}

/// Update the projection for one event. Replay calls this and nothing else.
async fn project<C: GenericClient>(
    conn: &C,
    id: &str,
    at: OffsetDateTime,
    event: &Event,
) -> Result<(), Error> {
    // the one time an event carries, formatted once so the arms can borrow it
    let until_text = match event {
        Event::Claimed { until, .. } | Event::Heartbeat { until } => crate::fmt_time(*until),
        _ => String::new(),
    };
    let (sql, args): (&str, Vec<&(dyn ToSql + Sync)>) = match event {
        Event::Created { id: new_id, spec } => return insert_task(conn, new_id, at, spec).await,
        Event::Admitted => (
            "UPDATE tasks SET status = 'admitted' WHERE id = $1",
            vec![&id],
        ),
        Event::Claimed { holder, .. } => (
            "UPDATE tasks SET status = 'claimed', holder = $2, lease_until = $3,
                attempts = attempts + 1 WHERE id = $1",
            vec![&id, holder, &until_text],
        ),
        Event::Heartbeat { .. } => (
            "UPDATE tasks SET lease_until = $2 WHERE id = $1",
            vec![&id, &until_text],
        ),
        Event::Released => (
            "UPDATE tasks SET status = 'admitted', holder = NULL, lease_until = NULL
             WHERE id = $1",
            vec![&id],
        ),
        Event::HandoffLeft { handoff } => (
            "UPDATE tasks SET handoff = $2 WHERE id = $1",
            vec![&id, handoff],
        ),
        Event::Completed { handoff } => (
            "UPDATE tasks SET status = 'completed', handoff = $2, lease_until = NULL
             WHERE id = $1",
            vec![&id, handoff],
        ),
        Event::Invalidated {
            reason,
            superseded_by,
        } => (
            "UPDATE tasks SET status = 'invalidated', reason = $2, superseded_by = $3,
                lease_until = NULL WHERE id = $1",
            vec![&id, reason, superseded_by],
        ),
        Event::Posted { to } => (
            "UPDATE tasks SET posted_to = $2 WHERE id = $1",
            vec![&id, to],
        ),
    };
    conn.execute(sql, &args).await?;
    Ok(())
}
