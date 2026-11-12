//! The PostgreSQL backend: the same tables and the same events, on a server several
//! processes can share. One connection, driven by a task of its own.
//!
//! One schema per domain, as the reference stack partitions everything by domain: a
//! domain's `events`, `tasks` and `promotions` live in a schema of their own, created
//! when the domain files its first task or records its first promotion, so a domain's
//! queue is a table no other domain's claims touch and a domain can be moved, backed up
//! or dropped as a unit. Two tables in `public` route the rest: `ledger_domains`, each
//! domain and its schema, and `ledger_tasks`, each task id and its domain, written in
//! the transaction that files the task, so an operation by id finds its schema in one
//! lookup. A promotion is no task and has no route there: it names its domain, and is
//! read by domain.
//!
//! Times are stored as the RFC 3339 text the SQLite backend stores, so a row reads back
//! exactly as it was written and a replay produces the same projection on either side.
//! A claim takes its candidates `FOR UPDATE SKIP LOCKED`, so two claimers on two
//! connections cannot both win the same task.

use crate::{
    check_holder, claimable, parse_time, Claimant, Error, Event, Held, PromotionRecord,
    PromotionSpec, RawPromotion, RawTask, Redaction, Status, Taken, Task, TaskSpec,
    PROMOTION_COLUMNS, TASK_COLUMNS,
};
use dm_core::Label;
use std::collections::HashMap;
use std::sync::Mutex;
use time::OffsetDateTime;
use tokio_postgres::error::SqlState;
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
    posted_to      TEXT,
    extra          TEXT
);
CREATE INDEX IF NOT EXISTS tasks_queue ON {s}.tasks (status, priority DESC, created);
CREATE UNIQUE INDEX IF NOT EXISTS tasks_origin ON {s}.tasks (origin, upstream_event)
    WHERE origin IS NOT NULL AND upstream_event IS NOT NULL;
";

/// One domain's promotions, created in its schema with the rest, and added at connect to
/// a schema made before promotions were recorded. A table an earlier shape made gets
/// each column it lacks, so the statements hold for every shape; the index is
/// [`PROMOTION_INDEX`], made again when the one there lacks the promoter. `{s}` is the
/// quoted schema name.
const PROMOTION_TABLE: &str = "
CREATE TABLE IF NOT EXISTS {s}.promotions (
    seq        BIGSERIAL PRIMARY KEY,
    domain     TEXT NOT NULL,
    entry      TEXT NOT NULL,
    tier       TEXT NOT NULL,
    promoter   TEXT NOT NULL,
    author     TEXT NOT NULL,
    why        TEXT NOT NULL,
    label_rank BIGINT NOT NULL,
    digest     TEXT NOT NULL,
    new_digest TEXT NOT NULL DEFAULT '',
    at         TEXT NOT NULL,
    why_erased BIGINT NOT NULL DEFAULT 0
);
ALTER TABLE {s}.promotions ADD COLUMN IF NOT EXISTS new_digest TEXT NOT NULL DEFAULT '';
ALTER TABLE {s}.promotions ADD COLUMN IF NOT EXISTS why_erased BIGINT NOT NULL DEFAULT 0;
";

/// The key a promotion is kept once under. `{s}` is the quoted schema name.
const PROMOTION_INDEX: &str = "
CREATE UNIQUE INDEX IF NOT EXISTS promotions_version
    ON {s}.promotions (entry, tier, digest, promoter);
";

/// Whether the promotions table `$1` names (`<schema>.promotions`, quoted) has an index
/// `promotions_version` that the promoter is part of.
const INDEX_HAS_PROMOTER: &str = "
SELECT EXISTS (
    SELECT 1 FROM pg_index i
    JOIN pg_class ic ON ic.oid = i.indexrelid
    JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = ANY (i.indkey)
    WHERE i.indrelid = to_regclass($1) AND ic.relname = 'promotions_version'
      AND a.attname = 'promoter')
";

/// The domains whose promotions table is missing, lacks a column, or is keyed without
/// the promoter: what a connect migrates, and nothing else.
const DOMAINS_TO_MIGRATE: &str = "
SELECT d.domain FROM public.ledger_domains d
LEFT JOIN pg_class c ON c.oid = to_regclass(d.schema || '.promotions')
WHERE c.oid IS NULL
   OR NOT EXISTS (SELECT 1 FROM pg_attribute a WHERE a.attrelid = c.oid
                  AND a.attname = 'new_digest' AND NOT a.attisdropped)
   OR NOT EXISTS (SELECT 1 FROM pg_attribute a WHERE a.attrelid = c.oid
                  AND a.attname = 'why_erased' AND NOT a.attisdropped)
   OR NOT EXISTS (SELECT 1 FROM pg_index i
                  JOIN pg_class ic ON ic.oid = i.indexrelid
                  JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = ANY (i.indkey)
                  WHERE i.indrelid = c.oid AND ic.relname = 'promotions_version'
                    AND a.attname = 'promoter')
ORDER BY d.domain
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
        extra: row.try_get(20)?,
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
        db.add_promotion_tables().await?;
        db.rebuild_stale_projections().await?;
        Ok(db)
    }

    /// Rebuild the projection of each domain whose tasks have a state name this code does
    /// not have, as a ledger from before the states were `open` and `done`: read as it
    /// stands, such a task would be in no list and no queue, without a word.
    async fn rebuild_stale_projections(&mut self) -> Result<(), Error> {
        for (domain, s) in self.schemas().await? {
            // a schema from before a task carried more than the ledger queries by: asked
            // of the catalog first, so a connect takes no lock on a table that has it
            let has_extra = self
                .client
                .query_opt(
                    "SELECT 1 FROM information_schema.columns
                     WHERE table_schema = $1 AND table_name = 'tasks' AND column_name = 'extra'",
                    // the catalog holds the name without the quotes the schema is written in
                    &[&s.trim_matches('"')],
                )
                .await?
                .is_some();
            if !has_extra {
                self.client
                    .batch_execute(&format!(
                        "ALTER TABLE {s}.tasks ADD COLUMN IF NOT EXISTS extra TEXT"
                    ))
                    .await?;
            }
            let row = self
                .client
                .query_one(
                    &format!(
                        "SELECT COUNT(*) FROM {s}.tasks WHERE status NOT IN
                             ('proposed', 'open', 'claimed', 'blocked', 'done', 'invalidated')"
                    ),
                    &[],
                )
                .await?;
            let stale: i64 = row.try_get(0)?;
            if stale > 0 {
                let tx = self.client.transaction().await?;
                rebuild_domain(&tx, &domain, &s).await?;
                tx.commit().await?;
            }
        }
        Ok(())
    }

    /// Give a promotions table to each domain schema made before promotions were
    /// recorded, and its missing columns and key to one an earlier shape made: one
    /// catalog query finds them, and only those are locked and migrated, so a connect
    /// costs nothing per domain whose table is as it should be.
    async fn add_promotion_tables(&mut self) -> Result<(), Error> {
        let rows = self.client.query(DOMAINS_TO_MIGRATE, &[]).await?;
        if rows.is_empty() {
            return Ok(());
        }
        let tx = self.client.transaction().await?;
        for row in &rows {
            let domain: String = row.try_get(0)?;
            ensure_promotions(&tx, &domain).await?;
        }
        tx.commit().await?;
        Ok(())
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

    pub(crate) async fn created_payload(&self, id: &str) -> Result<String, Error> {
        let s = self.schema_of_task(id).await?;
        let row = self
            .client
            .query_opt(
                &format!(
                    "SELECT payload FROM {s}.events WHERE task_id = $1 AND kind = 'created'
                     ORDER BY seq LIMIT 1"
                ),
                &[&id],
            )
            .await?
            .ok_or_else(|| Error::NotFound(id.to_string()))?;
        Ok(row.try_get(0)?)
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

    /// Append an event of the holder under the claim in force: the task's row is locked,
    /// read and the event appended in one transaction, so no claim comes between.
    pub(crate) async fn append_held(
        &mut self,
        id: &str,
        at: OffsetDateTime,
        by: Held<'_>,
        event: &Event,
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
        check_holder(&task_from_row(&row)?, by, at)?;
        apply(&tx, &s, id, at, event).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Who the ledger gave the claim of `generation` to: the holder of the task's
    /// `generation`th claim event, counted from one.
    pub(crate) async fn claimant(
        &self,
        id: &str,
        generation: u64,
    ) -> Result<Option<String>, Error> {
        let Some(before) = generation.checked_sub(1) else {
            return Ok(None);
        };
        let s = self.schema_of_task(id).await?;
        let row = self
            .client
            .query_opt(
                &format!(
                    "SELECT payload FROM {s}.events WHERE task_id = $1 AND kind = 'claimed'
                     ORDER BY seq LIMIT 1 OFFSET $2"
                ),
                &[&id, &i64::try_from(before).unwrap_or(i64::MAX)],
            )
            .await?;
        let payload: Option<String> = row.map(|r| r.try_get(0)).transpose()?;
        Ok(match payload.map(|p| Event::parse(id, &p)).transpose()? {
            Some(Event::Claimed { holder, .. }) => Some(holder),
            _ => None,
        })
    }

    /// The handoff a task was completed with under an attempt key, if it was.
    pub(crate) async fn completed_under(
        &self,
        task: &str,
        key: &str,
    ) -> Result<Option<String>, Error> {
        let s = self.schema_of_task(task).await?;
        let row = self
            .client
            .query_opt(
                &format!(
                    "SELECT payload FROM {s}.events
                     WHERE task_id = $1 AND kind = 'completed'
                       AND payload::jsonb ->> 'attempt' = $2
                     ORDER BY seq LIMIT 1"
                ),
                &[&task, &key],
            )
            .await?;
        let payload: Option<String> = row.map(|r| r.try_get(0)).transpose()?;
        Ok(match payload.map(|p| Event::parse(task, &p)).transpose()? {
            Some(Event::Completed { handoff, .. }) => Some(handoff),
            _ => None,
        })
    }

    /// The payloads of a task's events of one kind, oldest first.
    pub(crate) async fn payloads_of(&self, id: &str, kind: &str) -> Result<Vec<String>, Error> {
        let s = self.schema_of_task(id).await?;
        let rows = self
            .client
            .query(
                &format!(
                    "SELECT payload FROM {s}.events WHERE task_id = $1 AND kind = $2 ORDER BY seq"
                ),
                &[&id, &kind],
            )
            .await?;
        rows.iter().map(|r| Ok(r.try_get(0)?)).collect()
    }

    /// Record a promotion unless that promoter promoted that version of the entry to that
    /// tier already. The domain's lock is held from the check to the commit, so two
    /// processes recording the same promotion record it once.
    pub(crate) async fn record_promotion(
        &mut self,
        at: OffsetDateTime,
        spec: &PromotionSpec,
        why_erased: bool,
    ) -> Result<bool, Error> {
        let tx = self.client.transaction().await?;
        let s = ensure_domain(&tx, &spec.domain).await?;
        // a schema an older binary made has no table, or one of an earlier shape
        ensure_promotion_table(&tx, &s).await?;
        let seen = tx
            .query_opt(
                &format!(
                    "SELECT 1 FROM {s}.promotions
                     WHERE entry = $1 AND tier = $2 AND digest = $3 AND promoter = $4"
                ),
                &[&spec.entry, &spec.tier.to_string(), &spec.digest, &spec.by],
            )
            .await?
            .is_some();
        if seen {
            return Ok(false);
        }
        let key = crate::promotion_key(&spec.entry);
        apply(
            &tx,
            &s,
            &key,
            at,
            &Event::Promoted {
                spec: spec.clone(),
                why_erased,
            },
        )
        .await?;
        tx.commit().await?;
        self.remember(&spec.domain, &s);
        Ok(true)
    }

    /// Remove the why from an entry's promotion events and rows, in one transaction under
    /// the domain's lock. Returns how many records of the entry hold no why now.
    pub(crate) async fn scrub_promotions(
        &mut self,
        domain: &str,
        entry: &str,
    ) -> Result<usize, Error> {
        let Some(s) = self.schema(domain).await? else {
            return Ok(0);
        };
        let tx = self.client.transaction().await?;
        ensure_promotions(&tx, domain).await?;
        let n = scrub_promotions_in(&tx, &s, domain, entry).await?;
        tx.commit().await?;
        Ok(n)
    }

    /// Rewrite the tasks filed from `origin` to the redaction, in their events and their
    /// rows, and invalidate the open ones, in one transaction under the domain's lock.
    /// Returns how many tasks from that origin there are.
    pub(crate) async fn scrub_tasks(
        &mut self,
        at: OffsetDateTime,
        domain: &str,
        origin: &str,
        redaction: &Redaction,
    ) -> Result<usize, Error> {
        let Some(s) = self.schema(domain).await? else {
            return Ok(0);
        };
        let tx = self.client.transaction().await?;
        lock_domain(&tx, domain).await?;
        let n = scrub_tasks_in(&tx, &s, at, origin, redaction).await?;
        tx.commit().await?;
        Ok(n)
    }

    /// Both scrubs of an erasure in one transaction under the domain's lock. Returns the
    /// records that hold no why now and the tasks from the origin.
    pub(crate) async fn scrub_entry(
        &mut self,
        at: OffsetDateTime,
        domain: &str,
        entry: &str,
        origin: &str,
        redaction: &Redaction,
    ) -> Result<(usize, usize), Error> {
        let Some(s) = self.schema(domain).await? else {
            return Ok((0, 0));
        };
        let tx = self.client.transaction().await?;
        ensure_promotions(&tx, domain).await?;
        let records = scrub_promotions_in(&tx, &s, domain, entry).await?;
        let tasks = scrub_tasks_in(&tx, &s, at, origin, redaction).await?;
        tx.commit().await?;
        Ok((records, tasks))
    }

    pub(crate) async fn promotions(
        &self,
        domain: &str,
        entry: &str,
    ) -> Result<Vec<PromotionRecord>, Error> {
        let Some(s) = self.schema(domain).await? else {
            return Ok(Vec::new());
        };
        let rows = match self
            .client
            .query(
                &format!(
                    "SELECT {PROMOTION_COLUMNS} FROM {s}.promotions
                     WHERE domain = $1 AND entry = $2 ORDER BY seq"
                ),
                &[&domain, &entry],
            )
            .await
        {
            Ok(rows) => rows,
            // a schema an older binary made has no table, and so no promotions
            Err(e) if e.code() == Some(&SqlState::UNDEFINED_TABLE) => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        rows.iter()
            .map(|r| {
                RawPromotion {
                    domain: r.try_get(0)?,
                    entry: r.try_get(1)?,
                    tier: r.try_get(2)?,
                    promoter: r.try_get(3)?,
                    author: r.try_get(4)?,
                    why: r.try_get(5)?,
                    label_rank: r.try_get(6)?,
                    digest: r.try_get(7)?,
                    new_digest: r.try_get(8)?,
                    at: r.try_get(9)?,
                    why_erased: r.try_get(10)?,
                }
                .into_record()
            })
            .collect()
    }

    /// The id of the task claimed, if any candidate qualified. The candidates are read
    /// in queue order; the first the manifest can take is then locked on its own, `FOR
    /// UPDATE SKIP LOCKED`, and read again as it now stands, so a row another claimer
    /// holds or has just taken is passed over rather than claimed twice.
    pub(crate) async fn claim_next(
        &mut self,
        domain: &str,
        now: OffsetDateTime,
        who: Claimant<'_>,
        until: OffsetDateTime,
        manifest: &[String],
        handling: Label,
    ) -> Result<Option<Taken>, Error> {
        let Some(s) = self.schema(domain).await? else {
            return Ok(None);
        };
        // every task that may be claimable. Whether a claimed one's lease has run out is
        // asked of the parsed time: as text, a time with fewer digits of a second sorts
        // after a later one with more
        let claimable_now = "status IN ('open', 'blocked', 'claimed')";
        let tx = self.client.transaction().await?;
        // the claim this key made in this domain, looked for under the domain's lock, so
        // two connections with one key make one claim
        if let Some(key) = who.attempt {
            lock_domain(&tx, domain).await?;
            let before = tx
                .query_opt(
                    &format!(
                        "SELECT task_id, payload FROM {s}.events
                         WHERE kind = 'claimed' AND payload::jsonb ->> 'attempt' = $1
                         ORDER BY seq LIMIT 1"
                    ),
                    &[&key],
                )
                .await?;
            if let Some(row) = before {
                let (id, payload): (String, String) = (row.try_get(0)?, row.try_get(1)?);
                let generation = match Event::parse(&id, &payload)? {
                    Event::Claimed { generation, .. } => generation,
                    _ => 0,
                };
                return Ok(Some(Taken::Again { id, generation }));
            }
        }
        let rows = tx
            .query(
                &format!("SELECT {TASK_COLUMNS} FROM {s}.tasks WHERE {claimable_now}"),
                &[],
            )
            .await?;
        let mut candidates = rows
            .iter()
            .map(task_from_row)
            .collect::<Result<Vec<_>, _>>()?;
        crate::in_queue_order(&mut candidates);
        for seen in candidates {
            if !claimable(&seen, now, manifest, handling) {
                continue;
            }
            if take(&tx, &s, &seen.id, now, (who, until), manifest, handling).await? {
                tx.commit().await?;
                return Ok(Some(Taken::New(seen.id)));
            }
        }
        Ok(None)
    }

    /// [`take`] in a transaction of its own, for a test of what it refuses.
    #[cfg(test)]
    pub(crate) async fn take_alone(
        &mut self,
        id: &str,
        now: OffsetDateTime,
        who: (&str, OffsetDateTime),
    ) -> Result<bool, Error> {
        let who = (
            Claimant {
                holder: who.0,
                attempt: None,
            },
            who.1,
        );
        let s = self.schema_of_task(id).await?;
        let tx = self.client.transaction().await?;
        let taken = take(&tx, &s, id, now, who, &[], Label::Restricted).await?;
        tx.commit().await?;
        Ok(taken)
    }

    pub(crate) async fn claim(
        &mut self,
        id: &str,
        now: OffsetDateTime,
        who: Claimant<'_>,
        until: OffsetDateTime,
        manifest: &[String],
        handling: Label,
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
        if !claimable(&task, now, manifest, handling)
            || !deps_complete(&tx, &task.spec.depends_on).await?
            || awaits_continuation(&tx, &s, &task.id).await?
        {
            return Err(Error::WrongState {
                id: id.to_string(),
                status: task.status,
                op: "claim",
            });
        }
        let claimed = Event::Claimed {
            holder: who.holder.to_string(),
            until,
            generation: task.generation + 1,
            attempt: who.attempt.map(str::to_string),
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

    pub(crate) async fn with_origin(&self, domain: &str, origin: &str) -> Result<Vec<Task>, Error> {
        let Some(s) = self.schema(domain).await? else {
            return Ok(Vec::new());
        };
        self.select(&s, "WHERE origin = $1 ORDER BY created", &[&origin])
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
            "WHERE status = 'done' AND origin IS NOT NULL AND posted_to IS NULL
             ORDER BY created",
            &[],
        )
        .await
    }

    /// The tasks that depend on `id`, in whichever domain: a dependency may be in
    /// another domain's schema than its dependents.
    pub(crate) async fn dependents(&self, id: &str) -> Result<Vec<Task>, Error> {
        let mut out = Vec::new();
        for (_, s) in self.schemas().await? {
            out.extend(
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
                .await?,
            );
        }
        Ok(out)
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

/// Lock one candidate's row and claim it for `who`, a holder and the end of its lease, if
/// the task is still claimable now that the row is held. The candidate was read before
/// the lock: another connection may have claimed it and committed since, and this check
/// is then all that stands between the task and a second holder. A row another
/// connection holds is passed over.
async fn take<C: GenericClient>(
    tx: &C,
    s: &str,
    id: &str,
    now: OffsetDateTime,
    who: (Claimant<'_>, OffsetDateTime),
    manifest: &[String],
    handling: Label,
) -> Result<bool, Error> {
    let locked = tx
        .query_opt(
            &format!("SELECT {TASK_COLUMNS} FROM {s}.tasks WHERE id = $1 FOR UPDATE SKIP LOCKED"),
            &[&id],
        )
        .await?;
    let Some(locked) = locked else {
        return Ok(false);
    };
    let task = task_from_row(&locked)?;
    if !claimable(&task, now, manifest, handling)
        || !deps_complete(tx, &task.spec.depends_on).await?
        || awaits_continuation(tx, s, &task.id).await?
    {
        return Ok(false);
    }
    let claimed = Event::Claimed {
        holder: who.0.holder.to_string(),
        until: who.1,
        generation: task.generation + 1,
        attempt: who.0.attempt.map(str::to_string),
    };
    apply(tx, s, &task.id, now, &claimed).await?;
    Ok(true)
}

/// Take a domain's lock until the transaction ends.
async fn lock_domain<C: GenericClient>(conn: &C, domain: &str) -> Result<(), Error> {
    conn.execute(
        "SELECT pg_advisory_xact_lock(hashtext('ledger_domain:' || $1))",
        &[&domain],
    )
    .await?;
    Ok(())
}

/// Give a domain's schema its promotions table if it has none, under the domain's lock,
/// held until the caller's transaction ends. The schema's name comes from the routing
/// table, read under the lock.
async fn ensure_promotions<C: GenericClient>(conn: &C, domain: &str) -> Result<(), Error> {
    lock_domain(conn, domain).await?;
    let row = conn
        .query_one(
            "SELECT schema FROM public.ledger_domains WHERE domain = $1",
            &[&domain],
        )
        .await?;
    let s: String = row.try_get(0)?;
    ensure_promotion_table(conn, &s).await
}

/// Make schema `s`'s promotions table if it is not there, give it each column it lacks,
/// and make its key again when the one there lacks the promoter. The caller holds the
/// domain's lock.
async fn ensure_promotion_table<C: GenericClient>(conn: &C, s: &str) -> Result<(), Error> {
    conn.batch_execute(&PROMOTION_TABLE.replace("{s}", s))
        .await?;
    let row = conn
        .query_one(INDEX_HAS_PROMOTER, &[&format!("{s}.promotions")])
        .await?;
    let keyed_by_promoter: bool = row.try_get(0)?;
    if !keyed_by_promoter {
        conn.batch_execute(&format!(
            "DROP INDEX IF EXISTS {s}.promotions_version;{}",
            PROMOTION_INDEX.replace("{s}", s)
        ))
        .await?;
    }
    Ok(())
}

/// Make a domain's schema and tables if they are not there, under a lock on the domain
/// so two processes filing a domain's first task do not race to create it. The lock is
/// held until the caller's transaction ends.
async fn ensure_domain<C: GenericClient>(conn: &C, domain: &str) -> Result<String, Error> {
    lock_domain(conn, domain).await?;
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
        "CREATE SCHEMA IF NOT EXISTS {s};{}{}{}",
        DOMAIN_TABLES.replace("{s}", &s),
        PROMOTION_TABLE.replace("{s}", &s),
        PROMOTION_INDEX.replace("{s}", &s)
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
        if status.as_deref() != Some("done") {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Whether a continuation of the task is still open, in its domain's schema: its work
/// goes on there, and the continuation's completion closes it.
async fn awaits_continuation<C: GenericClient>(conn: &C, s: &str, id: &str) -> Result<bool, Error> {
    Ok(conn
        .query_opt(
            &format!(
                "SELECT 1 FROM {s}.tasks WHERE origin = $1
                 AND status NOT IN ('done', 'invalidated') LIMIT 1"
            ),
            &[&crate::continues(id)],
        )
        .await?
        .is_some())
}

/// Drop one domain's projection and replay its events into it.
async fn rebuild_domain<C: GenericClient>(conn: &C, domain: &str, s: &str) -> Result<u64, Error> {
    // a schema an older binary made after this process connected has no promotions table
    ensure_promotions(conn, domain).await?;
    conn.execute(&format!("DELETE FROM {s}.tasks"), &[]).await?;
    conn.execute(&format!("DELETE FROM {s}.promotions"), &[])
        .await?;
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

/// The scrub of an entry's promotions in schema `s`, in the caller's transaction: the why
/// leaves its events and its rows. Returns how many records of the entry hold no why now.
async fn scrub_promotions_in<C: GenericClient>(
    conn: &C,
    s: &str,
    domain: &str,
    entry: &str,
) -> Result<usize, Error> {
    let key = crate::promotion_key(entry);
    let rows = conn
        .query(
            &format!(
                "SELECT seq, payload FROM {s}.events
                 WHERE task_id = $1 AND kind = 'promoted' FOR UPDATE"
            ),
            &[&key],
        )
        .await?;
    for row in &rows {
        let seq: i64 = row.try_get(0)?;
        let payload: String = row.try_get(1)?;
        if let Some(scrubbed) = crate::scrub_payload(&key, domain, &payload)? {
            conn.execute(
                &format!("UPDATE {s}.events SET payload = $2 WHERE seq = $1"),
                &[&seq, &scrubbed],
            )
            .await?;
        }
    }
    conn.execute(
        &format!(
            "UPDATE {s}.promotions SET why = '', why_erased = 1
             WHERE domain = $1 AND entry = $2 AND (why_erased = 0 OR why != '')"
        ),
        &[&domain, &entry],
    )
    .await?;
    let row = conn
        .query_one(
            &format!(
                "SELECT COUNT(*) FROM {s}.promotions
                 WHERE domain = $1 AND entry = $2 AND why_erased = 1"
            ),
            &[&domain, &entry],
        )
        .await?;
    let n: i64 = row.try_get(0)?;
    Ok(usize::try_from(n).unwrap_or(0))
}

/// The scrub of the tasks filed from `origin` in schema `s`, in the caller's transaction:
/// each is rewritten to the redaction and the open ones are invalidated. Returns how many
/// tasks there are from that origin.
async fn scrub_tasks_in<C: GenericClient>(
    conn: &C,
    s: &str,
    at: OffsetDateTime,
    origin: &str,
    redaction: &Redaction,
) -> Result<usize, Error> {
    let rows = conn
        .query(
            &format!(
                "SELECT id, status IN ('done', 'invalidated') FROM {s}.tasks
                 WHERE origin = $1 ORDER BY created FOR UPDATE"
            ),
            &[&origin],
        )
        .await?;
    for row in &rows {
        let id: String = row.try_get(0)?;
        let terminal: bool = row.try_get(1)?;
        if !terminal {
            let closed = Event::Invalidated {
                reason: redaction.reason.clone(),
                superseded_by: None,
            };
            apply(conn, s, &id, at, &closed).await?;
        }
        redact_task(conn, s, &id, redaction).await?;
    }
    Ok(rows.len())
}

/// One task's events and row, rewritten to the redaction: the title and body it was
/// filed with, the reason it was invalidated for, and the handoffs left on it or that
/// closed it, each free text an approver may have put the entry's text in.
async fn redact_task<C: GenericClient>(
    conn: &C,
    s: &str,
    id: &str,
    redaction: &Redaction,
) -> Result<(), Error> {
    let rows = conn
        .query(
            &format!("SELECT seq, payload FROM {s}.events WHERE task_id = $1 FOR UPDATE"),
            &[&id],
        )
        .await?;
    for row in &rows {
        let seq: i64 = row.try_get(0)?;
        let payload: String = row.try_get(1)?;
        if let Some(redacted) = crate::redact_event(id, &payload, redaction)? {
            conn.execute(
                &format!("UPDATE {s}.events SET payload = $2 WHERE seq = $1"),
                &[&seq, &redacted],
            )
            .await?;
        }
    }
    conn.execute(
        &format!(
            "UPDATE {s}.tasks SET title = $2, body = $3,
                reason = CASE WHEN reason IS NULL THEN NULL ELSE $4 END,
                handoff = CASE WHEN handoff IS NULL THEN NULL ELSE $4 END
             WHERE id = $1"
        ),
        &[&id, &redaction.title, &redaction.body, &redaction.reason],
    )
    .await?;
    Ok(())
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
                upstream_event, created_by, label_rank, priority, created, status, extra)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, 'proposed', $13)"
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
            &spec.extra(),
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

/// The row a `Promoted` event projects to. No route: a promotion is read by its domain.
async fn insert_promotion<C: GenericClient>(
    conn: &C,
    s: &str,
    at: OffsetDateTime,
    p: &PromotionSpec,
    why_erased: bool,
) -> Result<(), Error> {
    conn.execute(
        &format!(
            "INSERT INTO {s}.promotions (domain, entry, tier, promoter, author, why,
                label_rank, digest, new_digest, at, why_erased)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)"
        ),
        &[
            &p.domain,
            &p.entry,
            &p.tier.to_string(),
            &p.by,
            &p.author,
            &p.why,
            &(p.classification as i64),
            &p.digest,
            &p.new_digest,
            &crate::fmt_time(at),
            &i64::from(why_erased),
        ],
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
        Event::Promoted { spec, why_erased } => {
            return insert_promotion(conn, s, at, spec, *why_erased).await
        }
        Event::Admitted => ("SET status = 'open' WHERE id = $1", vec![&id]),
        Event::Blocked { .. } => (
            "SET status = 'blocked' WHERE id = $1 AND status = 'open'",
            vec![&id],
        ),
        Event::Unblocked { .. } => (
            "SET status = 'open' WHERE id = $1 AND status = 'blocked'",
            vec![&id],
        ),
        Event::Retriaged { .. } => (
            "SET status = 'proposed' WHERE id = $1 AND status IN ('open', 'blocked')",
            vec![&id],
        ),
        // the record that someone tried, or spoke after its claim: nothing of the task moves
        Event::ClaimRefused { .. } | Event::LateHandoff { .. } => return Ok(()),
        Event::Claimed { holder, .. } => (
            "SET status = 'claimed', holder = $2, lease_until = $3,
                attempts = attempts + 1 WHERE id = $1",
            vec![&id, holder, &until_text],
        ),
        Event::Heartbeat { .. } => ("SET lease_until = $2 WHERE id = $1", vec![&id, &until_text]),
        Event::Released => (
            "SET status = 'open', holder = NULL, lease_until = NULL WHERE id = $1",
            vec![&id],
        ),
        Event::HandoffLeft { handoff } => ("SET handoff = $2 WHERE id = $1", vec![&id, handoff]),
        Event::Completed { handoff, .. } => (
            "SET status = 'done', handoff = $2, lease_until = NULL WHERE id = $1",
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
        Event::Rescored { priority, .. } => {
            ("SET priority = $2 WHERE id = $1", vec![&id, priority])
        }
    };
    conn.execute(&format!("UPDATE {s}.tasks {sql}"), &args)
        .await?;
    Ok(())
}
