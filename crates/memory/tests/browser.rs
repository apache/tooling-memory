//! Fetch in the owner's browser (PLAN M19, third cut), end to end: a root whose fetch
//! grant says `browser = true`, the web launcher over it in this process, and a
//! companion on its socket standing in for the extension. The launcher decides the URL
//! by the policy before the browser is asked, the ask names the hosts the session may
//! reach, the URL that answered is decided before anything is kept, and what is kept
//! says the browser observed it. No launcher, or no companion, is a plain refusal.

#![allow(clippy::unwrap_used)]

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

/// The web launcher's token: the launcher asks the companion with it.
const TOKEN: &str = "browser-test-token-0123456789abcdef0123456789abcdef";

fn amem(root: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_amem"));
    c.arg("--root").arg(root).env_remove("MEMORY_ROOT");
    c
}

fn ok(out: &Output) -> String {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout.clone()).unwrap()
}

fn failed(out: &Output) -> String {
    assert!(
        !out.status.success(),
        "succeeded: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// A root whose fetch is the browser's, reaching loopback's address and `docs.example`.
fn init(dir: &Path) -> PathBuf {
    let root = dir.join("memory");
    ok(&amem(&root)
        .args(["init", "--owner", "human:test:me", "--domain", "dom-me"])
        .output()
        .unwrap());
    let mut g = std::fs::OpenOptions::new()
        .append(true)
        .open(root.join("grants.toml"))
        .unwrap();
    write!(
        g,
        "\n[taps.fetch]\ndomain = \"dom-me\"\ntool = true\nceiling = \"public\"\nhosts = [\"127.0.0.1\", \"docs.example\"]\nloopback = true\nbrowser = true\n"
    )
    .unwrap();
    root
}

/// The web launcher over `root`, in this process, as `amem` runs it: its address is
/// written to `state/web.json`, where a launcher looks for it.
async fn web(root: &Path) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let state = dm_web::state::WebState {
        token: TOKEN.into(),
        url: None,
        workdir: None,
    };
    let access = dm_web::Access::new(root, &state, vec![]);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    dm_web::state::save(
        root,
        &dm_web::state::WebState {
            url: Some(format!("http://{addr}")),
            workdir: Some(root.display().to_string()),
            ..state
        },
    )
    .unwrap();
    let config = dm_web::Config {
        root: root.to_path_buf(),
        memory_binary: "amem".into(),
        workdir: root.to_path_buf(),
        harness: Some("claude".into()),
        auth: "subscription".into(),
        backend: "auto".into(),
        command_override: Some(vec!["true".into()]),
        access,
    };
    let h = tokio::spawn(async move {
        dm_web::serve(config, listener).await.unwrap();
    });
    (addr, h)
}

/// A companion on the launcher's socket, as the extension connects: it says hello,
/// keeps every ask it gets, and answers each with what `respond` makes of it.
async fn companion(
    addr: SocketAddr,
    respond: fn(&Value) -> Value,
) -> (Arc<Mutex<Vec<Value>>>, tokio::task::JoinHandle<()>) {
    let mut req = format!("ws://{addr}{}", dm_web::companion::WS_PATH)
        .into_client_request()
        .unwrap();
    req.headers_mut().insert(
        "sec-websocket-protocol",
        format!("amem, amem.key.{TOKEN}").parse().unwrap(),
    );
    let (ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    let (mut tx, mut rx) = ws.split();
    tx.send(Message::Text(
        json!({"type": "hello", "version": "test"})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    let asks = Arc::new(Mutex::new(Vec::new()));
    let seen = asks.clone();
    let h = tokio::spawn(async move {
        while let Some(Ok(Message::Text(t))) = rx.next().await {
            let m: Value = serde_json::from_str(&t).unwrap();
            if m["type"] != "fetch" {
                continue;
            }
            seen.lock().unwrap().push(m.clone());
            let mut r = respond(&m);
            r["id"] = m["id"].clone();
            tx.send(Message::Text(r.to_string().into())).await.unwrap();
        }
    });
    (asks, h)
}

/// What the browser says: a signed-in page for anything but `/away`, which answers from
/// a host the session may not reach.
fn browser(ask: &Value) -> Value {
    let url = ask["url"].as_str().unwrap();
    if url.ends_with("/away") {
        return json!({
            "status": 200, "content_type": "text/html",
            "final_url": "https://evil.example/page", "body": "<p>elsewhere</p>", "truncated": false,
        });
    }
    json!({
        "status": 200, "content_type": "text/html; charset=utf-8", "final_url": url,
        "body": "<html><head><title>Release policy</title></head><body><p>Signed in as the owner. Votes run for 72 hours.</p></body></html>",
        "truncated": false,
    })
}

/// Whether the launcher shows a companion connected.
async fn connected(addr: SocketAddr) -> bool {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let req = format!(
        "GET {} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {TOKEN}\r\nConnection: close\r\n\r\n",
        dm_web::companion::STATUS_PATH
    );
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    let body = out.split_once("\r\n\r\n").unwrap().1;
    serde_json::from_str::<Value>(body).unwrap()["connected"] == true
}

async fn fetch(root: &Path, url: &str) -> Output {
    let root = root.to_path_buf();
    let args = json!({"url": url}).to_string();
    tokio::task::spawn_blocking(move || {
        amem(&root).args(["tool", "fetch", &args]).output().unwrap()
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_browser_fetch_is_decided_before_and_after_and_kept_as_the_browser_s() {
    let dir = tempfile::tempdir().unwrap();
    let root = init(dir.path());
    let page = "http://127.0.0.1:9/page";

    // no web launcher: a plain refusal, before anything is asked
    let why = failed(&fetch(&root, page).await);
    assert!(why.contains("no web launcher is running"), "{why}");

    let (addr, _web) = web(&root).await;
    let why = failed(&fetch(&root, page).await);
    assert!(why.contains("no companion is connected"), "{why}");

    let (asks, _c) = companion(addr, browser).await;
    for _ in 0..50 {
        if connected(addr).await {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(
        connected(addr).await,
        "the companion never showed as connected"
    );

    // the browser fetched, and the answer is the tool's shape, kept as the browser's
    let answer: Value = serde_json::from_str(&ok(&fetch(&root, page).await)).unwrap();
    assert_eq!(answer["title"], "Release policy", "{answer}");
    assert!(
        answer["text"].as_str().unwrap().contains("72 hours"),
        "{answer}"
    );
    assert_eq!(answer["observed_by"], "browser", "{answer}");
    assert_eq!(answer["reached"], json!([page]), "{answer}");
    assert_eq!(answer["cached"], false);
    {
        let asks = asks.lock().unwrap();
        assert_eq!(asks.len(), 1, "{asks:?}");
        // the ask names the hosts the session may reach, and the cap
        let hosts = asks[0]["hosts"].as_array().unwrap();
        assert!(
            hosts.contains(&json!("127.0.0.1")) && hosts.contains(&json!("docs.example")),
            "{asks:?}"
        );
        assert_eq!(asks[0]["max_bytes"], 2_000_000);
    }

    // the same call again is answered from its source; the browser is not asked
    let again: Value = serde_json::from_str(&ok(&fetch(&root, page).await)).unwrap();
    assert_eq!(again["cached"], true, "{again}");
    assert_eq!(again["source"], answer["source"]);
    assert_eq!(asks.lock().unwrap().len(), 1);

    // a URL off the grant is refused here: the browser never hears of it
    let why = failed(&fetch(&root, "https://other.example/x").await);
    assert!(why.contains("refused"), "{why}");
    assert_eq!(asks.lock().unwrap().len(), 1, "the browser was asked");

    // an answer from a host the session may not reach is not believed, and not kept
    let why = failed(&fetch(&root, "http://127.0.0.1:9/away").await);
    assert!(why.contains("may not reach"), "{why}");
    assert_eq!(asks.lock().unwrap().len(), 2);
    let sources: Value =
        serde_json::from_str(&ok(&amem(&root).args(["source", "list"]).output().unwrap())).unwrap();
    let sources = sources.as_array().unwrap();
    assert_eq!(sources.len(), 1, "{sources:?}");
    assert_eq!(sources[0]["id"], answer["source"]);
}
