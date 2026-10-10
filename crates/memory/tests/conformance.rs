//! `scripts/conformance.py` against this build: the seven M0 steps over the surface the
//! binary serves, on both transports. The script is what another serving layer is held
//! to, so the laptop server passes it first; `scripts/conformance.test.sh` holds that the
//! script's assertions bite, against a fake with one defect at a time.

// An integration test is test code, but clippy's allow-unwrap-in-tests only sees
// #[cfg(test)] modules; here a panic is the failure being reported.
#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

fn python() -> String {
    for candidate in ["python3", "python"] {
        if Command::new(candidate)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
        {
            return candidate.to_string();
        }
    }
    panic!("python3 is needed to run scripts/conformance.py");
}

fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/conformance.py")
}

/// A path as the script's shell-style splitting reads it: quoted, so a Windows
/// backslash is a character and not an escape.
fn quoted(p: &Path) -> String {
    format!("'{}'", p.display())
}

fn init(root: &Path) {
    let out = Command::new(env!("CARGO_BIN_EXE_amem"))
        .arg("--root")
        .arg(root)
        .args(["init", "--owner", "human:test:me", "--domain", "dom-conf"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Run the script with `args`; its whole output, which must end on every step passing.
fn conforms(args: &[String]) -> String {
    let out = Command::new(python())
        .arg(script())
        .args(args)
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "conformance.py failed:\n{text}");
    text
}

#[test]
fn the_seven_steps_pass_over_stdio_with_the_operator() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("memory");
    init(&root);
    let amem = quoted(Path::new(env!("CARGO_BIN_EXE_amem")));
    let index = root.join("state").join("index.db");
    let text = conforms(&[
        "--stdio".into(),
        format!("{amem} --root {} serve", quoted(&root)),
        "--operator".into(),
        format!("{amem} --root {}", quoted(&root)),
        "--drop-index".into(),
        format!(
            "{} -c \"import os; os.remove(r'{}')\"",
            python(),
            index.display()
        ),
    ]);
    assert!(text.contains("7 of 7 steps passed"), "{text}");
    assert!(
        !text.contains("the loss was not shown"),
        "the rebuild was shown from a lost index: {text}"
    );
}

/// The same seven steps with the ledger on PostgreSQL rather than in the SQLite file:
/// the grants name the server, the binary connects to it, and the tasks the steps file
/// land there. Skipped, saying so, where no server can be had (see
/// `dm_ledger::testing`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_seven_steps_pass_over_stdio_with_a_postgres_ledger() {
    let Some(server) = dm_ledger::testing::PgServer::start().await.unwrap() else {
        eprintln!("skipped: the seven steps over a PostgreSQL ledger need a server");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("memory");
    init(&root);
    let grants = root.join("grants.toml");
    let mut text = std::fs::read_to_string(&grants).unwrap();
    text.push_str(&format!("\n[ledger]\nurl = \"{}\"\n", server.url()));
    std::fs::write(&grants, text).unwrap();
    let amem = quoted(Path::new(env!("CARGO_BIN_EXE_amem")));
    let index = root.join("state").join("index.db");
    let args = vec![
        "--stdio".into(),
        format!("{amem} --root {} serve", quoted(&root)),
        "--operator".into(),
        format!("{amem} --root {}", quoted(&root)),
        "--drop-index".into(),
        format!(
            "{} -c \"import os; os.remove(r'{}')\"",
            python(),
            index.display()
        ),
    ];
    let text = tokio::task::spawn_blocking(move || conforms(&args))
        .await
        .unwrap();
    assert!(text.contains("7 of 7 steps passed"), "{text}");
    // the tasks are on the server, and nothing was written to the SQLite file
    let (client, conn) = tokio_postgres::connect(server.url(), tokio_postgres::NoTls)
        .await
        .unwrap();
    tokio::spawn(conn);
    let row = client
        .query_one("SELECT COUNT(*) FROM tasks", &[])
        .await
        .unwrap();
    let tasks: i64 = row.get(0);
    assert!(tasks > 0, "the steps filed tasks on the server");
    assert!(
        !root.join("state").join("ledger.db").exists(),
        "no SQLite ledger was opened"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_seven_steps_pass_over_http() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("memory");
    init(&root);
    let memory = Arc::new(dm_server::Memory::open(&root).unwrap());
    let egress = dm_egress::Egress::new(
        memory,
        std::collections::HashMap::new(),
        &dir.path().join("telemetry.db"),
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let app = egress.router();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let amem = quoted(Path::new(env!("CARGO_BIN_EXE_amem")));
    let args = vec![
        "--url".into(),
        url,
        "--operator".into(),
        format!("{amem} --root {}", quoted(&root)),
    ];
    let text = tokio::task::spawn_blocking(move || conforms(&args))
        .await
        .unwrap();
    assert!(text.contains("7 of 7 steps passed"), "{text}");
}
