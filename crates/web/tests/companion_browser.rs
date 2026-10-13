//! The companion extension, driven by a real browser (PLAN M19, third cut): Chrome
//! loads `crates/web/companion` unpacked, signs in to a site that sets a cookie, and
//! lands on the launcher's page by a pairing link; the page hands the extension its key,
//! the extension connects, and a fetch the server asks of it comes back made with the
//! browser's cookies, so the site answers the session as signed in. An ask off the
//! hosts it names is refused in the extension, and an answer from a host off them is
//! reported, not read.
//!
//! Headless Chromium opens a page named on its command line in a target no extension
//! sees as a tab, so the page is opened as a tab through the DevTools endpoint instead,
//! the way a browser-automation client opens one.

// An integration test is test code; a panic here is the failure being reported.
#![allow(clippy::unwrap_used)]

use axum::http::HeaderMap;
use axum::response::{Html, IntoResponse, Redirect};
use axum::routing::get;
use axum::Router;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

mod support;
use support::chrome;

/// The token the test server is started with; the launcher asks the companion with it.
const TOKEN: &str = "companion-test-token";

async fn serve(app: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

/// A site with a sign-in that sets a cookie and sends the browser on to `next`, a page
/// that says who is signed in, and a redirect to the same server under another name.
async fn site() -> SocketAddr {
    async fn login(
        axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
    ) -> impl IntoResponse {
        (
            [("set-cookie", "session=owner-42; Path=/")],
            Redirect::temporary(q.get("next").map_or("/page", String::as_str)),
        )
    }
    async fn page(headers: HeaderMap) -> Html<String> {
        let cookie = headers
            .get("cookie")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        let who = cookie
            .split(';')
            .map(str::trim)
            .find_map(|c| c.strip_prefix("session="))
            .unwrap_or("nobody");
        Html(format!(
            "<html><head><title>Account</title></head><body><p>Signed in as {who}.</p></body></html>"
        ))
    }
    async fn away(headers: HeaderMap) -> Redirect {
        let host = headers
            .get("host")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        let port = host.rsplit(':').next().unwrap_or("80");
        Redirect::temporary(&format!("http://localhost:{port}/page"))
    }
    serve(
        Router::new()
            .route("/login", get(login))
            .route("/page", get(page))
            .route("/away", get(away)),
    )
    .await
}

/// `amem web` over a fresh root, on a loopback port.
async fn web(dir: &Path) -> SocketAddr {
    let root = dir.join("memory");
    dm_server::Memory::init(&root, "human:test:me", "dom-me").unwrap();
    let access = dm_web::Access::with_token(&root, TOKEN);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    access.bound(addr);
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
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

/// One HTTP/1.1 exchange on loopback, with a bearer when given.
async fn exchange(
    addr: SocketAddr,
    method: &str,
    path: &str,
    bearer: Option<&str>,
    body: &str,
) -> (u16, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let auth = bearer
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).await.unwrap();
    // read to the end of the body, by its length: the DevTools endpoint keeps the
    // connection open whatever the request asked, so reading to a close would wait forever
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let n = tokio::time::timeout_at(deadline, s.read(&mut buf))
            .await
            .expect("an answer within thirty seconds")
            .unwrap();
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
        let text = String::from_utf8_lossy(&out);
        if let Some((head, body)) = text.split_once("\r\n\r\n") {
            let length = head.lines().find_map(|l| {
                l.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
            });
            if length.is_some_and(|l| body.len() >= l) {
                break;
            }
        }
    }
    let out = String::from_utf8_lossy(&out).into_owned();
    let status = out.split(' ').nth(1).unwrap().parse().unwrap();
    (status, out.split_once("\r\n\r\n").unwrap().1.to_string())
}

/// One exchange with the launcher, as the launcher's own process asks it.
async fn http(addr: SocketAddr, method: &str, path: &str, body: &str) -> (u16, String) {
    exchange(addr, method, path, Some(TOKEN), body).await
}

async fn ask(addr: SocketAddr, url: &str, hosts: &[&str]) -> Value {
    let body = json!({"url": url, "hosts": hosts, "max_bytes": 100_000}).to_string();
    let (status, body) = http(addr, "POST", dm_web::companion::FETCH_PATH, &body).await;
    assert_eq!(status, 200, "{body}");
    serde_json::from_str(&body).unwrap()
}

/// Chrome, stopped when the test is done with it.
struct Browser {
    child: Child,
    log: std::path::PathBuf,
}

impl Browser {
    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// Where its DevTools endpoint listens, once it says so on stderr.
    async fn devtools(&self) -> SocketAddr {
        let started = Instant::now();
        loop {
            let log = self.log();
            if let Some(rest) = log.split("DevTools listening on ws://").nth(1) {
                let authority = rest.split('/').next().unwrap();
                return authority.parse().unwrap();
            }
            assert!(
                started.elapsed() < Duration::from_secs(60),
                "chrome gave no DevTools endpoint within a minute; it said:\n{log}"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// Open `url` in a new tab, through the DevTools endpoint.
    async fn open(&self, url: &str) {
        let devtools = self.devtools().await;
        let (status, body) = exchange(devtools, "PUT", &format!("/json/new?{url}"), None, "").await;
        assert_eq!(status, 200, "DevTools would not open a tab: {body}");
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        // asked, not killed: Chrome through a container wrapper is stopped by the signal
        // the wrapper forwards, and a kill of the wrapper would leave it running
        let _ = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status();
        let gone_by = Instant::now() + Duration::from_secs(10);
        while Instant::now() < gone_by {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Headless Chrome with the companion loaded and its DevTools endpoint on a free port;
/// it stays up until dropped.
fn browser(dir: &Path) -> Browser {
    let extension = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("companion")
        .canonicalize()
        .unwrap();
    let log = dir.join("chrome.log");
    let child = Command::new(chrome())
        .args([
            "--headless=new",
            "--no-sandbox",
            "--disable-gpu",
            "--disable-dev-shm-usage",
            "--no-first-run",
            "--enable-logging=stderr",
            "--remote-debugging-port=0",
        ])
        .arg(format!("--load-extension={}", extension.display()))
        .arg("about:blank")
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&log).unwrap())
        .spawn()
        .expect("chrome runs");
    Browser { child, log }
}

/// Wait for the launcher to show a companion connected, with the extension's version;
/// two minutes at most, then what Chrome said.
async fn connected(web: SocketAddr, chrome: &Browser) {
    let started = Instant::now();
    loop {
        let (status, body) = http(web, "GET", dm_web::companion::STATUS_PATH, "").await;
        assert_eq!(status, 200, "{body}");
        let shown: Value = serde_json::from_str(&body).unwrap();
        // the socket is up before the hello that names the version lands: wait for both
        if shown["connected"] == true && shown["version"] == "0.1.0" {
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(120),
            "no companion connected within two minutes; chrome said:\n{}",
            chrome.log()
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_companion_fetches_with_the_browser_s_cookies_and_only_where_the_ask_lets_it() {
    let dir = tempfile::tempdir().unwrap();
    let site = site().await;
    let web = web(dir.path()).await;
    let (_, pairing) = http(web, "POST", "/api/pair", "").await;
    let code = serde_json::from_str::<Value>(&pairing).unwrap()["code"]
        .as_str()
        .unwrap()
        .to_string();
    let chrome = browser(dir.path());
    // one tab: the sign-in, which sends the browser on to the launcher's page, paired
    let next = format!("http://{web}/?pair={code}")
        .replace('?', "%3F")
        .replace('=', "%3D");
    chrome
        .open(&format!(
            "http://127.0.0.1:{}/login?next={next}",
            site.port()
        ))
        .await;

    // the page pairs, hands the extension its key, and the extension connects
    connected(web, &chrome).await;

    // the browser signed in to the site; the session's fetch is signed in too
    let page = format!("http://127.0.0.1:{}/page", site.port());
    let got = ask(web, &page, &["127.0.0.1"]).await;
    assert_eq!(got["status"], 200, "{got}");
    assert_eq!(got["final_url"], page, "{got}");
    assert!(
        got["body"]
            .as_str()
            .unwrap()
            .contains("Signed in as owner-42"),
        "the cookie did not go with the fetch: {got}\n{}",
        chrome.log()
    );
    assert!(got["content_type"]
        .as_str()
        .unwrap()
        .starts_with("text/html"));
    assert_eq!(got["truncated"], false);

    // a URL off the hosts the ask names is not fetched
    let refused = ask(web, &page, &["docs.example"]).await;
    assert!(
        refused["error"]
            .as_str()
            .unwrap()
            .contains("not on a host this session may reach"),
        "{refused}"
    );

    // a redirect to a host off them is reported and not read
    let away = ask(
        web,
        &format!("http://127.0.0.1:{}/away", site.port()),
        &["127.0.0.1"],
    )
    .await;
    assert!(
        away["error"].as_str().unwrap().contains("may not reach"),
        "{away}"
    );
    assert!(
        away["final_url"]
            .as_str()
            .unwrap()
            .starts_with("http://localhost:"),
        "{away}"
    );
    assert!(away.get("body").is_none(), "{away}");

    // the cap holds
    let capped = {
        let body = json!({"url": page, "hosts": ["127.0.0.1"], "max_bytes": 10}).to_string();
        let (_, body) = http(web, "POST", dm_web::companion::FETCH_PATH, &body).await;
        serde_json::from_str::<Value>(&body).unwrap()
    };
    assert_eq!(capped["truncated"], true, "{capped}");
    assert_eq!(capped["body"].as_str().unwrap().len(), 10, "{capped}");
    drop(chrome);
}
