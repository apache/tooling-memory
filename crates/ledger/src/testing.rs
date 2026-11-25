//! A PostgreSQL server for tests, so the ledger's rules can be shown holding on both
//! backends. Behind the `pg-test` feature, for test code only.
//!
//! `MEMORY_PG_URL` names a server to use, and each test gets a database of its own on
//! it, dropped afterwards. Otherwise an embedded server is started for the test, which
//! the `postgresql_embedded` crate downloads once into `~/.theseus` and stops on drop;
//! where that cannot be done, the test is skipped with the reason on stderr. Under CI it
//! is an error instead: a green run there says the PostgreSQL half ran.

use postgresql_embedded::{PostgreSQL, Settings, VersionReq, BOOTSTRAP_DATABASE};
use tokio::sync::OnceCell;
use tokio_postgres::NoTls;

/// The release the embedded server is, held exactly rather than the newest, so a new
/// archive cannot change what the tests run on. Not 18.6.0, whose Linux build loads
/// `libxml2.so.2` from the system, and Ubuntu 26.04 has none; 18.6.1 carries its libraries
/// beside its binaries. 18.6.1's Linux and macOS archives
/// store zone aliases, `UTC` among them, as tar hard links, which the extractor of
/// `postgresql_archive` 0.20 skipped (theseus-rs/postgresql-embedded#263). An install that
/// 0.20 made stays in the cache without them: see [`create_database`].
const EMBEDDED_RELEASE: &str = "=18.6.1";

/// The release installed, once in a process. Installing lists every release through
/// GitHub's API; a setup that finds the release installed asks nothing. Concurrent tests
/// would otherwise each list them, and be rate-limited into skips.
static INSTALLED: OnceCell<()> = OnceCell::const_new();

/// The settings every embedded server here starts from.
fn settings() -> Result<Settings, String> {
    Ok(Settings {
        version: VersionReq::parse(EMBEDDED_RELEASE).map_err(|e| e.to_string())?,
        ..Settings::default()
    })
}

/// A server that cannot be had: a skip with the reason on stderr, or under CI, where a
/// skip would pass unseen, an error.
fn unavailable(why: String) -> Result<Option<PgServer>, String> {
    if std::env::var_os("CI").is_some() {
        return Err(why);
    }
    eprintln!("skipped: {why}");
    Ok(None)
}

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
    /// database. An embedded server that cannot be set up is a skip, not an error, but
    /// under CI (`CI` set) it is an error.
    pub async fn start() -> Result<Option<PgServer>, String> {
        if let Ok(admin) = std::env::var("MEMORY_PG_URL") {
            return PgServer::borrow(&admin).await.map(Some);
        }
        // a throwaway server installs the release; its temporary data directory goes with it
        let installed = INSTALLED
            .get_or_try_init(|| async {
                let mut first = PostgreSQL::new(settings()?);
                first.setup().await.map_err(|e| e.to_string())
            })
            .await;
        if let Err(e) = installed {
            return unavailable(format!("embedded PostgreSQL cannot be installed here: {e}"));
        }
        let pg = match started(0).await {
            Ok(pg) => pg,
            Err(why) => return unavailable(why),
        };
        create_database(&pg.settings().url(BOOTSTRAP_DATABASE), "ledger")
            .await
            .map_err(|e| format!("embedded PostgreSQL will not create a database: {e}"))?;
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

/// An embedded server of the installed release, started on `port`, or on a free port
/// for `0`; else the reason that it cannot be had.
async fn started(port: u16) -> Result<PostgreSQL, String> {
    let mut pg = PostgreSQL::new(Settings {
        port,
        ..settings()?
    });
    if let Err(e) = pg.setup().await {
        return Err(format!("embedded PostgreSQL cannot be set up here: {e}"));
    }
    let asked = std::time::Instant::now();
    if let Err(e) = pg.start().await {
        let took = asked.elapsed();
        return Err(format!(
            "embedded PostgreSQL did not start: {e}{}",
            start_evidence(&pg, took)
        ));
    }
    Ok(pg)
}

/// What a failed start leaves behind, for the error to carry: how long `pg_ctl` ran, the
/// port, and the server's own log. `pg_ctl` sends the server's output to `start.log` in
/// the data directory, which goes when the server is dropped. On Windows
/// `postgresql_commands` 0.22 does not read what `pg_ctl` itself prints, so its error
/// there is `Command error: stdout=; stderr=` whatever the cause.
fn start_evidence(pg: &PostgreSQL, took: std::time::Duration) -> String {
    let data = &pg.settings().data_dir;
    let log = match std::fs::read_to_string(data.join("start.log")) {
        Ok(text) if text.trim().is_empty() => "start.log is empty".to_string(),
        Ok(text) => format!("start.log:\n{}", text.trim_end()),
        Err(e) => format!("start.log cannot be read: {e}"),
    };
    let pid_file = if data.join("postmaster.pid").exists() {
        "present"
    } else {
        "absent"
    };
    format!(
        "\npg_ctl ran for {took:?} on port {}; postmaster.pid is {pid_file}; {log}",
        pg.settings().port
    )
}

/// Makes database `name` on the server `admin` names, with tokio-postgres, which sets no
/// zone on connect. `PostgreSQL::create_database` connects with a client that sets `UTC`,
/// which an 18.6.1 that `postgresql_archive` 0.20 installed is without, and is refused.
async fn create_database(admin: &str, name: &str) -> Result<(), tokio_postgres::Error> {
    let (client, conn) = tokio_postgres::connect(admin, NoTls).await?;
    tokio::spawn(conn);
    client
        .batch_execute(&format!("CREATE DATABASE {name}"))
        .await
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
    use super::{started, unavailable, with_db, PgServer};
    use std::net::TcpListener;

    /// The error of a start that fails says why, in the server's own words. The port is
    /// held here on each loopback address, so the server finds no address to listen on.
    #[tokio::test]
    async fn a_start_that_fails_gives_the_log_of_the_server() {
        // the release is installed by the first start, and where it cannot be, this skips
        let Some(_installed) = PgServer::start().await.unwrap() else {
            return;
        };
        let v4 = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = v4.local_addr().unwrap().port();
        // a host with no IPv6 loopback has no second address for the server either
        let _v6 = TcpListener::bind(("::1", port));
        let why = match started(port).await {
            Ok(_) => panic!("a server started on port {port}, which this test holds"),
            Err(why) if why.contains("did not start") => why,
            // the setup failed before the start: as each test here takes it
            Err(why) => {
                assert!(unavailable(why).unwrap().is_none());
                return;
            }
        };
        assert!(why.contains("start.log:"), "{why}");
        assert!(why.contains("could not bind"), "the server's reason: {why}");
        assert!(why.contains(&format!("on port {port}")), "{why}");
    }

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
