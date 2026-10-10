//! The PostgreSQL backend: the same two tables and the same events, on a server several
//! processes can share. One connection, driven by a task of its own.
//!
//! One schema per domain, as the reference stack partitions everything by domain: a
//! domain's `events` and `tasks` live in a schema of their own, created when the domain
//! files its first task, so a domain's queue is a table no other domain's claims touch
//! and a domain can be moved, backed up or dropped as a unit. Two tables in `public`
//! route the rest: `ledger_domains`, each domain and its schema, and `ledger_tasks`,
//! each task id and its domain, written in the transaction that files the task, so an
//! operation by id finds its schema in one lookup.
//!
//! Times are stored as the RFC 3339 text the SQLite backend stores, so a row reads back
//! exactly as it was written and a replay produces the same projection on either side.
//! A claim takes its candidates `FOR UPDATE SKIP LOCKED`, so two claimers on two
//! connections cannot both win the same task.

use crate::{claimable, parse_time, Error, Event, RawTask, Status, Task, TaskSpec, TASK_COLUMNS};
use std::collections::HashMap;
use std::sync::Mutex;
use time::OffsetDateTime;
use tokio_postgres::types::ToSql;
use tokio_postgres::{Client, GenericClient, NoTls, Row};

/// The routing tables, in `public`.
const DIRECTORY: &str = "
CREATE TABLE IF NOT EXISTS public.ledger_domains (
    domain TEXT PRIMARY KEY,
    schema TEXT NOT NULL UNIQUE
);
CREATE TABLE IF NOT EXISTS public.ledger_tasks (
    id     TEXT PRIMARY KEY,
    domain TEXT NOT NULL REFERENCES public.ledger_domains (domain)
);
";

/// One domain's tables, created in its schema. `{s}` is the quoted schema name.
const DOMAIN_TABLES: &str = "
CREATE TABLE IF NOT EXISTS {s}.events (
    seq      BIGSERIAL PRIMARY KEY,
    task_id  TEXT NOT NULL,
    kind     TEXT NOT NULL,
    at       TEXT NOT NULL,
    payload  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS events_task ON {s}.events (task_id, seq);
CREATE TABLE IF NOT EXISTS {s}.tasks (
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
CREATE INDEX IF NOT EXISTS tasks_queue ON {s}.tasks (status, priority DESC, created);
CREATE UNIQUE INDEX IF NOT EXISTS tasks_origin ON {s}.tasks (origin, upstream_event)
    WHERE origin IS NOT NULL AND upstream_event IS NOT NULL;
";

/// The schema a domain's tables live in: `ledger_` and the domain with anything that is
/// not a lowercase letter or digit made an underscore, then eight hex digits of the
/// domain's digest, so two domains that read alike never share a schema. Quoted.
pub(crate) fn schema_for(domain: &str) -> String {
    let readable: String = domain
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .take(40)
        .collect();
    format!("\"ledger_{readable}_{}\"", &dm_core::digest(domain)[..8])
}

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
    /// Domains whose schema is known to exist, and the schema.
    known: Mutex<HashMap<String, String>>,
}

impl Db {
    /// Connect, spawn the task that drives the connection, create the routing tables,
    /// and move a ledger from before domains had schemas into them.
    pub(crate) async fn connect(url: &str) -> Result<Db, Error> {
        let (client, connection) = tokio_postgres::connect(url, NoTls).await?;
        // The connection ends when the client is dropped; an error on the way surfaces
        // on the next query as its own error, so nothing is lost by not logging it here.
        tokio::spawn(async move {
            let _ = connection.await;
        });
        client.batch_execute(DIRECTORY).await?;
        let mut db = Db {
            client,
            known: Mutex::new(HashMap::new()),
        };
        db.migrate_shared_tables().await?;
        Ok(db)
    }

    fn cached(&self, domain: &str) -> Option<String> {
        self.known
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(domain)
            .cloned()
    }

    fn remember(&self, domain: &str, schema: &str) {
        self.known
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(domain.to_string(), schema.to_string());
    }

    /// The schema of a domain that has filed a task, or `None`.
    async fn schema(&self, domain: &str) -> Result<Option<String>, Error> {
        if let Some(s) = self.cached(domain) {
            return Ok(Some(s));
        }
        let row = self
            .client
            .query_opt(
                "SELECT schema FROM public.ledger_domains WHERE domain = $1",
                &[&domain],
            )
            .await?;
        let schema: Option<String> = row.map(|r| r.try_get(0)).transpose()?;
        if let Some(s) = &schema {
            self.remember(domain, s);
        }
        Ok(schema)
    }

    /// The schema of the domain a task belongs to.
    async fn schema_of_task(&self, id: &str) -> Result<String, Error> {
        schema_of_task_in(&self.client, id)
            .await?
            .ok_or_else(|| Error::NotFound(id.to_string()))
    }

    pub(crate) async fn get(&self, id: &str) -> Result<Task, Error> {
        let s = self.schema_of_task(id).await?;
        get_in(&self.client, &s, id).await
    }

    pub(crate) async fn find_origin(
        &self,
        domain: &str,
        origin: &str,
        upstream_event: &str,
    ) -> Result<Option<String>, Error> {
        let Some(s) = self.schema(domain).await? else {
            return Ok(None);
        };
        let row = self
            .client
            .query_opt(
                &format!("SELECT id FROM {s}.tasks WHERE origin = $1 AND upstream_event = $2"),
                &[&origin, &upstream_event],
            )
            .await?;
        Ok(row.map(|r| r.try_get(0)).transpose()?)
    }

    /// Append the event and project it, in one transaction. A task's first event makes
    /// its domain's schema if this is the domain's first task.
    pub(crate) async fn append(
        &mut self,
        id: &str,
        at: OffsetDateTime,
        event: &Event,
    ) -> Result<(), Error> {
        let tx = self.client.transaction().await?;
        let s = match event {
            Event::Created { spec, .. } => ensure_domain(&tx, &spec.domain).await?,
            _ => schema_of_task_in(&tx, id)
                .await?
                .ok_or_else(|| Error::NotFound(id.to_string()))?,
        };
        apply(&tx, &s, id, at, event).await?;
        tx.commit().await?;
        if let Event::Created { spec, .. } = event {
            self.remember(&spec.domain, &s);
        }
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
        let Some(s) = self.schema(domain).await? else {
            return Ok(None);
        };
        let claimable_now = "(status = 'admitted' OR (status = 'claimed' AND lease_until < $1))";
        let now_text = crate::fmt_time(now);
        let tx = self.client.transaction().await?;
        let rows = tx
            .query(
                &format!(
                    "SELECT {TASK_COLUMNS} FROM {s}.tasks WHERE {claimable_now}
                     ORDER BY priority DESC, created"
                ),
                &[&now_text],
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
                        "SELECT {TASK_COLUMNS} FROM {s}.tasks WHERE id = $2 AND {claimable_now}
                         FOR UPDATE SKIP LOCKED"
                    ),
                    &[&now_text, &seen.id],
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
            let claimed = Event::Claimed {
                holder: holder.to_string(),
                until,
            };
            apply(&tx, &s, &task.id, now, &claimed).await?;
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
        let s = self.schema_of_task(id).await?;
        let tx = self.client.transaction().await?;
        let row = tx
            .query_opt(
                &format!("SELECT {TASK_COLUMNS} FROM {s}.tasks WHERE id = $1 FOR UPDATE"),
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
        let claimed = Event::Claimed {
            holder: holder.to_string(),
            until,
        };
        apply(&tx, &s, id, now, &claimed).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn select(
        &self,
        s: &str,
        sql: &str,
        args: &[&(dyn ToSql + Sync)],
    ) -> Result<Vec<Task>, Error> {
        let rows = self
            .client
            .query(&format!("SELECT {TASK_COLUMNS} FROM {s}.tasks {sql}"), args)
            .await?;
        rows.iter().map(task_from_row).collect()
    }

    /// What was filed from a task is filed in its domain: its children and its
    /// continuation carry its domain, and its retries its root's.
    pub(crate) async fn filed_from(&self, parent: &str) -> Result<Vec<Task>, Error> {
        let Some(s) = schema_of_task_in(&self.client, parent).await? else {
            return Ok(Vec::new());
        };
        self.select(
            &s,
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
        let Some(s) = self.schema(domain).await? else {
            return Ok(Vec::new());
        };
        self.select(
            &s,
            "WHERE status = $1 ORDER BY priority DESC, created",
            &[&status.as_str()],
        )
        .await
    }

    pub(crate) async fn unposted(&self, domain: &str) -> Result<Vec<Task>, Error> {
        let Some(s) = self.schema(domain).await? else {
            return Ok(Vec::new());
        };
        self.select(
            &s,
            "WHERE status = 'completed' AND origin IS NOT NULL AND posted_to IS NULL
             ORDER BY created",
            &[],
        )
        .await
    }

    pub(crate) async fn dependents(&self, id: &str) -> Result<Vec<Task>, Error> {
        let Some(s) = schema_of_task_in(&self.client, id).await? else {
            return Ok(Vec::new());
        };
        self.select(
            &s,
            &format!(
                "WHERE id IN (
                     SELECT t.id FROM {s}.tasks t,
                         jsonb_array_elements_text(t.depends_on::jsonb) AS d(value)
                     WHERE d.value = $1)
                 ORDER BY created"
            ),
            &[&id],
        )
        .await
    }

    /// Every domain's schema.
    async fn schemas(&self) -> Result<Vec<(String, String)>, Error> {
        let rows = self
            .client
            .query(
                "SELECT domain, schema FROM public.ledger_domains ORDER BY domain",
                &[],
            )
            .await?;
        rows.iter()
            .map(|r| Ok((r.try_get(0)?, r.try_get(1)?)))
            .collect()
    }

    pub(crate) async fn event_count(&self) -> Result<u64, Error> {
        let mut n = 0u64;
        for (_, s) in self.schemas().await? {
            let row = self
                .client
                .query_one(&format!("SELECT COUNT(*) FROM {s}.events"), &[])
                .await?;
            let c: i64 = row.try_get(0)?;
            n += u64::try_from(c).unwrap_or(0);
        }
        Ok(n)
    }

    /// Replay every domain's events into a fresh projection, one transaction for all.
    pub(crate) async fn rebuild(&mut self) -> Result<u64, Error> {
        let schemas = self.schemas().await?;
        let tx = self.client.transaction().await?;
        let mut n = 0;
        for (domain, s) in &schemas {
            n += rebuild_domain(&tx, domain, s).await?;
        }
        tx.commit().await?;
        Ok(n)
    }

    /// A ledger from before domains had schemas kept everything in `public.events` and
    /// `public.tasks`. Each domain's events move to its schema in sequence, its
    /// projection is replayed there, and the old tables are renamed, kept rather than
    /// dropped, in one transaction.
    async fn migrate_shared_tables(&mut self) -> Result<(), Error> {
        let row = self
            .client
            .query_one("SELECT to_regclass('public.tasks') IS NOT NULL", &[])
            .await?;
        let shared: bool = row.try_get(0)?;
        if !shared {
            return Ok(());
        }
        let tx = self.client.transaction().await?;
        let domains = tx
            .query(
                "SELECT DISTINCT domain FROM public.tasks ORDER BY domain",
                &[],
            )
            .await?;
        for row in &domains {
            let domain: String = row.try_get(0)?;
            let s = ensure_domain(&tx, &domain).await?;
            tx.execute(
                &format!(
                    "INSERT INTO {s}.events (task_id, kind, at, payload)
                     SELECT e.task_id, e.kind, e.at, e.payload
                     FROM public.events e JOIN public.tasks t ON t.id = e.task_id
                     WHERE t.domain = $1 ORDER BY e.seq"
                ),
                &[&domain],
            )
            .await?;
            rebuild_domain(&tx, &domain, &s).await?;
        }
        tx.batch_execute(
            "ALTER TABLE public.tasks RENAME TO tasks_before_domain_schemas;
             ALTER TABLE public.events RENAME TO events_before_domain_schemas;",
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }
}

/// Make a domain's schema and tables if they are not there, under a lock on the domain
/// so two processes filing a domain's first task do not race to create it.
async fn ensure_domain<C: GenericClient>(conn: &C, domain: &str) -> Result<String, Error> {
    conn.execute(
        "SELECT pg_advisory_xact_lock(hashtext('ledger_domain:' || $1))",
        &[&domain],
    )
    .await?;
    let known = conn
        .query_opt(
            "SELECT schema FROM public.ledger_domains WHERE domain = $1",
            &[&domain],
        )
        .await?;
    if let Some(row) = known {
        return Ok(row.try_get(0)?);
    }
    let s = schema_for(domain);
    conn.batch_execute(&format!(
        "CREATE SCHEMA IF NOT EXISTS {s};{}",
        DOMAIN_TABLES.replace("{s}", &s)
    ))
    .await?;
    conn.execute(
        "INSERT INTO public.ledger_domains (domain, schema) VALUES ($1, $2)",
        &[&domain, &s],
    )
    .await?;
    Ok(s)
}

async fn schema_of_task_in<C: GenericClient>(conn: &C, id: &str) -> Result<Option<String>, Error> {
    let row = conn
        .query_opt(
            "SELECT d.schema FROM public.ledger_tasks t
             JOIN public.ledger_domains d ON d.domain = t.domain WHERE t.id = $1",
            &[&id],
        )
        .await?;
    Ok(row.map(|r| r.try_get(0)).transpose()?)
}

async fn get_in<C: GenericClient>(conn: &C, s: &str, id: &str) -> Result<Task, Error> {
    let row = conn
        .query_opt(
            &format!("SELECT {TASK_COLUMNS} FROM {s}.tasks WHERE id = $1"),
            &[&id],
        )
        .await?
        .ok_or_else(|| Error::NotFound(id.to_string()))?;
    task_from_row(&row)
}

/// Whether every dependency is complete, wherever its domain is.
async fn deps_complete<C: GenericClient>(conn: &C, deps: &[String]) -> Result<bool, Error> {
    for d in deps {
        let Some(s) = schema_of_task_in(conn, d).await? else {
            return Ok(false);
        };
        let row = conn
            .query_opt(
                &format!("SELECT status FROM {s}.tasks WHERE id = $1"),
                &[&d],
            )
            .await?;
        let status: Option<String> = row.map(|r| r.try_get(0)).transpose()?;
        if status.as_deref() != Some("completed") {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Drop one domain's projection and replay its events into it.
async fn rebuild_domain<C: GenericClient>(conn: &C, domain: &str, s: &str) -> Result<u64, Error> {
    conn.execute(&format!("DELETE FROM {s}.tasks"), &[]).await?;
    conn.execute(
        "DELETE FROM public.ledger_tasks WHERE domain = $1",
        &[&domain],
    )
    .await?;
    let rows = conn
        .query(
            &format!("SELECT task_id, at, payload FROM {s}.events ORDER BY seq"),
            &[],
        )
        .await?;
    let mut n = 0;
    for row in &rows {
        let task_id: String = row.try_get(0)?;
        let at: String = row.try_get(1)?;
        let payload: String = row.try_get(2)?;
        let at = parse_time(&task_id, &at)?;
        let event = Event::parse(&task_id, &payload)?;
        project(conn, s, &task_id, at, &event).await?;
        n += 1;
    }
    Ok(n)
}

/// Append the event and project it, in the caller's transaction.
async fn apply<C: GenericClient>(
    conn: &C,
    s: &str,
    id: &str,
    at: OffsetDateTime,
    event: &Event,
) -> Result<(), Error> {
    conn.execute(
        &format!("INSERT INTO {s}.events (task_id, kind, at, payload) VALUES ($1, $2, $3, $4)"),
        &[
            &id,
            &event.kind(),
            &crate::fmt_time(at),
            &event.payload(id)?,
        ],
    )
    .await?;
    project(conn, s, id, at, event).await
}

/// The row a `Created` event projects to: a task born proposed, and its route.
async fn insert_task<C: GenericClient>(
    conn: &C,
    s: &str,
    id: &str,
    at: OffsetDateTime,
    spec: &TaskSpec,
) -> Result<(), Error> {
    let requirements = serde_json::to_string(&spec.requirements).unwrap_or_default();
    let depends_on = serde_json::to_string(&spec.depends_on).unwrap_or_default();
    conn.execute(
        &format!(
            "INSERT INTO {s}.tasks (id, domain, title, body, requirements, depends_on, origin,
                upstream_event, created_by, label_rank, priority, created, status)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, 'proposed')"
        ),
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
    conn.execute(
        "INSERT INTO public.ledger_tasks (id, domain) VALUES ($1, $2)",
        &[&id, &spec.domain],
    )
    .await?;
    Ok(())
}

/// Update the projection for one event. Replay calls this and nothing else.
async fn project<C: GenericClient>(
    conn: &C,
    s: &str,
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
        Event::Created { id: new_id, spec } => return insert_task(conn, s, new_id, at, spec).await,
        Event::Admitted => ("SET status = 'admitted' WHERE id = $1", vec![&id]),
        Event::Claimed { holder, .. } => (
            "SET status = 'claimed', holder = $2, lease_until = $3,
                attempts = attempts + 1 WHERE id = $1",
            vec![&id, holder, &until_text],
        ),
        Event::Heartbeat { .. } => ("SET lease_until = $2 WHERE id = $1", vec![&id, &until_text]),
        Event::Released => (
            "SET status = 'admitted', holder = NULL, lease_until = NULL WHERE id = $1",
            vec![&id],
        ),
        Event::HandoffLeft { handoff } => ("SET handoff = $2 WHERE id = $1", vec![&id, handoff]),
        Event::Completed { handoff } => (
            "SET status = 'completed', handoff = $2, lease_until = NULL WHERE id = $1",
            vec![&id, handoff],
        ),
        Event::Invalidated {
            reason,
            superseded_by,
        } => (
            "SET status = 'invalidated', reason = $2, superseded_by = $3,
                lease_until = NULL WHERE id = $1",
            vec![&id, reason, superseded_by],
        ),
        Event::Posted { to } => ("SET posted_to = $2 WHERE id = $1", vec![&id, to]),
    };
    conn.execute(&format!("UPDATE {s}.tasks {sql}"), &args)
        .await?;
    Ok(())
}
