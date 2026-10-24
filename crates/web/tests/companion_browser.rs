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
use support::chrome_for_extensions;

/// The token the test server is started with; the launcher asks the companion with it.
const TOKEN: &str = "companion-test-token";

async fn serve(app: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

/// Requests the site answered under the name `localhost`, where a redirect and a page's
/// script send the browser: a request the companion must stop before it is made.
static ELSEWHERE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn who(headers: &HeaderMap) -> String {
    let cookie = headers
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    cookie
        .split(';')
        .map(str::trim)
        .find_map(|c| c.strip_prefix("session="))
        .unwrap_or("nobody")
        .to_string()
}

fn port_of(headers: &HeaderMap) -> String {
    let host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if host.starts_with("localhost") {
        ELSEWHERE.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
    host.rsplit(':').next().unwrap_or("80").to_string()
}

/// A site with a sign-in that sets a cookie and sends the browser on to `next`, a page
/// that says who is signed in, the same page answered late, a page that says so only once
/// its script has run, a redirect to the same server under another name, a page whose
/// script goes there, and a page whose script goes to `about:blank`.
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
        let who = who(&headers);
        port_of(&headers);
        Html(format!(
            "<html><head><title>Account</title></head><body><p>Signed in as {who}.</p></body></html>"
        ))
    }
    // the page, answered long after a tab's first look at its navigation
    async fn late(headers: HeaderMap) -> Html<String> {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        page(headers).await
    }
    // the page is a shell until its script runs, which only a browser does
    async fn app(headers: HeaderMap) -> Html<String> {
        let who = who(&headers);
        Html(format!(
            "<html><head><title>Shell</title></head><body><p id=\"out\">loading</p>\
             <script>setTimeout(function () {{ document.title = 'Built'; \
             document.getElementById('out').textContent = 'Built by script for {who}.'; }}, 200);</script>\
             </body></html>"
        ))
    }
    async fn away(headers: HeaderMap) -> Redirect {
        let port = port_of(&headers);
        Redirect::temporary(&format!("http://localhost:{port}/page"))
    }
    async fn leave(headers: HeaderMap) -> Html<String> {
        let port = port_of(&headers);
        Html(format!(
            "<html><head><title>Leaving</title></head><body>\
             <script>location.href = 'http://localhost:{port}/page';</script></body></html>"
        ))
    }
    async fn blank() -> Html<&'static str> {
        Html(
            "<html><head><title>Blanking</title></head><body>\
             <script>location.href = 'about:blank';</script></body></html>",
        )
    }
    serve(
        Router::new()
            .route("/login", get(login))
            .route("/page", get(page))
            .route("/late", get(late))
            .route("/app", get(app))
            .route("/away", get(away))
            .route("/leave", get(leave))
            .route("/blank", get(blank)),
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

/// An ask to render: the page opened in a tab and read as its scripts built it.
async fn render(addr: SocketAddr, url: &str, hosts: &[&str]) -> Value {
    let body =
        json!({"url": url, "hosts": hosts, "max_bytes": 100_000, "render": true}).to_string();
    let (status, body) = http(addr, "POST", dm_web::companion::FETCH_PATH, &body).await;
    assert_eq!(status, 200, "{body}");
    serde_json::from_str(&body).unwrap()
}

/// The extension's version, as its manifest says.
fn version() -> String {
    let manifest = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("companion/manifest.json"),
    )
    .unwrap();
    serde_json::from_str::<Value>(&manifest).unwrap()["version"]
        .as_str()
        .unwrap()
        .to_string()
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
    let child = Command::new(chrome_for_extensions())
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
        if shown["connected"] == true && shown["version"] == version() {
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

/// Rendered: a page that is a shell until its script runs is opened in a tab, signed in,
/// and read as built; a page whose script leaves the hosts is stopped at the navigation.
async fn rendered(web: SocketAddr, site: SocketAddr, chrome: &Browser) {
    // rendered: the page is opened in a tab, signed in, and read once its script built it
    let app = format!("http://127.0.0.1:{}/app", site.port());
    let built = render(web, &app, &["127.0.0.1"]).await;
    assert_eq!(built["status"], 200, "{built}\n{}", chrome.log());
    assert_eq!(built["final_url"], app, "{built}");
    assert_eq!(built["rendered"], true, "{built}");
    let body = built["body"].as_str().unwrap();
    assert!(
        body.contains("Built by script for owner-42.") && body.contains("<title>Built</title>"),
        "the page was not read as its script built it: {built}"
    );
    assert!(built["content_type"]
        .as_str()
        .unwrap()
        .starts_with("text/html"));

    // a rendered page whose script leaves the hosts is stopped at the navigation
    let left = render(
        web,
        &format!("http://127.0.0.1:{}/leave", site.port()),
        &["127.0.0.1"],
    )
    .await;
    let why = left["error"].as_str().unwrap();
    assert!(
        why.contains("may not reach") && why.contains("stopped before it was fetched"),
        "{left}\n{}",
        chrome.log()
    );
    assert!(left.get("body").is_none(), "{left}");
    assert_eq!(
        ELSEWHERE.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the tab left the hosts"
    );
}

/// Rendered, at the tab's own `about:blank`: a page that answers late is waited for, and
/// a page whose script sends the tab to `about:blank` is refused.
async fn rendered_by_the_blank(web: SocketAddr, site: SocketAddr, chrome: &Browser) {
    // a page that answers late is waited for: until the page commits, the tab shows the
    // blank it was opened on, which is not a place the page was sent to
    let late = format!("http://127.0.0.1:{}/late", site.port());
    let waited = render(web, &late, &["127.0.0.1"]).await;
    assert_eq!(waited["status"], 200, "{waited}\n{}", chrome.log());
    assert_eq!(waited["final_url"], late, "{waited}");
    assert!(
        waited["body"]
            .as_str()
            .unwrap()
            .contains("Signed in as owner-42."),
        "{waited}"
    );

    // a rendered page whose script sends the tab to about:blank is refused, not read
    let blanked = render(
        web,
        &format!("http://127.0.0.1:{}/blank", site.port()),
        &["127.0.0.1"],
    )
    .await;
    assert_eq!(blanked["final_url"], "about:blank", "{blanked}");
    assert!(
        blanked["error"].as_str().unwrap().contains("may not reach"),
        "{blanked}\n{}",
        chrome.log()
    );
    assert!(blanked.get("body").is_none(), "{blanked}");
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

    // a redirect to a host off them is stopped before it is made: the site never hears
    // the request, and the refusal says where it was going
    let away = ask(
        web,
        &format!("http://127.0.0.1:{}/away", site.port()),
        &["127.0.0.1"],
    )
    .await;
    let why = away["error"].as_str().unwrap();
    assert!(
        why.contains("may not reach") && why.contains("stopped before it was fetched"),
        "{away}\n{}",
        chrome.log()
    );
    assert!(
        away["final_url"]
            .as_str()
            .unwrap()
            .starts_with("http://localhost:"),
        "{away}"
    );
    assert!(away.get("body").is_none(), "{away}");
    assert_eq!(
        ELSEWHERE.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the redirect was followed"
    );

    rendered(web, site, &chrome).await;
    rendered_by_the_blank(web, site, &chrome).await;

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
