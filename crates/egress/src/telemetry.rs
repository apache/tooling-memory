//! Where token usage goes: one SQLite table in the state directory. Raw quantities per
//! destination and model, as the paper's §12 asks; a price vector turns them into cost
//! elsewhere, and under a subscription the price is notional.

use crate::usage::Usage;
use rusqlite::{params, Connection};
use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::SystemTime;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// One session's model calls, summed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionTotals {
    /// The memory session id.
    pub session: String,
    /// Whether any of it was reported by the harness rather than observed by the egress.
    pub reported: bool,
    /// How many calls.
    pub calls: u64,
    /// Summed input tokens, cache reads excluded.
    pub input_tokens: u64,
    /// Summed output tokens.
    pub output_tokens: u64,
    /// Summed cache reads.
    pub cache_read_tokens: u64,
    /// Summed cache writes.
    pub cache_write_tokens: u64,
    /// The first call, RFC 3339.
    pub started: String,
}

/// What can go wrong recording usage.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// SQLite.
    #[error("telemetry: {0}")]
    Db(#[from] rusqlite::Error),
}

/// One row of the summary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Totals {
    /// The destination the calls went to.
    pub destination: String,
    /// The model the provider reported, or "?" when it did not.
    pub model: String,
    /// Who counted: [`SOURCE_EGRESS`] for calls the proxy saw, [`SOURCE_HARNESS`]
    /// for what the harness reported about a tunnelled login.
    pub source: String,
    /// How many calls.
    pub calls: u64,
    /// Summed input tokens.
    pub input_tokens: u64,
    /// Summed output tokens.
    pub output_tokens: u64,
    /// Summed cache reads.
    pub cache_read_tokens: u64,
    /// Summed cache writes.
    pub cache_write_tokens: u64,
}

/// The usage log.
pub struct Telemetry {
    conn: Mutex<Connection>,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS usage (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    at            TEXT NOT NULL,
    session       TEXT NOT NULL,
    destination   TEXT NOT NULL,
    model         TEXT,
    status        INTEGER NOT NULL,
    input_tokens  INTEGER NOT NULL,
    output_tokens INTEGER NOT NULL,
    cache_read    INTEGER NOT NULL,
    cache_write   INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS usage_dest ON usage (destination, model);
";

/// Where a usage row came from.
pub const SOURCE_EGRESS: &str = "egress";
/// A row the harness reported about itself, read from its transcript after the run.
/// Rows of this source carry status 0: there was no response to take a status from.
pub const SOURCE_HARNESS: &str = "harness";

/// Older databases have no `source` column; give them one, and call every row they
/// hold egress-observed, which is all they could have been.
fn migrate(conn: &Connection) -> Result<(), rusqlite::Error> {
    let has_source = conn
        .prepare("SELECT 1 FROM pragma_table_info('usage') WHERE name = 'source'")?
        .exists([])?;
    if !has_source {
        conn.execute_batch("ALTER TABLE usage ADD COLUMN source TEXT NOT NULL DEFAULT 'egress';")?;
    }
    Ok(())
}

fn lock(m: &Mutex<Connection>) -> MutexGuard<'_, Connection> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Telemetry {
    /// Open or create the log.
    ///
    /// # Errors
    ///
    /// When the file cannot be opened or the schema created.
    pub fn open(path: &Path) -> Result<Telemetry, Error> {
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        Ok(Telemetry {
            conn: Mutex::new(conn),
        })
    }

    /// An in-memory log, for tests.
    ///
    /// # Errors
    ///
    /// Never in practice.
    pub fn in_memory() -> Result<Telemetry, Error> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        Ok(Telemetry {
            conn: Mutex::new(conn),
        })
    }

    /// Record one call that started at `started`.
    ///
    /// # Errors
    ///
    /// A database error.
    pub fn record(
        &self,
        started: SystemTime,
        session: &str,
        destination: &str,
        status: u16,
        usage: &Usage,
    ) -> Result<(), Error> {
        self.insert(
            Some(started),
            session,
            destination,
            status,
            usage,
            SOURCE_EGRESS,
        )
    }

    /// Record what a harness reported about itself: usage read from its transcript
    /// after a run whose calls went through a tunnel the egress could not meter.
    ///
    /// # Errors
    ///
    /// A database error.
    pub fn record_reported(
        &self,
        session: &str,
        destination: &str,
        usage: &Usage,
    ) -> Result<(), Error> {
        self.insert(None, session, destination, 0, usage, SOURCE_HARNESS)
    }

    fn insert(
        &self,
        started: Option<SystemTime>,
        session: &str,
        destination: &str,
        status: u16,
        usage: &Usage,
        source: &str,
    ) -> Result<(), Error> {
        let at = OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .unwrap_or_default();
        lock(&self.conn).execute(
            "INSERT INTO usage (at, session, destination, model, status, input_tokens, output_tokens, cache_read, cache_write, source)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                at,
                session,
                destination,
                usage.model,
                i64::from(status),
                i64::try_from(usage.input_tokens).unwrap_or(i64::MAX),
                i64::try_from(usage.output_tokens).unwrap_or(i64::MAX),
                i64::try_from(usage.cache_read_tokens).unwrap_or(i64::MAX),
                i64::try_from(usage.cache_write_tokens).unwrap_or(i64::MAX),
                source,
            ],
        )?;
        trace(started, session, destination, status, usage, source);
        Ok(())
    }

    /// Totals by session: what each session's model calls cost, all destinations.
    ///
    /// # Errors
    ///
    /// A database error.
    pub fn by_session(&self) -> Result<Vec<SessionTotals>, Error> {
        let conn = lock(&self.conn);
        let mut stmt = conn.prepare(
            "SELECT session, COUNT(*), SUM(input_tokens), SUM(output_tokens), SUM(cache_read),
                    SUM(cache_write), MIN(at), MAX(source = 'harness')
             FROM usage WHERE COALESCE(model, '') NOT LIKE 'http %' AND COALESCE(model, '') != 'tunnel'
             GROUP BY session ORDER BY MIN(at)",
        )?;
        let rows = stmt.query_map([], |r| {
            let u = |i: usize| -> rusqlite::Result<u64> {
                Ok(u64::try_from(r.get::<_, i64>(i)?).unwrap_or(0))
            };
            Ok(SessionTotals {
                session: r.get(0)?,
                calls: u(1)?,
                input_tokens: u(2)?,
                output_tokens: u(3)?,
                cache_read_tokens: u(4)?,
                cache_write_tokens: u(5)?,
                started: r.get(6)?,
                reported: r.get::<_, i64>(7)? != 0,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// One session's totals, asked of that session's rows only; `None` before its first
    /// call.
    ///
    /// # Errors
    ///
    /// A database error.
    pub fn session(&self, session: &str) -> Result<Option<SessionTotals>, Error> {
        let conn = lock(&self.conn);
        let mut stmt = conn.prepare(
            "SELECT COUNT(*), SUM(input_tokens), SUM(output_tokens), SUM(cache_read),
                    SUM(cache_write), MIN(at), MAX(source = 'harness')
             FROM usage WHERE session = ?1 AND COALESCE(model, '') NOT LIKE 'http %'
               AND COALESCE(model, '') != 'tunnel'",
        )?;
        let totals = stmt.query_row([session], |r| {
            let u = |i: usize| -> rusqlite::Result<u64> {
                Ok(u64::try_from(r.get::<_, Option<i64>>(i)?.unwrap_or(0)).unwrap_or(0))
            };
            Ok(SessionTotals {
                session: session.to_string(),
                calls: u(0)?,
                input_tokens: u(1)?,
                output_tokens: u(2)?,
                cache_read_tokens: u(3)?,
                cache_write_tokens: u(4)?,
                started: r.get::<_, Option<String>>(5)?.unwrap_or_default(),
                reported: r.get::<_, Option<i64>>(6)?.unwrap_or(0) != 0,
            })
        })?;
        Ok((totals.calls > 0).then_some(totals))
    }

    /// Tokens in and out spent at a destination: by one session, or since a time, or
    /// both. What the spend limits are checked against.
    ///
    /// # Errors
    ///
    /// A database error.
    pub fn spent(
        &self,
        destination: &str,
        session: Option<&str>,
        since: Option<OffsetDateTime>,
    ) -> Result<u64, Error> {
        let since = since.map(|t| t.format(&Rfc3339).unwrap_or_default());
        let n: i64 = lock(&self.conn).query_row(
            "SELECT COALESCE(SUM(input_tokens + output_tokens), 0) FROM usage
             WHERE destination = ?1 AND (?2 IS NULL OR session = ?2) AND (?3 IS NULL OR at >= ?3)",
            params![destination, session, since],
            |r| r.get(0),
        )?;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    /// Totals by destination and model, most tokens first.
    ///
    /// # Errors
    ///
    /// A database error.
    pub fn summary(&self) -> Result<Vec<Totals>, Error> {
        let conn = lock(&self.conn);
        let mut stmt = conn.prepare(
            "SELECT destination, COALESCE(model, '?'), COUNT(*), SUM(input_tokens), SUM(output_tokens),
                    SUM(cache_read), SUM(cache_write), source
             FROM usage GROUP BY destination, model, source
             ORDER BY SUM(input_tokens) + SUM(output_tokens) DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            let u = |i: usize| -> rusqlite::Result<u64> {
                Ok(u64::try_from(r.get::<_, i64>(i)?).unwrap_or(0))
            };
            Ok(Totals {
                destination: r.get(0)?,
                model: r.get(1)?,
                source: r.get(7)?,
                calls: u(2)?,
                input_tokens: u(3)?,
                output_tokens: u(4)?,
                cache_read_tokens: u(5)?,
                cache_write_tokens: u(6)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}

/// The row as a span for a collector: from the request's start when there was a request,
/// an instant for a count the harness reported.
fn trace(
    started: Option<SystemTime>,
    session: &str,
    destination: &str,
    status: u16,
    usage: &Usage,
    source: &str,
) {
    if !dm_otel::enabled() {
        return;
    }
    let mut attrs: Vec<(&str, dm_otel::Value<'_>)> = vec![
        ("memory.session", session.into()),
        ("egress.destination", destination.into()),
        ("gen_ai.usage.input_tokens", usage.input_tokens.into()),
        ("gen_ai.usage.output_tokens", usage.output_tokens.into()),
        ("memory.cache_read_tokens", usage.cache_read_tokens.into()),
        ("memory.cache_write_tokens", usage.cache_write_tokens.into()),
        ("memory.source", source.into()),
    ];
    if let Some(model) = &usage.model {
        attrs.push(("gen_ai.request.model", model.as_str().into()));
    }
    if status != 0 {
        attrs.push(("http.response.status_code", i64::from(status).into()));
    }
    match started {
        Some(at) => dm_otel::span_since(at, "model.call", &attrs).end(),
        None => dm_otel::event("model.call", &attrs),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_sums() {
        let t = Telemetry::in_memory().unwrap();
        let u = Usage {
            model: Some("m".into()),
            input_tokens: 10,
            output_tokens: 5,
            cache_read_tokens: 2,
            cache_write_tokens: 0,
        };
        t.record(SystemTime::now(), "s1", "anthropic", 200, &u)
            .unwrap();
        t.record(SystemTime::now(), "s1", "anthropic", 200, &u)
            .unwrap();
        t.record(
            SystemTime::now(),
            "s2",
            "openai",
            200,
            &Usage {
                model: None,
                input_tokens: 1,
                ..Usage::default()
            },
        )
        .unwrap();
        let s = t.summary().unwrap();
        assert_eq!(s[0].destination, "anthropic");
        assert_eq!(
            (
                s[0].calls,
                s[0].input_tokens,
                s[0].output_tokens,
                s[0].cache_read_tokens
            ),
            (2, 20, 10, 4)
        );
        assert_eq!(s[1].model, "?");
        let one = t.session("s1").unwrap().unwrap();
        assert_eq!(
            (one.calls, one.input_tokens, one.output_tokens),
            (2, 20, 10)
        );
        assert_eq!(t.session("s2").unwrap().unwrap().calls, 1);
        assert_eq!(t.session("s3").unwrap(), None, "no calls yet");
        let all = t.by_session().unwrap();
        assert_eq!(all.iter().find(|x| x.session == "s1"), Some(&one));
    }

    #[test]
    fn calls_are_traced_beside_their_rows() {
        use opentelemetry::Value;
        use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
        use std::time::{Duration, SystemTime};
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let _guard = dm_otel::install(provider);
        let t = Telemetry::in_memory().unwrap();
        let u = Usage {
            model: Some("claude-x".into()),
            input_tokens: 10,
            output_tokens: 5,
            cache_read_tokens: 2,
            cache_write_tokens: 1,
        };
        let started = SystemTime::now() - Duration::from_millis(20);
        t.record(started, "sess-traced", "anthropic", 200, &u)
            .unwrap();
        t.record_reported("sess-traced", "anthropic", &u).unwrap();
        let spans: Vec<_> = exporter
            .get_finished_spans()
            .unwrap()
            .into_iter()
            .filter(|s| {
                s.attributes.iter().any(|kv| {
                    kv.key.as_str() == "memory.session" && kv.value.as_str() == "sess-traced"
                })
            })
            .collect();
        assert_eq!(spans.len(), 2, "one span per row");
        let attr = |i: usize, k: &str| {
            spans[i]
                .attributes
                .iter()
                .find(|kv| kv.key.as_str() == k)
                .map(|kv| kv.value.clone())
        };
        assert!(spans.iter().all(|s| s.name == "model.call"));
        assert_eq!(
            spans[0].start_time, started,
            "the span starts with the request"
        );
        assert_eq!(
            attr(0, "egress.destination"),
            Some(Value::from("anthropic"))
        );
        assert_eq!(
            attr(0, "gen_ai.request.model"),
            Some(Value::from("claude-x"))
        );
        assert_eq!(attr(0, "http.response.status_code"), Some(Value::I64(200)));
        assert_eq!(attr(0, "gen_ai.usage.input_tokens"), Some(Value::I64(10)));
        assert_eq!(attr(0, "gen_ai.usage.output_tokens"), Some(Value::I64(5)));
        assert_eq!(attr(0, "memory.cache_read_tokens"), Some(Value::I64(2)));
        assert_eq!(attr(0, "memory.cache_write_tokens"), Some(Value::I64(1)));
        assert_eq!(attr(0, "memory.source"), Some(Value::from("egress")));
        assert_eq!(attr(1, "memory.source"), Some(Value::from("harness")));
        assert_eq!(
            attr(1, "http.response.status_code"),
            None,
            "the harness's count has no response"
        );
    }
}
