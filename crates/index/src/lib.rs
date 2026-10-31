//! The search index: summaries first, filtered inside every query, rebuildable from the
//! record alone.
//!
//! The index is derived. It holds one row of metadata per entry and a full-text table
//! over title, summary, and body, ranked with BM25. Nothing here is authoritative: delete
//! the file, run [`Index::rebuild`] over the record, and the same index comes back.
//!
//! Every read takes a [`ReadFilter`] and applies it in SQL. A hit, a count, or a summary
//! never reflects an entry the filter does not admit, because the rows were never
//! selected. That is the property the adversarial pass attacks first; keep it in the
//! query, not in Rust after the query.
//!
//! This backend is SQLite with FTS5, one file, nothing to operate. A second backend
//! (Solr, Tantivy) would implement the same methods; the surface is what matters.

use dm_core::{Entry, EntryType, Label, ReadFilter, Status, TrustTier};
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Row, TransactionBehavior};
use std::path::Path;
use time::{format_description::well_known::Rfc3339, Date, OffsetDateTime};

/// What can go wrong at the index.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// SQLite refused or failed.
    #[error("index: {0}")]
    Db(#[from] rusqlite::Error),
    /// Another connection kept the write-ahead log from being checkpointed, so bytes
    /// removed from the tables are still in it.
    #[error(
        "index: another connection holds the write-ahead log; what was removed is still in it"
    )]
    Held,
    /// A row holds something the index did not write. The index is corrupt; rebuild it.
    #[error("index row for {id}: {what}")]
    BadRow {
        /// The entry the row is for.
        id: String,
        /// What was wrong.
        what: String,
    },
}

/// One search result: everything the caller needs to decide whether to open the entry.
#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    /// The entry's id.
    pub id: String,
    /// Its domain.
    pub domain: String,
    /// Its kind.
    pub entry_type: EntryType,
    /// How far it has been checked.
    pub trust_tier: TrustTier,
    /// How sensitive it is.
    pub classification: Label,
    /// The title the index shows.
    pub title: String,
    /// The one-line summary.
    pub summary: String,
    /// BM25 rank. Lower is better; zero for keyed lookups that have no query.
    pub score: f64,
    /// Whether the entry's `review_by` has passed: the decay clock the paper gives facts,
    /// shown with the hit so a reader sees it before leaning on it.
    pub review_due: bool,
}

/// An entry's indexed metadata, without its body.
#[derive(Clone, Debug, PartialEq)]
pub struct Meta {
    /// The hit-shaped part.
    pub hit: Hit,
    /// Who wrote it.
    pub author: String,
    /// When.
    pub created: OffsetDateTime,
    /// Its decay clock, if any.
    pub review_by: Option<Date>,
    /// Its lifecycle state; `None` means active.
    pub status: Option<Status>,
    /// The entry it replaces, if any.
    pub supersedes: Option<String>,
    /// Where it came from, if imported or tapped.
    pub origin: Option<String>,
    /// Digest of the rendered entry at indexing time.
    pub digest: String,
}

/// The index, over one SQLite connection.
pub struct Index {
    conn: Connection,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS entries (
    id             TEXT PRIMARY KEY,
    domain         TEXT NOT NULL,
    entry_type     TEXT NOT NULL,
    author         TEXT NOT NULL,
    created        TEXT NOT NULL,
    created_unix   INTEGER,
    created_nanos  INTEGER,
    review_by      TEXT,
    tier_rank      INTEGER NOT NULL,
    label_rank     INTEGER NOT NULL,
    status         TEXT,
    supersedes     TEXT,
    origin         TEXT,
    title          TEXT NOT NULL,
    summary        TEXT NOT NULL,
    digest         TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS entries_domain ON entries (domain);
CREATE INDEX IF NOT EXISTS entries_review ON entries (review_by);
CREATE INDEX IF NOT EXISTS entries_origin ON entries (origin);
CREATE VIRTUAL TABLE IF NOT EXISTS entries_fts USING fts5 (
    id UNINDEXED, title, summary, body, tokenize = 'porter unicode61'
);
INSERT OR IGNORE INTO meta (key, value) VALUES ('schema', '1');
";

/// The order of `created`: whole seconds since the Unix epoch, then the nanoseconds within
/// that second. `created` itself is the frontmatter's RFC 3339 text, which does not sort
/// as text: `10:00:00Z` sorts after `10:00:00.5Z`, `.15Z` before `.1Z`, and an offset other
/// than UTC by its local hour. Both are integers, so a year at either end of what the
/// frontmatter accepts still has a place. An older binary writes rows without them: the
/// trigger [`SORT_KEY_TRIGGER`] fills those in from `created` as the row lands, to the
/// millisecond, and [`Index::sort_keys`] fills in any left on the next open.
const NEWEST_FIRST: &str = "e.created_unix DESC, e.created_nanos DESC";

/// Lives in the file, so it fires for whichever binary inserts the row.
const SORT_KEY_TRIGGER: &str = "
CREATE TRIGGER IF NOT EXISTS entries_sort_key AFTER INSERT ON entries
WHEN NEW.created_unix IS NULL
BEGIN
    UPDATE entries SET
        created_unix = unixepoch(NEW.created),
        created_nanos = min(999, CAST(
            (unixepoch(NEW.created, 'subsec') - unixepoch(NEW.created)) * 1000 + 0.5
            AS INTEGER)) * 1000000
    WHERE id = NEW.id;
END;";

fn sort_key(at: OffsetDateTime) -> (i64, i64) {
    (at.unix_timestamp(), i64::from(at.nanosecond()))
}

const HIT_COLUMNS: &str =
    "e.id, e.domain, e.entry_type, e.tier_rank, e.label_rank, e.title, e.summary, e.review_by";

fn tier_from_rank(id: &str, rank: i64) -> Result<TrustTier, Error> {
    match rank {
        0 => Ok(TrustTier::Raw),
        1 => Ok(TrustTier::Curated),
        2 => Ok(TrustTier::Verified),
        other => Err(Error::BadRow {
            id: id.to_string(),
            what: format!("tier rank {other}"),
        }),
    }
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

fn status_from_text(id: &str, s: Option<String>) -> Result<Option<Status>, Error> {
    match s.as_deref() {
        None => Ok(None),
        Some("demoted") => Ok(Some(Status::Demoted)),
        Some("superseded") => Ok(Some(Status::Superseded)),
        Some("erased") => Ok(Some(Status::Erased)),
        Some(other) => Err(Error::BadRow {
            id: id.to_string(),
            what: format!("status {other:?}"),
        }),
    }
}

fn status_to_text(s: Option<Status>) -> Option<&'static str> {
    s.map(|s| match s {
        Status::Demoted => "demoted",
        Status::Superseded => "superseded",
        Status::Erased => "erased",
    })
}

fn hit_from_row(row: &Row<'_>, score: f64) -> Result<Hit, Error> {
    let id: String = row.get(0)?;
    let entry_type: String = row.get(2)?;
    let entry_type = entry_type.parse().map_err(|_| Error::BadRow {
        id: id.clone(),
        what: format!("entry type {entry_type:?}"),
    })?;
    // the decay clock, read at query time: an entry past its review date is served
    // with that said, so the reader sees it before leaning on it
    let review_by: Option<String> = row.get(7)?;
    let today = OffsetDateTime::now_utc().date();
    let review_due = review_by
        .as_deref()
        .and_then(|d| Date::parse(d, &time::format_description::well_known::Iso8601::DATE).ok())
        .is_some_and(|d| d <= today);
    Ok(Hit {
        trust_tier: tier_from_rank(&id, row.get(3)?)?,
        classification: label_from_rank(&id, row.get(4)?)?,
        id,
        domain: row.get(1)?,
        entry_type,
        title: row.get(5)?,
        summary: row.get(6)?,
        score,
        review_due,
    })
}

/// Turn free text into an FTS5 query that cannot fail to parse: every whitespace-separated
/// token becomes a quoted phrase, and the phrases are ANDed. `None` when there is nothing
/// to search for.
/// The FTS5 query for a text: every word a quoted phrase, so user input can never be
/// query syntax; joined with AND (implicit) or, for the fallback, OR.
fn fts_query(text: &str, any_word: bool) -> Option<String> {
    let parts: Vec<String> = text
        .split_whitespace()
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect();
    if parts.is_empty() {
        None
    } else if any_word {
        Some(parts.join(" OR "))
    } else {
        Some(parts.join(" "))
    }
}

/// The WHERE fragment and parameters a filter compiles to. Positional parameters start at
/// `first`, so the caller can put its own before them.
fn compile_filter(f: &ReadFilter, first: usize) -> (String, Vec<rusqlite::types::Value>) {
    let mut sql = format!(
        " AND e.label_rank <= ?{} AND e.tier_rank >= ?{}",
        first,
        first + 1
    );
    let mut values: Vec<rusqlite::types::Value> =
        vec![(f.handling as i64).into(), (f.min_tier as i64).into()];
    if !f.include_inactive {
        sql.push_str(" AND e.status IS NULL");
    }
    if !f.domains.is_empty() {
        let marks: Vec<String> = (0..f.domains.len())
            .map(|i| format!("?{}", first + 2 + i))
            .collect();
        sql.push_str(&format!(" AND e.domain IN ({})", marks.join(", ")));
        values.extend(f.domains.iter().map(|d| d.clone().into()));
    }
    (sql, values)
}

/// The longest chain of replacements [`Index::replaced_by`] walks, against a cycle.
const MAX_CHAIN: usize = 64;

impl Index {
    /// Open or create the index file.
    ///
    /// # Errors
    ///
    /// When the file cannot be opened or the schema cannot be created.
    pub fn open(path: &Path) -> Result<Index, Error> {
        let conn = Connection::open(path)?;
        Index::init(conn)
    }

    /// An index that lives only in memory.
    ///
    /// # Errors
    ///
    /// When SQLite cannot create the schema, which it can always do in memory.
    pub fn in_memory() -> Result<Index, Error> {
        Index::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Index, Error> {
        // secure_delete overwrites what a delete frees, so a removed body does not wait
        // in a free page for the next write
        conn.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON; PRAGMA secure_delete = ON;",
        )?;
        conn.execute_batch(SCHEMA)?;
        let mut ix = Index { conn };
        ix.sort_keys()?;
        Ok(ix)
    }

    /// Give every row its sort key: add the columns to an index made before they existed,
    /// and fill in the rows written without them. Looks first without a lock, so an open
    /// with nothing to do writes nothing; the work itself holds the write lock from its
    /// first read, so two processes opening at once do it once.
    fn sort_keys(&mut self) -> Result<(), Error> {
        if self.sort_keys_done()? {
            return Ok(());
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let has: i64 = tx.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('entries') WHERE name = 'created_unix'",
            [],
            |r| r.get(0),
        )?;
        if has == 0 {
            tx.execute_batch(
                "ALTER TABLE entries ADD COLUMN created_unix INTEGER;
                 ALTER TABLE entries ADD COLUMN created_nanos INTEGER;",
            )?;
        }
        tx.execute_batch(
            "CREATE INDEX IF NOT EXISTS entries_created ON entries (created_unix, created_nanos);",
        )?;
        tx.execute_batch(SORT_KEY_TRIGGER)?;
        let rows: Vec<(String, String)> = {
            let mut stmt =
                tx.prepare("SELECT id, created FROM entries WHERE created_unix IS NULL")?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect::<Result<_, _>>()?
        };
        {
            let mut update = tx.prepare(
                "UPDATE entries SET created_unix = ?1, created_nanos = ?2 WHERE id = ?3",
            )?;
            for (id, created) in rows {
                // a value that does not parse is left for the read to report as a bad row
                if let Ok(at) = OffsetDateTime::parse(&created, &Rfc3339) {
                    let (unix, nanos) = sort_key(at);
                    update.execute(params![unix, nanos, id])?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    fn sort_keys_done(&self) -> Result<bool, Error> {
        let has: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('entries') WHERE name = 'created_unix'",
            [],
            |r| r.get(0),
        )?;
        if has == 0 {
            return Ok(false);
        }
        let indexed: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE (type = 'index' AND name = 'entries_created')
                OR (type = 'trigger' AND name = 'entries_sort_key')",
            [],
            |r| r.get(0),
        )?;
        let missing: Option<i64> = self
            .conn
            .query_row(
                "SELECT 1 FROM entries WHERE created_unix IS NULL LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()?;
        Ok(indexed == 2 && missing.is_none())
    }

    /// Index an entry, replacing whatever the index held for its id.
    ///
    /// # Errors
    ///
    /// A database error.
    pub fn upsert(&mut self, entry: &Entry) -> Result<(), Error> {
        let tx = self.conn.transaction()?;
        Index::upsert_in(&tx, entry)?;
        tx.commit()?;
        Ok(())
    }

    fn upsert_in(tx: &rusqlite::Transaction<'_>, entry: &Entry) -> Result<(), Error> {
        let f = &entry.front;
        let created = f
            .created
            .format(&Rfc3339)
            .unwrap_or_else(|_| String::from("1970-01-01T00:00:00Z"));
        let review_by = f.review_by.map(|d| d.to_string());
        let (created_unix, created_nanos) = sort_key(f.created);
        tx.execute(
            "INSERT OR REPLACE INTO entries
             (id, domain, entry_type, author, created, review_by, tier_rank, label_rank,
              status, supersedes, origin, title, summary, digest, created_unix, created_nanos)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                f.id,
                f.domain,
                f.entry_type.to_string(),
                f.author,
                created,
                review_by,
                f.trust_tier as i64,
                f.classification as i64,
                status_to_text(f.status),
                f.supersedes,
                f.origin,
                entry.title(),
                entry.summary(),
                dm_core::digest(&entry.render()),
                created_unix,
                created_nanos,
            ],
        )?;
        tx.execute("DELETE FROM entries_fts WHERE id = ?1", params![f.id])?;
        tx.execute(
            "INSERT INTO entries_fts (id, title, summary, body) VALUES (?1, ?2, ?3, ?4)",
            params![f.id, entry.title(), entry.summary(), entry.body],
        )?;
        Ok(())
    }

    /// Forget an entry. True if the index held it.
    ///
    /// # Errors
    ///
    /// A database error.
    pub fn remove(&mut self, id: &str) -> Result<bool, Error> {
        let tx = self.conn.transaction()?;
        let n = tx.execute("DELETE FROM entries WHERE id = ?1", params![id])?;
        tx.execute("DELETE FROM entries_fts WHERE id = ?1", params![id])?;
        tx.commit()?;
        Ok(n > 0)
    }

    /// Leave no trace of what was removed: the full-text index merges its segments so
    /// deleted documents drop out of them, and the file gives its free pages back, so the
    /// bytes of a removed body are gone from disk rather than waiting to be overwritten.
    ///
    /// # Errors
    ///
    /// A database error.
    pub fn vacuum(&mut self) -> Result<(), Error> {
        self.conn
            .execute_batch("INSERT INTO entries_fts(entries_fts) VALUES('optimize');")?;
        self.checkpoint()?;
        self.conn.execute_batch("VACUUM;")?;
        self.checkpoint()
    }

    /// Fold the write-ahead log into the file and truncate it, waiting a little for a
    /// reader mid-query; [`Error::Held`] when it still could not, since the log then
    /// still holds what the tables no longer do.
    fn checkpoint(&self) -> Result<(), Error> {
        // a short wait per try: a held log is answered, not waited out
        self.conn
            .busy_timeout(std::time::Duration::from_millis(200))?;
        let mut held = true;
        for _ in 0..5 {
            let busy: i64 = self
                .conn
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))?;
            if busy == 0 {
                held = false;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        self.conn.busy_timeout(std::time::Duration::from_secs(5))?;
        if held {
            Err(Error::Held)
        } else {
            Ok(())
        }
    }

    /// Replace the whole index with what the record holds. One transaction: a reader
    /// sees the old index or the new one, never half.
    ///
    /// # Errors
    ///
    /// A database error; the index is left as it was.
    pub fn rebuild<I>(&mut self, entries: I) -> Result<usize, Error>
    where
        I: IntoIterator<Item = Entry>,
    {
        let tx = self.conn.transaction()?;
        tx.execute_batch("DELETE FROM entries; DELETE FROM entries_fts;")?;
        let mut n = 0;
        for e in entries {
            Index::upsert_in(&tx, &e)?;
            n += 1;
        }
        tx.commit()?;
        Ok(n)
    }

    /// Replace one domain's part of the index with `entries`, in one transaction: what a
    /// domain's materializer builds, leaving every other domain as it was.
    ///
    /// # Errors
    ///
    /// A database error; the index is left as it was.
    pub fn rebuild_domain<I>(&mut self, domain: &str, entries: I) -> Result<usize, Error>
    where
        I: IntoIterator<Item = Entry>,
    {
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM entries_fts WHERE id IN (SELECT id FROM entries WHERE domain = ?1)",
            params![domain],
        )?;
        tx.execute("DELETE FROM entries WHERE domain = ?1", params![domain])?;
        let mut n = 0;
        for e in entries {
            Index::upsert_in(&tx, &e)?;
            n += 1;
        }
        tx.commit()?;
        Ok(n)
    }

    /// Ranked full-text search over title, summary, and body, within the filter.
    /// Summaries only; the caller opens an entry with `get` if a summary warrants it.
    ///
    /// # Errors
    ///
    /// A database error, or a row the index did not write.
    pub fn search(
        &self,
        query: &str,
        filter: &ReadFilter,
        limit: usize,
    ) -> Result<Vec<Hit>, Error> {
        let Some(q) = fts_query(query, false) else {
            return Ok(Vec::new());
        };
        let hits = self.search_fts(&q, filter, limit)?;
        if !hits.is_empty() || query.split_whitespace().count() < 2 {
            return Ok(hits);
        }
        // every word as a phrase found nothing: a question in natural language rather
        // than a known item. Any word, still ranked by BM25, so the best partial match
        // leads. Explainable either way.
        match fts_query(query, true) {
            Some(q) => self.search_fts(&q, filter, limit),
            None => Ok(hits),
        }
    }

    fn search_fts(&self, q: &str, filter: &ReadFilter, limit: usize) -> Result<Vec<Hit>, Error> {
        let (where_sql, mut values) = compile_filter(filter, 2);
        let sql = format!(
            "SELECT {HIT_COLUMNS}, bm25(entries_fts, 0.0, 10.0, 5.0, 1.0) AS score
             FROM entries_fts f JOIN entries e ON e.id = f.id
             WHERE entries_fts MATCH ?1{where_sql}
             ORDER BY score, {NEWEST_FIRST}
             LIMIT {}",
            limit.max(1)
        );
        values.insert(0, q.to_string().into());
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(values), |row| {
            let score: f64 = row.get(8)?;
            Ok(hit_from_row(row, score))
        })?;
        rows.map(|r| r?).collect()
    }

    /// How many entries the filter admits. The same filter as a search, so the count
    /// cannot reveal what the search would not.
    ///
    /// # Errors
    ///
    /// A database error.
    pub fn count(&self, filter: &ReadFilter) -> Result<u64, Error> {
        let (where_sql, values) = compile_filter(filter, 1);
        let sql = format!("SELECT COUNT(*) FROM entries e WHERE 1 = 1{where_sql}");
        let n: i64 = self
            .conn
            .query_row(&sql, params_from_iter(values), |r| r.get(0))?;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    /// Entries of one kind, or of any kind, newest first, within the filter. The keyed
    /// lookup: handoffs for a domain, decisions for a domain.
    ///
    /// # Errors
    ///
    /// A database error, or a row the index did not write.
    pub fn list(
        &self,
        filter: &ReadFilter,
        kind: Option<EntryType>,
        limit: usize,
    ) -> Result<Vec<Hit>, Error> {
        let (where_sql, mut values) = compile_filter(filter, 2);
        let kind_sql = if kind.is_some() {
            " AND e.entry_type = ?1"
        } else {
            " AND ?1 = ?1"
        };
        values.insert(0, kind.map_or_else(String::new, |k| k.to_string()).into());
        let sql = format!(
            "SELECT {HIT_COLUMNS} FROM entries e WHERE 1 = 1{kind_sql}{where_sql}
             ORDER BY {NEWEST_FIRST} LIMIT {}",
            limit.max(1)
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(values), |row| Ok(hit_from_row(row, 0.0)))?;
        rows.map(|r| r?).collect()
    }

    /// Entries whose review date is on or before `on`, soonest first, within the filter.
    ///
    /// # Errors
    ///
    /// A database error, or a row the index did not write.
    pub fn due_for_review(
        &self,
        filter: &ReadFilter,
        on: Date,
        limit: usize,
    ) -> Result<Vec<Hit>, Error> {
        let (where_sql, mut values) = compile_filter(filter, 2);
        values.insert(0, on.to_string().into());
        let sql = format!(
            "SELECT {HIT_COLUMNS} FROM entries e
             WHERE e.review_by IS NOT NULL AND e.review_by <= ?1{where_sql}
             ORDER BY e.review_by, e.created_unix, e.created_nanos LIMIT {}",
            limit.max(1)
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(values), |row| Ok(hit_from_row(row, 0.0)))?;
        rows.map(|r| r?).collect()
    }

    /// Where a reader of a replaced entry goes: the end of its chain of successors,
    /// the newest active entry the filter admits, resolved in one query however many
    /// times the entry was replaced, so a reader is redirected once, not hop by hop.
    /// `None` when no active successor is admitted.
    ///
    /// # Errors
    ///
    /// A database error.
    pub fn replaced_by(&self, id: &str, filter: &ReadFilter) -> Result<Option<String>, Error> {
        let (where_sql, mut values) = compile_filter(filter, 2);
        values.insert(0, id.to_string().into());
        let sql = format!(
            "WITH RECURSIVE chain(id, depth) AS (
                 SELECT e.id, 1 FROM entries e WHERE e.supersedes = ?1
                 UNION
                 SELECT e.id, c.depth + 1 FROM entries e JOIN chain c ON e.supersedes = c.id
                 WHERE c.depth < {MAX_CHAIN}
             )
             SELECT e.id FROM chain c JOIN entries e ON e.id = c.id
             WHERE e.status IS NULL{where_sql}
             ORDER BY c.depth DESC, {NEWEST_FIRST} LIMIT 1"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = stmt.query(params_from_iter(values))?;
        Ok(match rows.next()? {
            Some(row) => Some(row.get(0)?),
            None => None,
        })
    }

    /// One entry's metadata, if the filter admits it. `None` both when the entry is absent
    /// and when it is not for this session to see; a caller cannot tell the two apart.
    ///
    /// # Errors
    ///
    /// A database error, or a row the index did not write.
    pub fn meta(&self, id: &str, filter: &ReadFilter) -> Result<Option<Meta>, Error> {
        let (where_sql, mut values) = compile_filter(filter, 2);
        values.insert(0, id.to_string().into());
        let sql = format!(
            "SELECT {HIT_COLUMNS}, e.author, e.created, e.review_by, e.status, e.supersedes,
                    e.origin, e.digest
             FROM entries e WHERE e.id = ?1{where_sql}"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let row = stmt
            .query_row(params_from_iter(values), |row| {
                // the hit's own columns come first, review_by among them now
                let hit = hit_from_row(row, 0.0);
                let created: String = row.get(9)?;
                let review_by: Option<String> = row.get(10)?;
                let status: Option<String> = row.get(11)?;
                Ok((
                    hit,
                    row.get::<_, String>(8)?,
                    created,
                    review_by,
                    status,
                    row.get::<_, Option<String>>(12)?,
                    row.get::<_, Option<String>>(13)?,
                    row.get::<_, String>(14)?,
                ))
            })
            .optional()?;
        let Some((hit, author, created, review_by, status, supersedes, origin, digest)) = row
        else {
            return Ok(None);
        };
        let hit = hit?;
        let created = OffsetDateTime::parse(&created, &Rfc3339).map_err(|_| Error::BadRow {
            id: hit.id.clone(),
            what: format!("created {created:?}"),
        })?;
        let review_by = match review_by {
            None => None,
            Some(s) => Some(
                Date::parse(&s, &time::format_description::well_known::Iso8601::DATE).map_err(
                    |_| Error::BadRow {
                        id: hit.id.clone(),
                        what: format!("review_by {s:?}"),
                    },
                )?,
            ),
        };
        let status = status_from_text(&hit.id, status)?;
        Ok(Some(Meta {
            hit,
            author,
            created,
            review_by,
            status,
            supersedes,
            origin,
            digest,
        }))
    }

    /// The id already indexed in `domain` with this origin, if any. Unfiltered by label
    /// on purpose: it serves the import path's dedup, which runs as the owner, and
    /// reveals only an id. Scoped to the domain, since one domain's record says nothing
    /// about another's.
    ///
    /// # Errors
    ///
    /// A database error.
    pub fn by_origin(&self, domain: &str, origin: &str) -> Result<Option<String>, Error> {
        Ok(self
            .conn
            .query_row(
                "SELECT id FROM entries WHERE domain = ?1 AND origin = ?2 LIMIT 1",
                params![domain, origin],
                |r| r.get(0),
            )
            .optional()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dm_core::Frontmatter;
    use time::macros::{date, datetime};

    fn entry(domain: &str, id: &str, title: &str, body: &str) -> Entry {
        Entry {
            front: Frontmatter {
                id: id.into(),
                entry_type: EntryType::Fact,
                domain: domain.into(),
                author: "human:test:me".into(),
                created: datetime!(2026-10-01 10:00 UTC),
                source: vec![],
                confidence: None,
                trust_tier: TrustTier::Raw,
                review_by: None,
                supersedes: None,
                policy_context: vec![],
                classification: Label::Public,
                status: None,
                origin: None,
                summary: None,
                title: Some(title.into()),
            },
            body: body.into(),
        }
    }

    fn ids(hits: &[Hit]) -> Vec<&str> {
        hits.iter().map(|h| h.id.as_str()).collect()
    }

    fn any() -> ReadFilter {
        ReadFilter::any_domain(Label::Restricted)
    }

    #[test]
    fn search_finds_by_body_and_ranks_title_matches_first() {
        let mut ix = Index::in_memory().unwrap();
        ix.upsert(&entry(
            "dom-a",
            "mem-a-body",
            "Unrelated title",
            "The retry policy uses backoff.\n",
        ))
        .unwrap();
        ix.upsert(&entry(
            "dom-a",
            "mem-a-title",
            "Retry policy",
            "Nothing else here.\n",
        ))
        .unwrap();
        ix.upsert(&entry(
            "dom-a",
            "mem-a-none",
            "Deployment notes",
            "Ship on Fridays.\n",
        ))
        .unwrap();
        let hits = ix.search("retry policy", &any(), 10).unwrap();
        assert_eq!(ids(&hits), vec!["mem-a-title", "mem-a-body"]);
        assert!(hits[0].score < hits[1].score, "bm25: lower is better");
        assert_eq!(hits[0].summary, "Nothing else here.");
        assert!(ix.search("fridays", &any(), 10).unwrap().len() == 1);
        assert!(ix.search("nowhere", &any(), 10).unwrap().is_empty());
    }

    #[test]
    fn the_filter_is_applied_inside_the_query() {
        let mut ix = Index::in_memory().unwrap();
        let mut secret = entry("dom-a", "mem-a-secret", "Budget", "The secret budget.\n");
        secret.front.classification = Label::Restricted;
        let mut internal = entry(
            "dom-a",
            "mem-a-internal",
            "Budget",
            "The internal budget.\n",
        );
        internal.front.classification = Label::Internal;
        let public = entry("dom-a", "mem-a-public", "Budget", "The public budget.\n");
        let other = entry(
            "dom-b",
            "mem-b-public",
            "Budget",
            "Another domain's budget.\n",
        );
        let mut gone = entry("dom-a", "mem-a-gone", "Budget", "A demoted budget.\n");
        gone.front.status = Some(Status::Demoted);
        for e in [&secret, &internal, &public, &other, &gone] {
            ix.upsert(e).unwrap();
        }

        let f = ReadFilter::for_domains(["dom-a"], Label::Internal);
        let hits = ix.search("budget", &f, 10).unwrap();
        let mut got = ids(&hits);
        got.sort_unstable();
        assert_eq!(got, vec!["mem-a-internal", "mem-a-public"]);
        assert_eq!(
            ix.count(&f).unwrap(),
            2,
            "the count sees what the search sees"
        );
        assert!(
            ix.meta("mem-a-secret", &f).unwrap().is_none(),
            "meta hides what the filter hides"
        );
        assert!(ix.meta("mem-a-public", &f).unwrap().is_some());
        assert!(ix.meta("mem-b-public", &f).unwrap().is_none(), "domain");

        let mut wide = f.clone();
        wide.include_inactive = true;
        assert_eq!(
            ix.count(&wide).unwrap(),
            3,
            "inactive entries come back only when asked for"
        );
        let mut curated = f.clone();
        curated.min_tier = TrustTier::Curated;
        assert_eq!(ix.count(&curated).unwrap(), 0, "tier floor");
        assert_eq!(ix.count(&any()).unwrap(), 4);
    }

    #[test]
    fn a_question_falls_back_to_any_word_when_no_entry_has_them_all() {
        let mut ix = Index::in_memory().unwrap();
        ix.upsert(&entry(
            "dom-a",
            "mem-a-dr19",
            "DR-19: does the wiki stay on MoinMoin",
            "MoinMoin cannot rank full text without Xapian beside it.\n",
        ))
        .unwrap();
        ix.upsert(&entry(
            "dom-a",
            "mem-a-commits",
            "Commit messages",
            "Short subject, body says why.\n",
        ))
        .unwrap();
        let hits = ix
            .search("what did DR-19 decide about MoinMoin", &any(), 10)
            .unwrap();
        assert_eq!(ids(&hits), vec!["mem-a-dr19"], "any-word fallback finds it");
        assert!(ix
            .search("nothing matches this at all", &any(), 10)
            .unwrap()
            .is_empty());
        assert!(
            ix.search("zzzz", &any(), 10).unwrap().is_empty(),
            "a single unknown word has no fallback"
        );
    }

    #[test]
    fn query_syntax_from_a_user_cannot_break_the_search() {
        let mut ix = Index::in_memory().unwrap();
        ix.upsert(&entry(
            "dom-a",
            "mem-a-1",
            "Quotes",
            "She said \"hello\" and left.\n",
        ))
        .unwrap();
        for q in [
            "\"unbalanced",
            "foo OR bar)",
            "NEAR(",
            "a*",
            "-",
            "\"\"",
            "hello\"",
        ] {
            assert!(ix.search(q, &any(), 10).is_ok(), "{q:?}");
        }
        assert_eq!(
            ids(&ix.search("\"hello\"", &any(), 10).unwrap()),
            vec!["mem-a-1"]
        );
        assert!(ix.search("   ", &any(), 10).unwrap().is_empty());
        assert_eq!(
            fts_query("a \"b\" c", false),
            Some("\"a\" \"\"\"b\"\"\" \"c\"".into())
        );
    }

    #[test]
    fn rebuild_from_the_record_replaces_everything() {
        let mut ix = Index::in_memory().unwrap();
        ix.upsert(&entry(
            "dom-a",
            "mem-a-stale",
            "Stale",
            "Gone from the record.\n",
        ))
        .unwrap();
        let fresh = vec![
            entry("dom-a", "mem-a-1", "One", "first\n"),
            entry("dom-a", "mem-a-2", "Two", "second\n"),
        ];
        assert_eq!(ix.rebuild(fresh.clone()).unwrap(), 2);
        assert_eq!(ix.count(&any()).unwrap(), 2);
        assert!(ix.search("gone", &any(), 10).unwrap().is_empty());
        let mut incremental = Index::in_memory().unwrap();
        for e in &fresh {
            incremental.upsert(e).unwrap();
        }
        assert_eq!(
            ix.search("first second", &any(), 10).unwrap(),
            incremental.search("first second", &any(), 10).unwrap(),
            "a rebuild and an incremental index answer alike"
        );
        assert_eq!(
            ids(&ix.search("second", &any(), 10).unwrap()),
            ids(&incremental.search("second", &any(), 10).unwrap())
        );
    }

    #[test]
    fn upsert_replaces_and_remove_forgets() {
        let mut ix = Index::in_memory().unwrap();
        ix.upsert(&entry("dom-a", "mem-a-1", "Old title", "alpha\n"))
            .unwrap();
        ix.upsert(&entry("dom-a", "mem-a-1", "New title", "beta\n"))
            .unwrap();
        assert!(ix.search("alpha", &any(), 10).unwrap().is_empty());
        assert_eq!(ix.search("beta", &any(), 10).unwrap()[0].title, "New title");
        assert_eq!(ix.count(&any()).unwrap(), 1);
        assert!(ix.remove("mem-a-1").unwrap());
        assert!(!ix.remove("mem-a-1").unwrap());
        assert_eq!(ix.count(&any()).unwrap(), 0);
        assert!(ix.search("beta", &any(), 10).unwrap().is_empty());
    }

    #[test]
    fn keyed_lookups_list_and_due_for_review() {
        let mut ix = Index::in_memory().unwrap();
        let mut h = entry(
            "dom-a",
            "mem-a-handoff",
            "Stopped at step 3",
            "Resume there.\n",
        );
        h.front.entry_type = EntryType::Handoff;
        h.front.created = datetime!(2026-10-02 09:00 UTC);
        let mut fact = entry("dom-a", "mem-a-fact", "Roster", "Three members.\n");
        fact.front.review_by = Some(date!(2026 - 11 - 01));
        let mut old_fact = entry("dom-a", "mem-a-old", "Versions", "Two releases.\n");
        old_fact.front.review_by = Some(date!(2026 - 09 - 01));
        for e in [&h, &fact, &old_fact] {
            ix.upsert(e).unwrap();
        }
        assert_eq!(
            ids(&ix.list(&any(), Some(EntryType::Handoff), 10).unwrap()),
            vec!["mem-a-handoff"]
        );
        let all = ix.list(&any(), None, 10).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].id, "mem-a-handoff", "newest first");
        assert_eq!(
            ids(&ix
                .due_for_review(&any(), date!(2026 - 10 - 01), 10)
                .unwrap()),
            vec!["mem-a-old"]
        );
        assert_eq!(
            ids(&ix
                .due_for_review(&any(), date!(2026 - 12 - 01), 10)
                .unwrap()),
            vec!["mem-a-old", "mem-a-fact"]
        );
    }

    // as RFC 3339 text, each of these sorts out of time order against another: a shorter
    // fraction, no fraction, and an offset other than UTC
    const WITHIN_A_SECOND: [(&str, OffsetDateTime); 4] = [
        ("mem-a-whole", datetime!(2026-10-01 10:00:00 UTC)),
        ("mem-a-tenth", datetime!(2026-10-01 10:00:00.1 UTC)),
        ("mem-a-tenth-and", datetime!(2026-10-01 10:00:00.15 UTC)),
        ("mem-a-paris", datetime!(2026-10-01 10:59:59 +01:00)),
    ];
    const IN_TIME_ORDER: [&str; 4] = [
        "mem-a-tenth-and",
        "mem-a-tenth",
        "mem-a-whole",
        "mem-a-paris",
    ];

    fn holding(at: &[(&str, OffsetDateTime)]) -> Index {
        let mut ix = Index::in_memory().unwrap();
        for (id, created) in at {
            let mut e = entry("dom-a", id, id, "Body.\n");
            e.front.created = *created;
            ix.upsert(&e).unwrap();
        }
        ix
    }

    #[test]
    fn list_is_newest_first_within_a_second_and_across_offsets() {
        let ix = holding(&WITHIN_A_SECOND);
        assert_eq!(ids(&ix.list(&any(), None, 10).unwrap()), IN_TIME_ORDER);
    }

    #[test]
    fn a_created_at_either_end_of_the_years_is_indexed_ordered_and_read_back() {
        // each is a year the frontmatter accepts whose UTC instant is a year outside it
        // and the second sorts after the first as text, though it is earlier
        let ends = [
            ("mem-a-late", datetime!(9999-12-31 23:30:00 -01:00)),
            ("mem-a-late-paris", datetime!(9999-12-31 23:45:00 +01:00)),
            ("mem-a-early", datetime!(0000-01-01 00:30:00 +01:00)),
            ("mem-a-now", datetime!(2026-10-01 10:00:00 UTC)),
        ];
        let ix = holding(&ends);
        assert_eq!(
            ids(&ix.list(&any(), None, 10).unwrap()),
            ["mem-a-late", "mem-a-late-paris", "mem-a-now", "mem-a-early"]
        );
        for (id, created) in ends {
            let m = ix.meta(id, &any()).unwrap().unwrap();
            assert_eq!(m.created, created, "{id}");
        }
    }

    #[test]
    fn due_for_review_and_replaced_by_order_by_the_instant() {
        let mut ix = Index::in_memory().unwrap();
        for (id, created) in WITHIN_A_SECOND {
            let mut e = entry("dom-a", id, id, "Body.\n");
            e.front.created = created;
            e.front.review_by = Some(date!(2026 - 10 - 01));
            e.front.supersedes = Some("mem-a-first".into());
            ix.upsert(&e).unwrap();
        }
        let mut oldest_first = IN_TIME_ORDER;
        oldest_first.reverse();
        assert_eq!(
            ids(&ix
                .due_for_review(&any(), date!(2026 - 10 - 01), 10)
                .unwrap()),
            oldest_first
        );
        assert_eq!(
            ix.replaced_by("mem-a-first", &any()).unwrap().as_deref(),
            Some(IN_TIME_ORDER[0])
        );
    }

    /// A row as a binary from before the sort key writes it: the columns it names.
    fn insert_as_before(conn: &Connection, id: &str, created: OffsetDateTime) {
        conn.execute(
            "INSERT OR REPLACE INTO entries
             (id, domain, entry_type, author, created, review_by, tier_rank, label_rank,
              status, supersedes, origin, title, summary, digest)
             VALUES (?1, 'dom-a', 'fact', 'human:test:me', ?2, NULL, 0, 0,
              NULL, NULL, NULL, ?1, '', '')",
            params![id, created.format(&Rfc3339).unwrap()],
        )
        .unwrap();
    }

    #[test]
    fn an_index_file_from_before_the_sort_key_is_ordered_on_open_and_after() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        {
            let conn = Connection::open(&path).unwrap();
            let before = SCHEMA.replace(
                "    created_unix   INTEGER,\n    created_nanos  INTEGER,\n",
                "",
            );
            assert_ne!(before, SCHEMA);
            conn.execute_batch(&before).unwrap();
            for (id, created) in WITHIN_A_SECOND {
                insert_as_before(&conn, id, created);
            }
        }
        let ix = Index::open(&path).unwrap();
        assert_eq!(ids(&ix.list(&any(), None, 10).unwrap()), IN_TIME_ORDER);
        assert!(ix.sort_keys_done().unwrap());
        // an older binary goes on writing to the file after a newer one opened it
        insert_as_before(
            &ix.conn,
            "mem-a-later",
            datetime!(2026-10-01 10:00:00.2 UTC),
        );
        assert_eq!(
            ids(&ix.list(&any(), None, 10).unwrap())[0],
            "mem-a-later",
            "an older binary's row lands without the order"
        );
    }

    #[test]
    fn meta_round_trips_and_origin_dedups() {
        let mut ix = Index::in_memory().unwrap();
        let mut e = entry("dom-a", "mem-a-1", "Imported", "From elsewhere.\n");
        e.front.origin = Some("github:apache/x#12".into());
        e.front.review_by = Some(date!(2027 - 01 - 01));
        e.front.supersedes = Some("mem-a-0".into());
        e.front.status = Some(Status::Superseded);
        ix.upsert(&e).unwrap();
        let mut f = any();
        f.include_inactive = true;
        let m = ix.meta("mem-a-1", &f).unwrap().unwrap();
        assert_eq!(m.author, "human:test:me");
        assert_eq!(m.created, datetime!(2026-10-01 10:00 UTC));
        assert_eq!(m.review_by, Some(date!(2027 - 01 - 01)));
        assert_eq!(m.status, Some(Status::Superseded));
        assert_eq!(m.supersedes.as_deref(), Some("mem-a-0"));
        assert_eq!(m.origin.as_deref(), Some("github:apache/x#12"));
        assert_eq!(m.digest, dm_core::digest(&e.render()));
        assert_eq!(m.hit.title, "Imported");
        assert_eq!(
            ix.by_origin("dom-a", "github:apache/x#12")
                .unwrap()
                .as_deref(),
            Some("mem-a-1")
        );
        assert_eq!(ix.by_origin("dom-a", "github:apache/x#13").unwrap(), None);
        assert_eq!(
            ix.by_origin("dom-b", "github:apache/x#12").unwrap(),
            None,
            "another domain's entry is not this domain's duplicate"
        );
        assert!(ix.meta("mem-a-none", &f).unwrap().is_none());
    }

    #[test]
    fn the_file_persists_across_opens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        {
            let mut ix = Index::open(&path).unwrap();
            ix.upsert(&entry("dom-a", "mem-a-1", "Persisted", "still here\n"))
                .unwrap();
        }
        let ix = Index::open(&path).unwrap();
        assert_eq!(
            ids(&ix.search("still", &any(), 10).unwrap()),
            vec!["mem-a-1"]
        );
    }

    /// Bytes anywhere in the index's files: the database and whatever journal it keeps.
    fn index_bytes(dir: &std::path::Path) -> Vec<u8> {
        let mut all = Vec::new();
        for f in std::fs::read_dir(dir).unwrap().flatten() {
            all.extend(std::fs::read(f.path()).unwrap());
        }
        all
    }

    #[test]
    fn a_removed_body_leaves_no_bytes_behind_once_vacuumed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let marker = b"passphrase-Qz7xx";
        {
            let mut ix = Index::open(&path).unwrap();
            let body = format!(
                "The body says {m} twice: {m}.\n",
                m = String::from_utf8_lossy(marker)
            );
            ix.upsert(&entry("dom-a", "mem-a-1", "A title", &body))
                .unwrap();
            assert!(ix.remove("mem-a-1").unwrap());
            ix.vacuum().unwrap();
        }
        let bytes = index_bytes(dir.path());
        assert!(
            !bytes.windows(marker.len()).any(|w| w == marker),
            "the removed body is still in the index's files"
        );
    }

    #[test]
    fn vacuum_says_so_when_another_connection_keeps_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        let marker = b"passphrase-Qz7xx";
        let mut ix = Index::open(&path).unwrap();
        let body = format!("The body says {m}.\n", m = String::from_utf8_lossy(marker));
        ix.upsert(&entry("dom-a", "mem-a-1", "A title", &body))
            .unwrap();
        ix.upsert(&entry("dom-a", "mem-a-2", "Other", "kept\n"))
            .unwrap();
        // another process, mid-query, holds a read snapshot of the log
        let reader = Connection::open(&path).unwrap();
        reader
            .busy_timeout(std::time::Duration::from_millis(100))
            .unwrap();
        let mut stmt = reader.prepare("SELECT id FROM entries").unwrap();
        let mut rows = stmt.query([]).unwrap();
        let _first = rows.next().unwrap();
        assert!(ix.remove("mem-a-1").unwrap());
        assert!(
            matches!(ix.vacuum(), Err(Error::Held)),
            "a vacuum that could not checkpoint the log says so"
        );
        drop(rows);
        drop(stmt);
        ix.vacuum().unwrap();
        drop(reader);
        drop(ix);
        let bytes = index_bytes(dir.path());
        assert!(!bytes.windows(marker.len()).any(|w| w == marker));
    }
}
