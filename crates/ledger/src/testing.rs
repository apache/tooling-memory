//! A PostgreSQL server for tests, so the ledger's rules can be shown holding on both
//! backends. Behind the `pg-test` feature, for test code only.
//!
//! `MEMORY_PG_URL` names a server to use, and each test gets a database of its own on
//! it, dropped afterwards. Otherwise an embedded server is started for the test, which
//! the `postgresql_embedded` crate downloads once into `~/.theseus` and stops on drop;
//! where that cannot be done, the test is skipped with the reason on stderr.

use postgresql_embedded::{PostgreSQL, Settings};
use tokio_postgres::NoTls;

/// A server for one test, with a database of its own.
pub struct PgServer {
    embedded: Option<PostgreSQL>,
    /// The admin URL and the database to drop, when the server is not ours.
    borrowed: Option<(String, String)>,
    url: String,
}

impl PgServer {
    /// A server to test against: `Ok(None)`, with the reason printed, when none can be
    /// had here.
    ///
    /// # Errors
    ///
    /// When `MEMORY_PG_URL` names a server that cannot be reached or will not create a
    /// database: a server someone pointed us at and we could not use is worth hearing
    /// about. An embedded server is the opposite -- one that cannot be set up, will not
    /// start, or will not create a database is a skip, since nothing asked for it by
    /// name.
    pub async fn start() -> Result<Option<PgServer>, String> {
        if let Ok(admin) = std::env::var("MEMORY_PG_URL") {
            return PgServer::borrow(&admin).await.map(Some);
        }
        let mut pg = PostgreSQL::new(Settings::default());
        if let Err(e) = pg.setup().await {
            eprintln!("skipped: embedded PostgreSQL cannot be set up here: {e}");
            return Ok(None);
        }
        if let Err(e) = pg.start().await {
            eprintln!("skipped: embedded PostgreSQL did not start: {e}");
            return Ok(None);
        }
        // A skip like the two above it: a server we started that will not give us a
        // database is a server that cannot be had here, whichever step it refused at.
        // The PostgreSQL 18.6.1 archive postgresql_embedded downloads ships a trimmed
        // share/timezone with no `UTC`, and create_database is the first thing to ask
        // for it, so this is where an unusable embedded server shows up.
        if let Err(e) = pg.create_database("ledger").await {
            eprintln!("skipped: embedded PostgreSQL will not create a database: {e}");
            return Ok(None);
        }
        let url = pg.settings().url("ledger");
        Ok(Some(PgServer {
            embedded: Some(pg),
            borrowed: None,
            url,
        }))
    }

    /// The URL a ledger connects to.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    async fn borrow(admin: &str) -> Result<PgServer, String> {
        let name = fresh_name();
        let (client, conn) = tokio_postgres::connect(admin, NoTls)
            .await
            .map_err(|e| format!("MEMORY_PG_URL does not connect: {e}"))?;
        tokio::spawn(conn);
        client
            .batch_execute(&format!("CREATE DATABASE {name}"))
            .await
            .map_err(|e| format!("MEMORY_PG_URL will not create a database: {e}"))?;
        Ok(PgServer {
            embedded: None,
            borrowed: Some((admin.to_string(), name.clone())),
            url: with_db(admin, &name),
        })
    }
}

fn fresh_name() -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("dm_ledger_test_{}_{n}", std::process::id())
}

/// `admin` with its database replaced by `db`.
fn with_db(admin: &str, db: &str) -> String {
    let (base, query) = admin.split_once('?').map_or((admin, ""), |(b, q)| (b, q));
    let scheme_end = base.find("://").map_or(0, |i| i + 3);
    let rest = &base[scheme_end..];
    let authority = rest.split_once('/').map_or(rest, |(a, _)| a);
    let mut url = format!("{}{authority}/{db}", &base[..scheme_end]);
    if !query.is_empty() {
        url.push('?');
        url.push_str(query);
    }
    url
}

impl Drop for PgServer {
    fn drop(&mut self) {
        // the embedded server stops and removes its data directory on drop
        drop(self.embedded.take());
        let Some((admin, name)) = self.borrowed.take() else {
            return;
        };
        // a thread of its own, since this may run inside a runtime; best effort, since
        // a database left behind is a nuisance and not a failure
        let _ = std::thread::spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            rt.block_on(async {
                let Ok((client, conn)) = tokio_postgres::connect(&admin, NoTls).await else {
                    return;
                };
                tokio::spawn(conn);
                let _ = client
                    .batch_execute(&format!("DROP DATABASE {name} WITH (FORCE)"))
                    .await;
            });
        })
        .join();
    }
}

#[cfg(test)]
mod tests {
    use super::with_db;

    #[test]
    fn the_database_is_swapped_and_the_rest_kept() {
        assert_eq!(
            with_db("postgres://u:p@h:5432/postgres", "t1"),
            "postgres://u:p@h:5432/t1"
        );
        assert_eq!(
            with_db("postgres://u:p@h/postgres?sslmode=disable", "t1"),
            "postgres://u:p@h/t1?sslmode=disable"
        );
        assert_eq!(with_db("postgres://h", "t1"), "postgres://h/t1");
    }
}
