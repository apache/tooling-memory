//! The browser reader, driven by a real browser: `memory web` serves the page, the
//! page exports a domain into a bundle, verifies every entry's digest against the
//! manifest, indexes the entries in SQLite with FTS5 compiled to WebAssembly, and ranks
//! a query the way the server does. Headless Chrome or Chromium loads the page with a
//! virtual-time budget so the asynchronous work finishes before the DOM is dumped.

// An integration test is test code; a panic here is the failure being reported.
#![allow(clippy::unwrap_used)]

use axum::extract::{Path as AxumPath, State};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use dm_core::Label;
use dm_server::{Memory, WriteRequest};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A Chrome or Chromium to drive: `CHROME` in the environment, else where the usual
/// installs put it, including the Playwright browser directory.
fn chrome() -> PathBuf {
    if let Some(c) = std::env::var_os("CHROME") {
        return PathBuf::from(c);
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
    candidates
        .into_iter()
        .find(|p| p.is_file())
        .or_else(chrome_in_a_container)
        .expect("no Chrome, Chromium or container runtime found; install one or set CHROME=<path> for the reader test")
}

/// The Playwright image's Chrome, through scripts/chrome-container.sh, when a container
/// runtime answers: a host with no Chrome, or one without the libraries it needs. Linux
/// only, where the container's host network is this host's and reaches the test's server;
/// elsewhere a container runs in a virtual machine whose loopback is its own.
fn chrome_in_a_container() -> Option<PathBuf> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    // `info` asks the daemon, so a runtime that is installed but cannot run a container
    // leaves the clear message above rather than a failed `run`
    let runs = |runtime: &str| {
        std::process::Command::new(runtime)
            .arg("info")
            .output()
            .is_ok_and(|o| o.status.success())
    };
    (runs("podman") || runs("docker"))
        .then(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/chrome-container.sh"))
}

/// A record with three entries, one of them restricted and one carrying an event
/// handler in its Markdown.
async fn seed(root: &Path) -> Vec<String> {
    let m = Memory::init(root, "human:test:me", "dom-me").unwrap();
    let mut ids = Vec::new();
    for (title, body, label) in [
        (
            "Retry policy",
            "Operation R retries with exponential backoff.\n",
            Label::Internal,
        ),
        (
            "Commit convention",
            "Short subject, body says why.\n\n<img src=x onerror=\"document.title='owned'\">\n",
            Label::Internal,
        ),
        (
            "Key rotation",
            "The signing key rotates quarterly.\n",
            Label::Restricted,
        ),
    ] {
        let w = m
            .write(WriteRequest {
                title: Some(title.into()),
                body: body.into(),
                classification: Some(label),
                ..WriteRequest::default()
            })
            .await
            .unwrap();
        ids.push(w.id);
    }
    ids
}

/// The token the test server is started with; the page is opened on a pairing link
/// minted with it, as `amem` opens it.
const TOKEN: &str = "reader-test-token";

/// `amem web` over `root`, on a loopback port.
async fn web(dir: &Path, root: PathBuf) -> SocketAddr {
    let access = dm_web::Access::with_token(&root, TOKEN);
    let app = dm_web::router(dm_web::Config {
        root,
        memory_binary: "memory".into(),
        workdir: dir.to_path_buf(),
        harness: Some("claude".into()),
        auth: "subscription".into(),
        backend: "auto".into(),
        command_override: Some(vec!["true".into()]),
        access,
    })
    .unwrap();
    serve(app).await
}

async fn serve(app: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

/// A directory served as plain files from another origin, with the CORS header a
/// static host would set; what a bundle published anywhere looks like to the page.
async fn static_host(dir: PathBuf) -> SocketAddr {
    async fn file(
        State(dir): State<PathBuf>,
        AxumPath(name): AxumPath<String>,
    ) -> impl IntoResponse {
        match std::fs::read(dir.join(name)) {
            Ok(bytes) => (
                axum::http::StatusCode::OK,
                [("access-control-allow-origin", "*")],
                bytes,
            ),
            Err(_) => (
                axum::http::StatusCode::NOT_FOUND,
                [("access-control-allow-origin", "*")],
                Vec::new(),
            ),
        }
    }
    serve(Router::new().route("/{name}", get(file)).with_state(dir)).await
}

/// A pairing code from the server `url` is on, as `amem` asks for one with the token.
async fn pairing_code(url: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let authority = url
        .strip_prefix("http://")
        .unwrap()
        .split('/')
        .next()
        .unwrap();
    let mut s = tokio::net::TcpStream::connect(authority).await.unwrap();
    let req = format!(
        "POST /api/pair HTTP/1.1\r\nHost: {authority}\r\nAuthorization: Bearer {TOKEN}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    let body = out.split_once("\r\n\r\n").unwrap().1;
    serde_json::from_str::<serde_json::Value>(body).unwrap()["code"]
        .as_str()
        .unwrap()
        .to_string()
}

/// The page at `url` after its asynchronous work, as the browser serialises it.
async fn dump(url: String) -> String {
    let url = format!("{url}&pair={}", pairing_code(&url).await);
    let chrome = chrome();
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
    let dom = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        dom.contains("Reader · amem"),
        "the page did not load:\n{dom}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    dom
}

fn hit_ids(dom: &str) -> Vec<&str> {
    dom.match_indices("<li class=\"am-hit")
        .map(|(i, _)| {
            let rest = &dom[i..];
            let rest = &rest[rest.find("data-id=\"").unwrap() + 9..];
            &rest[..rest.find('"').unwrap()]
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_reader_loads_a_domain_verifies_it_and_ranks_like_the_server() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("memory");
    seed(&root).await;
    let addr = web(dir.path(), root).await;
    let dom = dump(format!(
        "http://{addr}/reader?bundle=bundle/dom-me/restricted&q=backoff"
    ))
    .await;
    assert!(
        dom.contains("dom-me: 3 entries, highest label restricted"),
        "the bundle did not load:\n{dom}"
    );
    let open = r#"class="hit-open" type="button">"#;
    let first_hit = dom
        .find(open)
        .map(|i| &dom[i + open.len()..i + open.len() + 30])
        .unwrap_or_default();
    assert!(
        first_hit.starts_with("Retry policy"),
        "the entry with the term ranks first: {first_hit:?}\n{dom}"
    );
    assert_eq!(
        hit_ids(&dom).len(),
        1,
        "only the entry with the term matches:\n{dom}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tampered_entry_is_refused_even_when_the_manifest_blanks_its_digest() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("memory");
    let ids = seed(&root).await;
    let published = dir.path().join("published");
    Memory::open(&root)
        .unwrap()
        .export(Some("dom-me"), Label::Internal, &published)
        .await
        .unwrap();
    // the entry changes after the export, and the manifest's digest for it is blanked
    // so a reader that only checks a digest it was given admits the change
    let file = published.join(format!("{}.md", ids[0]));
    let text = std::fs::read_to_string(&file).unwrap();
    std::fs::write(&file, text.replace("exponential", "no")).unwrap();
    let mpath = published.join("manifest.yaml");
    let mut manifest =
        dm_core::manifest::Manifest::from_yaml(&std::fs::read_to_string(&mpath).unwrap()).unwrap();
    let tampered = manifest
        .entries
        .iter_mut()
        .find(|e| e.id == ids[0])
        .unwrap();
    tampered.digest = String::new();
    std::fs::write(&mpath, manifest.to_yaml()).unwrap();
    let host = static_host(published).await;
    let addr = web(dir.path(), root).await;
    let dom = dump(format!("http://{addr}/reader?bundle=http://{host}")).await;
    assert!(
        dom.contains("dom-me: 1 entries, highest label internal") && dom.contains("1 refused"),
        "the tampered entry was admitted:\n{dom}"
    );
    assert!(!hit_ids(&dom).contains(&ids[0].as_str()), "{dom}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_handling_in_the_url_hides_what_is_above_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("memory");
    let ids = seed(&root).await;
    let addr = web(dir.path(), root).await;
    let dom = dump(format!(
        "http://{addr}/reader?bundle=bundle/dom-me/restricted&handling=internal&open={}",
        ids[2]
    ))
    .await;
    let shown = hit_ids(&dom);
    assert!(
        shown.len() == 2 && !shown.contains(&ids[2].as_str()),
        "the restricted entry shows at handling internal: {shown:?}\n{dom}"
    );
    assert!(
        !dom.contains("quarterly"),
        "the restricted entry opened from the URL above the handling:\n{dom}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_entry_opened_from_the_url_is_rendered_without_its_scripts() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("memory");
    let ids = seed(&root).await;
    let addr = web(dir.path(), root).await;
    let dom = dump(format!(
        "http://{addr}/reader?bundle=bundle/dom-me/internal&open={}",
        ids[1]
    ))
    .await;
    assert!(
        dom.contains(r#"<h2 class="am-entry-title">Commit convention</h2>"#)
            && dom.contains("body says why"),
        "the entry named in the URL did not open:\n{dom}"
    );
    assert!(
        !dom.contains("onerror") && !dom.contains("<title>owned"),
        "an event handler in an entry's Markdown survived rendering:\n{dom}"
    );
}
