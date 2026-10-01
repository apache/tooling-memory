//! Where token usage goes: one SQLite table in the state directory. Raw quantities per
//! destination and model, as the paper's §12 asks; a price vector turns them into cost
//! elsewhere, and under a subscription the price is notional.

use crate::usage::Usage;
use rusqlite::{params, Connection};
use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

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
        Ok(Telemetry {
            conn: Mutex::new(conn),
        })
    }

    /// Record one call.
    ///
    /// # Errors
    ///
    /// A database error.
    pub fn record(
        &self,
        session: &str,
        destination: &str,
        status: u16,
        usage: &Usage,
    ) -> Result<(), Error> {
        let at = OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .unwrap_or_default();
        lock(&self.conn).execute(
            "INSERT INTO usage (at, session, destination, model, status, input_tokens, output_tokens, cache_read, cache_write)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
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
            ],
        )?;
        Ok(())
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
                    SUM(cache_read), SUM(cache_write)
             FROM usage GROUP BY destination, model
             ORDER BY SUM(input_tokens) + SUM(output_tokens) DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            let u = |i: usize| -> rusqlite::Result<u64> {
                Ok(u64::try_from(r.get::<_, i64>(i)?).unwrap_or(0))
            };
            Ok(Totals {
                destination: r.get(0)?,
                model: r.get(1)?,
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
        t.record("s1", "anthropic", 200, &u).unwrap();
        t.record("s1", "anthropic", 200, &u).unwrap();
        t.record(
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
    }
}
