//! The browser reader, driven by a real browser: `memory web` serves the page, the
//! page exports a domain into a bundle, verifies every entry's digest against the
//! manifest, indexes the entries in SQLite with FTS5 compiled to WebAssembly, and ranks
//! a query the way the server does. Headless Chrome or Chromium loads the page with a
//! virtual-time budget so the asynchronous work finishes before the DOM is dumped.

// An integration test is test code; a panic here is the failure being reported.
#![allow(clippy::unwrap_used)]

use dm_server::{Memory, WriteRequest};
use std::path::PathBuf;
use std::time::Duration;

/// A Chrome or Chromium to drive: `CHROME` in the environment, else where the usual
/// installs put it, including the Playwright browser directory.
fn chrome() -> Option<PathBuf> {
    if let Some(c) = std::env::var_os("CHROME") {
        return Some(PathBuf::from(c));
    }
    let mut candidates: Vec<PathBuf> = vec![
        "/usr/bin/google-chrome".into(),
        "/usr/bin/google-chrome-stable".into(),
        "/usr/bin/chromium".into(),
        "/usr/bin/chromium-browser".into(),
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome".into(),
        r"C:\Program Files\Google\Chrome\Application\chrome.exe".into(),
        r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe".into(),
    ];
    if let Ok(dirs) = std::fs::read_dir("/opt/pw-browsers") {
        for d in dirs.flatten() {
            if d.file_name().to_string_lossy().starts_with("chromium-") {
                candidates.push(d.path().join("chrome-linux/chrome"));
            }
        }
    }
    candidates.into_iter().find(|p| p.is_file())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_reader_loads_a_domain_verifies_it_and_ranks_like_the_server() {
    let chrome = chrome().expect(
        "no Chrome or Chromium found; install one or set CHROME=<path> for the reader test",
    );
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("memory");
    let m = Memory::init(&root, "human:test:me", "dom-me").unwrap();
    for (title, body) in [
        (
            "Retry policy",
            "Operation R retries with exponential backoff.\n",
        ),
        ("Commit convention", "Short subject, body says why.\n"),
        (
            "Import steps",
            "The import has five steps; the fourth is slow.\n",
        ),
    ] {
        m.write(WriteRequest {
            title: Some(title.into()),
            body: body.into(),
            ..WriteRequest::default()
        })
        .await
        .unwrap();
    }
    drop(m);
    let app = dm_web::router(dm_web::Config {
        root,
        memory_binary: "memory".into(),
        workdir: dir.path().to_path_buf(),
        harness: "claude".into(),
        auth: "subscription".into(),
        backend: "auto".into(),
        command_override: Some(vec!["true".into()]),
    })
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let url = format!("http://{addr}/reader?bundle=bundle/dom-me/internal&q=backoff");
    let out = tokio::time::timeout(
        Duration::from_secs(120),
        tokio::task::spawn_blocking(move || {
            std::process::Command::new(chrome)
                .args([
                    "--headless=new",
                    "--no-sandbox",
                    "--disable-gpu",
                    "--disable-dev-shm-usage",
                    "--virtual-time-budget=20000",
                    "--dump-dom",
                    &url,
                ])
                .output()
                .expect("chrome runs")
        }),
    )
    .await
    .expect("chrome finished inside two minutes")
    .unwrap();
    server.abort();
    let dom = String::from_utf8_lossy(&out.stdout);
    assert!(
        dom.contains("dom-me: 3 entries, highest label internal"),
        "the bundle did not load:\n{dom}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let first_hit = dom
        .find(r#"class="t">"#)
        .map(|i| &dom[i + 10..i + 40])
        .unwrap_or_default();
    assert!(
        first_hit.starts_with("Retry policy"),
        "the entry with the term ranks first: {first_hit:?}\n{dom}"
    );
    let hits = dom.matches("<li data-id=").count();
    assert_eq!(hits, 1, "only the entry with the term matches:\n{dom}");
}
