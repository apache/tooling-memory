//! A web terminal on the launcher.
//!
//! Each session is a pseudo-terminal running `memory launch`, kept alive by this server
//! whether or not a browser is attached. Its output goes into a ring buffer and to every
//! attached websocket; a browser that connects, or reconnects, gets the ring first, so a
//! refresh or a dropped connection lands in the same place. Beside the terminal a reading
//! pane renders entries from the record as they land. Loopback only; identity in front of
//! it comes later.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Json, Response};
use axum::routing::get;
use axum::Router;
use bytes::Bytes;
use dm_server::Memory;
use futures_util::{SinkExt, StreamExt};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

mod pages;

/// What can go wrong.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The pseudo-terminal could not be opened or the launcher could not be started.
    #[error("session: {0}")]
    Session(String),
    /// The listener.
    #[error("listen: {0}")]
    Io(#[from] std::io::Error),
    /// The record.
    #[error("memory: {0}")]
    Memory(#[from] dm_server::Error),
}

/// How much output a session keeps for late and returning browsers.
const RING_BYTES: usize = 2 * 1024 * 1024;

/// How to run the launcher.
#[derive(Clone, Debug)]
pub struct Config {
    /// The record.
    pub root: PathBuf,
    /// The `memory` binary; sessions run `<binary> --root <root> launch ...`.
    pub memory_binary: PathBuf,
    /// The working tree sessions start in.
    pub workdir: PathBuf,
    /// Default harness.
    pub harness: String,
    /// Default auth mode.
    pub auth: String,
    /// Default backend.
    pub backend: String,
    /// For tests: run this command verbatim instead of the launcher.
    pub command_override: Option<Vec<String>>,
}

/// A request to start a session.
#[derive(Debug, Default, Deserialize)]
pub struct NewSession {
    /// claude, codex, gemini, copilot, opencode.
    #[serde(default)]
    pub harness: Option<String>,
    /// subscription, key, gateway.
    #[serde(default)]
    pub auth: Option<String>,
    /// Another destination, where the harness allows it.
    #[serde(default)]
    pub provider: Option<String>,
    /// Non-interactive: a prompt.
    #[serde(default)]
    pub prompt: Option<String>,
    /// Task mode.
    #[serde(default)]
    pub work: bool,
    /// Arguments for the harness.
    #[serde(default)]
    pub args: Vec<String>,
}

/// One session as the API shows it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionInfo {
    /// The id in the URL.
    pub id: String,
    /// One line.
    pub title: String,
    /// Still running.
    pub alive: bool,
    /// The exit code, once it has one.
    pub exit: Option<i32>,
    /// When it started, RFC 3339.
    pub started: String,
}

struct Session {
    info: Mutex<SessionInfo>,
    writer: Mutex<Box<dyn Write + Send>>,
    master: Mutex<Box<dyn portable_pty::MasterPty + Send>>,
    child: Mutex<Box<dyn portable_pty::Child + Send + Sync>>,
    ring: Mutex<VecDeque<u8>>,
    live: broadcast::Sender<Bytes>,
    alive: AtomicBool,
}

impl Session {
    fn snapshot(&self) -> SessionInfo {
        let mut i = self
            .info
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        i.alive = self.alive.load(Ordering::SeqCst);
        i
    }
}

#[derive(Clone)]
struct App {
    config: Config,
    memory: Arc<Memory>,
    sessions: Arc<Mutex<BTreeMap<String, Arc<Session>>>>,
    /// Bundle exports run one at a time: each replaces a directory by renaming.
    exports: Arc<tokio::sync::Mutex<()>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn now() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

fn new_id() -> String {
    let n = time::OffsetDateTime::now_utc().unix_timestamp_nanos();
    format!(
        "{:x}",
        n as u64 ^ u64::from(std::process::id()).rotate_left(32)
    )
}

/// The launcher's command line for a request.
fn launch_argv(config: &Config, req: &NewSession) -> Vec<String> {
    if let Some(c) = &config.command_override {
        return c.clone();
    }
    let mut a = vec![
        config.memory_binary.display().to_string(),
        "--root".into(),
        config.root.display().to_string(),
        if req.work { "work" } else { "launch" }.into(),
        "--harness".into(),
        req.harness
            .clone()
            .unwrap_or_else(|| config.harness.clone()),
        "--auth".into(),
        req.auth.clone().unwrap_or_else(|| config.auth.clone()),
        "--backend".into(),
        config.backend.clone(),
    ];
    if let Some(p) = &req.provider {
        a.extend(["--provider".to_string(), p.clone()]);
    }
    if !req.work {
        if let Some(p) = &req.prompt {
            a.extend(["--prompt".to_string(), p.clone()]);
        }
        if !req.args.is_empty() {
            a.push("--".into());
            a.extend(req.args.iter().cloned());
        }
    }
    a
}

fn title_for(req: &NewSession, config: &Config) -> String {
    let h = req
        .harness
        .clone()
        .unwrap_or_else(|| config.harness.clone());
    match (&req.prompt, req.work) {
        (_, true) => format!("{h}: next task"),
        (Some(p), _) => format!("{h}: {}", p.chars().take(60).collect::<String>()),
        (None, _) => h,
    }
}

impl App {
    fn start(&self, req: &NewSession) -> Result<Arc<Session>, Error> {
        let argv = launch_argv(&self.config, req);
        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows: 40,
                cols: 120,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| Error::Session(e.to_string()))?;
        let mut cmd = CommandBuilder::new(&argv[0]);
        cmd.args(&argv[1..]);
        cmd.cwd(&self.config.workdir);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| Error::Session(e.to_string()))?;
        drop(pair.slave);
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| Error::Session(e.to_string()))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| Error::Session(e.to_string()))?;
        let (live, _) = broadcast::channel(1024);
        let id = new_id();
        let session = Arc::new(Session {
            info: Mutex::new(SessionInfo {
                id: id.clone(),
                title: title_for(req, &self.config),
                alive: true,
                exit: None,
                started: now(),
            }),
            writer: Mutex::new(writer),
            master: Mutex::new(pair.master),
            child: Mutex::new(child),
            ring: Mutex::new(VecDeque::with_capacity(64 * 1024)),
            live,
            alive: AtomicBool::new(true),
        });
        lock(&self.sessions).insert(id, session.clone());
        spawn_reader(session.clone(), reader);
        Ok(session)
    }

    fn get(&self, id: &str) -> Option<Arc<Session>> {
        lock(&self.sessions).get(id).cloned()
    }
}

/// Pump the pseudo-terminal into the ring and to the attached browsers, then record the
/// exit when the launcher is done.
fn spawn_reader(session: Arc<Session>, mut reader: Box<dyn Read + Send>) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 16 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let chunk = Bytes::copy_from_slice(&buf[..n]);
                    {
                        let mut ring = lock(&session.ring);
                        ring.extend(chunk.iter());
                        while ring.len() > RING_BYTES {
                            ring.pop_front();
                        }
                    }
                    let _ = session.live.send(chunk);
                }
            }
        }
        let code = lock(&session.child)
            .wait()
            .ok()
            .map(|s| i32::try_from(s.exit_code()).unwrap_or(1));
        lock(&session.info).exit = code;
        session.alive.store(false, Ordering::SeqCst);
        let _ = session.live.send(Bytes::new());
    });
}

/// The incoming side of the websocket: keystrokes and resizes.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Incoming {
    Input { data: String },
    Resize { cols: u16, rows: u16 },
}

async fn ws_session(session: Arc<Session>, socket: WebSocket) {
    let (mut tx, mut rx) = socket.split();
    let mut live = session.live.subscribe();
    let replay: Vec<u8> = lock(&session.ring).iter().copied().collect();
    if tx.send(Message::Binary(Bytes::from(replay))).await.is_err() {
        return;
    }
    if !session.alive.load(Ordering::SeqCst) {
        let info = session.snapshot();
        let _ = tx
            .send(Message::Text(
                json!({ "type": "exit", "code": info.exit })
                    .to_string()
                    .into(),
            ))
            .await;
    }
    let s2 = session.clone();
    let incoming = tokio::spawn(async move {
        while let Some(Ok(msg)) = rx.next().await {
            match msg {
                Message::Text(t) => match serde_json::from_str::<Incoming>(&t) {
                    Ok(Incoming::Input { data }) => {
                        let _ = lock(&s2.writer).write_all(data.as_bytes());
                        let _ = lock(&s2.writer).flush();
                    }
                    Ok(Incoming::Resize { cols, rows }) => {
                        let _ = lock(&s2.master).resize(PtySize {
                            rows,
                            cols,
                            pixel_width: 0,
                            pixel_height: 0,
                        });
                    }
                    Err(_) => {}
                },
                Message::Binary(b) => {
                    let _ = lock(&s2.writer).write_all(&b);
                    let _ = lock(&s2.writer).flush();
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    });
    loop {
        match live.recv().await {
            Ok(chunk) if chunk.is_empty() => {
                let info = session.snapshot();
                let _ = tx
                    .send(Message::Text(
                        json!({ "type": "exit", "code": info.exit })
                            .to_string()
                            .into(),
                    ))
                    .await;
                break;
            }
            Ok(chunk) => {
                if tx.send(Message::Binary(chunk)).await.is_err() {
                    break;
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) => {
                // a slow browser: resend the ring rather than lose bytes
                let replay: Vec<u8> = lock(&session.ring).iter().copied().collect();
                if tx.send(Message::Binary(Bytes::from(replay))).await.is_err() {
                    break;
                }
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
    incoming.abort();
}

async fn index() -> Html<&'static str> {
    Html(pages::INDEX)
}

async fn vendor(Path(name): Path<String>) -> Response {
    match pages::vendor(&name) {
        Some((content_type, bytes)) => (
            [
                (axum::http::header::CONTENT_TYPE, content_type),
                (
                    axum::http::header::CACHE_CONTROL,
                    "public, max-age=31536000, immutable",
                ),
            ],
            bytes,
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "no such file").into_response(),
    }
}

async fn session_page(State(app): State<App>, Path(id): Path<String>) -> Response {
    match app.get(&id) {
        Some(_) => Html(pages::SESSION).into_response(),
        None => (StatusCode::NOT_FOUND, "no such session").into_response(),
    }
}

async fn ws(State(app): State<App>, Path(id): Path<String>, up: WebSocketUpgrade) -> Response {
    match app.get(&id) {
        Some(s) => up.on_upgrade(move |socket| ws_session(s, socket)),
        None => (StatusCode::NOT_FOUND, "no such session").into_response(),
    }
}

async fn list_sessions(State(app): State<App>) -> Json<Vec<SessionInfo>> {
    let v: Vec<SessionInfo> = lock(&app.sessions).values().map(|s| s.snapshot()).collect();
    Json(v)
}

async fn create_session(State(app): State<App>, Json(req): Json<NewSession>) -> Response {
    match app.start(&req) {
        Ok(s) => Json(s.snapshot()).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn session_info(State(app): State<App>, Path(id): Path<String>) -> Response {
    match app.get(&id) {
        Some(s) => Json(s.snapshot()).into_response(),
        None => (StatusCode::NOT_FOUND, "no such session").into_response(),
    }
}

async fn kill_session(State(app): State<App>, Path(id): Path<String>) -> Response {
    match app.get(&id) {
        Some(s) => {
            let _ = lock(&s.child).kill();
            StatusCode::NO_CONTENT.into_response()
        }
        None => (StatusCode::NOT_FOUND, "no such session").into_response(),
    }
}

/// Recent entries, newest first: what the pane watches.
async fn entries(State(app): State<App>) -> Response {
    match app.memory.list(None, None, 30).await {
        Ok(hits) => Json(
            hits.iter()
                .map(|h| {
                    json!({
                        "id": h.id, "title": h.title, "type": h.entry_type.to_string(),
                        "summary": h.summary, "classification": h.classification.to_string(),
                    })
                })
                .collect::<Vec<Value>>(),
        )
        .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// One entry in full, for the pane.
async fn entry(State(app): State<App>, Path(id): Path<String>) -> Response {
    match app.memory.get(&id).await {
        Ok(e) => Json(json!({
            "id": e.front.id, "title": e.title(), "type": e.front.entry_type.to_string(),
            "author": e.front.author, "classification": e.front.classification.to_string(),
            "trust_tier": e.front.trust_tier.to_string(), "body": e.body,
        }))
        .into_response(),
        Err(e) => (StatusCode::NOT_FOUND, e.to_string()).into_response(),
    }
}

/// The browser reader.
async fn reader() -> Html<&'static str> {
    Html(pages::READER)
}

/// The reader's wasm and glue. They change with the binary, under the same names, so
/// the browser revalidates them rather than keeping a copy.
async fn reader_asset(Path(name): Path<String>) -> Response {
    match pages::reader_asset(&name) {
        Some((content_type, bytes)) => (
            [
                (axum::http::header::CONTENT_TYPE, content_type),
                (axum::http::header::CACHE_CONTROL, "no-cache"),
            ],
            bytes,
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "no such asset").into_response(),
    }
}

/// The domains the record grants, for the reader's picker.
async fn domains(State(app): State<App>) -> Json<Vec<String>> {
    Json(app.memory.domains())
}

/// A bundle file name: letters, digits, dot, underscore and hyphen, not starting with a
/// dot. Anything else is not a name and is never joined to a path.
fn plain_file_name(file: &str) -> bool {
    !file.is_empty()
        && !file.starts_with('.')
        && file
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
}

/// A status and the line that explains it.
type Refusal = (StatusCode, String);

/// What every bundle request passes before a path is built: the label parses, the file
/// is a plain name, the domain is granted and the label is within the handling. The
/// error is the refusal.
fn admit_bundle_request(
    app: &App,
    domain: &str,
    label: &str,
    file: &str,
) -> Result<dm_core::Label, Refusal> {
    let Ok(label) = label.parse::<dm_core::Label>() else {
        return Err((
            StatusCode::BAD_REQUEST,
            "label is public, internal or restricted".into(),
        ));
    };
    if !plain_file_name(file) {
        return Err((StatusCode::NOT_FOUND, "no such file".into()));
    }
    if !app.memory.domains().iter().any(|d| d == domain) {
        return Err((
            StatusCode::FORBIDDEN,
            format!("domain {domain} is not granted"),
        ));
    }
    if label > app.memory.grants().handling {
        return Err((
            StatusCode::FORBIDDEN,
            format!(
                "{label} is above the handling {}",
                app.memory.grants().handling
            ),
        ));
    }
    Ok(label)
}

/// The refusal for an export that did not happen: a denial is the client's, anything
/// else is the server's.
fn export_refusal(e: dm_server::Error) -> Refusal {
    let status = match e {
        dm_server::Error::Denied(_)
        | dm_server::Error::NotFound(_)
        | dm_server::Error::Bundle(dm_server::bundle::Error::Invalid(_)) => StatusCode::FORBIDDEN,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, e.to_string())
}

/// Export the domain afresh into a sibling directory and swap it in, so `dir` holds
/// its last export and nothing that was there before. One export runs at a time.
async fn refresh_bundle(
    app: &App,
    domain: &str,
    label: dm_core::Label,
    dir: &std::path::Path,
) -> Result<(), Refusal> {
    let _one_at_a_time = app.exports.lock().await;
    let sibling = |suffix: &str| {
        let mut name = dir.file_name().unwrap_or_default().to_os_string();
        name.push(suffix);
        dir.with_file_name(name)
    };
    let (fresh, old) = (sibling(".new"), sibling(".old"));
    let _ = tokio::fs::remove_dir_all(&fresh).await;
    let _ = tokio::fs::remove_dir_all(&old).await;
    app.memory
        .export(Some(domain), label, &fresh)
        .await
        .map_err(export_refusal)?;
    let failed = |e: std::io::Error| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    if tokio::fs::metadata(dir).await.is_ok() {
        tokio::fs::rename(dir, &old).await.map_err(failed)?;
    }
    tokio::fs::rename(&fresh, dir).await.map_err(failed)?;
    let _ = tokio::fs::remove_dir_all(&old).await;
    Ok(())
}

/// Whether the directory's current manifest lists `file`; the manifest lists itself.
async fn listed(dir: &std::path::Path, file: &str) -> bool {
    if file == "manifest.yaml" {
        return true;
    }
    let Ok(text) = tokio::fs::read_to_string(dir.join("manifest.yaml")).await else {
        return false;
    };
    dm_core::manifest::Manifest::from_yaml(&text)
        .map(|m| m.entries.iter().any(|e| e.path == file))
        .unwrap_or(false)
}

/// A file of a domain's bundle. Every request is checked against the grants before a
/// path is built. Asking for the manifest exports the domain afresh at that label,
/// under the owner's filter and the label bound export always applies; the entries
/// are then served from that export, and only the ones its manifest lists.
async fn bundle_file(
    State(app): State<App>,
    Path((domain, label, file)): Path<(String, String, String)>,
) -> Response {
    let label = match admit_bundle_request(&app, &domain, &label, &file) {
        Ok(label) => label,
        Err(refused) => return refused.into_response(),
    };
    let dir = dm_server::bundle::bundle_dir(&app.config.root, &domain, label);
    if file == "manifest.yaml" {
        if let Err(failed) = refresh_bundle(&app, &domain, label, &dir).await {
            return failed.into_response();
        }
    } else if !listed(&dir, &file).await {
        return (
            StatusCode::NOT_FOUND,
            "no such file; load manifest.yaml first",
        )
            .into_response();
    }
    let content_type = if file.ends_with(".yaml") {
        "application/yaml"
    } else {
        "text/markdown; charset=utf-8"
    };
    match tokio::fs::read(dir.join(&file)).await {
        Ok(bytes) => (
            [
                (axum::http::header::CONTENT_TYPE, content_type),
                (axum::http::header::CACHE_CONTROL, "no-store"),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "no such file").into_response(),
    }
}

/// The router, for serving or for tests.
///
/// # Errors
///
/// When the record cannot be opened.
pub fn router(config: Config) -> Result<Router, Error> {
    let memory = Arc::new(Memory::open(&config.root)?);
    let app = App {
        config,
        memory,
        sessions: Arc::new(Mutex::new(BTreeMap::new())),
        exports: Arc::new(tokio::sync::Mutex::new(())),
    };
    Ok(Router::new()
        .route("/", get(index))
        .route("/vendor/{name}", get(vendor))
        .route("/s/{id}", get(session_page))
        .route("/ws/{id}", get(ws))
        .route("/api/sessions", get(list_sessions).post(create_session))
        .route("/api/sessions/{id}", get(session_info).delete(kill_session))
        .route("/api/entries", get(entries))
        .route("/api/entry/{id}", get(entry))
        .route("/api/domains", get(domains))
        .route("/reader", get(reader))
        .route("/reader/{name}", get(reader_asset))
        .route("/bundle/{domain}/{label}/{file}", get(bundle_file))
        .with_state(app))
}

/// Serve on `listen` until the process ends.
///
/// # Errors
///
/// When the address cannot be bound or the record cannot be opened.
pub async fn serve(config: Config, listen: &str) -> Result<(), Error> {
    let listener = tokio::net::TcpListener::bind(listen).await?;
    eprintln!(
        "memory: web terminal on http://{} · record {} · sessions start in {}",
        listener.local_addr()?,
        config.root.display(),
        config.workdir.display()
    );
    let app = router(config)?;
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(dir: &std::path::Path, command: &[&str]) -> Config {
        Config {
            root: dir.join("memory"),
            memory_binary: "memory".into(),
            workdir: dir.to_path_buf(),
            harness: "claude".into(),
            auth: "subscription".into(),
            backend: "auto".into(),
            command_override: Some(command.iter().map(|s| (*s).to_string()).collect()),
        }
    }

    #[test]
    fn the_launcher_command_line_follows_the_request() {
        let mut c = config(std::path::Path::new("/tmp/x"), &[]);
        c.command_override = None;
        let a = launch_argv(
            &c,
            &NewSession {
                harness: Some("codex".into()),
                auth: None,
                provider: Some("xai".into()),
                prompt: Some("hi".into()),
                work: false,
                args: vec!["-v".into()],
            },
        );
        assert_eq!(
            a.join(" "),
            format!(
                "memory --root {} launch --harness codex --auth subscription --backend auto --provider xai --prompt hi -- -v",
                c.root.display()
            )
        );
        let a = launch_argv(
            &c,
            &NewSession {
                work: true,
                ..NewSession::default()
            },
        );
        assert!(
            a.join(" ")
                .ends_with("work --harness claude --auth subscription --backend auto"),
            "{a:?}"
        );
        assert_eq!(
            title_for(
                &NewSession {
                    work: true,
                    ..NewSession::default()
                },
                &c
            ),
            "claude: next task"
        );
    }

    async fn start(
        dir: &std::path::Path,
        command: &[&str],
    ) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        Memory::init(&dir.join("memory"), "human:test:me", "dom-me").unwrap();
        let app = router(config(dir, command)).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let h = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (addr, h)
    }

    /// A small HTTP/1.1 client over a socket: enough for the tests, and no TLS stack.
    async fn http(
        addr: std::net::SocketAddr,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> (u16, String) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        let body = body.unwrap_or("");
        let req = format!(
            "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        s.write_all(req.as_bytes()).await.unwrap();
        let mut out = Vec::new();
        s.read_to_end(&mut out).await.unwrap();
        let text = String::from_utf8_lossy(&out).into_owned();
        let status: u16 = text
            .split(' ')
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        let body = text
            .split_once("\r\n\r\n")
            .map(|(_, b)| b.to_string())
            .unwrap_or_default();
        (status, body)
    }

    async fn get_json(addr: std::net::SocketAddr, path: &str) -> (u16, Value) {
        let (status, body) = http(addr, "GET", path, None).await;
        (status, serde_json::from_str(&body).unwrap_or(Value::Null))
    }

    #[cfg(unix)]
    async fn ws_connect(
        addr: std::net::SocketAddr,
        id: &str,
    ) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>
    {
        let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws/{id}"))
            .await
            .unwrap();
        ws
    }

    #[cfg(unix)]
    async fn read_until(
        ws: &mut (impl StreamExt<
            Item = Result<
                tokio_tungstenite::tungstenite::Message,
                tokio_tungstenite::tungstenite::Error,
            >,
        > + Unpin),
        needle: &str,
    ) -> String {
        let mut got = String::new();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        while !got.contains(needle) && tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(std::time::Duration::from_secs(1), ws.next()).await {
                Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Binary(b)))) => {
                    got.push_str(&String::from_utf8_lossy(&b))
                }
                Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Text(t)))) => {
                    got.push_str(&format!("<{t}>"))
                }
                Ok(Some(Ok(_))) => {}
                Ok(Some(Err(_)) | None) => break,
                Err(_) => {}
            }
        }
        got
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn a_session_streams_echoes_and_replays_its_scrollback() {
        let dir = tempfile::tempdir().unwrap();
        let (addr, _h) = start(
            dir.path(),
            &[
                "sh",
                "-c",
                "echo hello-from-pty; read x; echo got-$x; exit 3",
            ],
        )
        .await;
        let (status, body) = http(
            addr,
            "POST",
            "/api/sessions",
            Some(&json!({ "prompt": "say hi" }).to_string()),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let created: SessionInfo = serde_json::from_str(&body).unwrap();
        assert!(created.alive);
        assert_eq!(created.title, "claude: say hi");

        let mut ws = ws_connect(addr, &created.id).await;
        let got = read_until(&mut ws, "hello-from-pty").await;
        assert!(got.contains("hello-from-pty"), "{got}");
        ws.send(tokio_tungstenite::tungstenite::Message::Text(
            json!({"type":"resize","cols":100,"rows":30})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
        ws.send(tokio_tungstenite::tungstenite::Message::Text(
            json!({"type":"input","data":"pong\n"}).to_string().into(),
        ))
        .await
        .unwrap();
        let got = read_until(&mut ws, "exit").await;
        assert!(got.contains("got-pong"), "{got}");
        assert!(
            got.contains(r#"{"code":3,"type":"exit"}"#)
                || got.contains(r#"{"type":"exit","code":3}"#),
            "{got}"
        );

        a_late_browser_sees_everything(addr, &created.id).await;
    }

    #[cfg(unix)]
    async fn a_late_browser_sees_everything(addr: std::net::SocketAddr, id: &str) {
        let created_id = id;
        // a second browser, after the fact, gets the whole scrollback and the exit
        let mut ws2 = ws_connect(addr, created_id).await;
        let got = read_until(&mut ws2, "exit").await;
        assert!(
            got.contains("hello-from-pty") && got.contains("got-pong"),
            "replayed: {got}"
        );
        let (_, info) = get_json(addr, &format!("/api/sessions/{created_id}")).await;
        let info: SessionInfo = serde_json::from_value(info).unwrap();
        assert!(!info.alive);
        assert_eq!(info.exit, Some(3));
        let (_, list) = get_json(addr, "/api/sessions").await;
        let list: Vec<SessionInfo> = serde_json::from_value(list).unwrap();
        assert_eq!(list.len(), 1);
        let (page, _) = http(addr, "GET", &format!("/s/{created_id}"), None).await;
        assert_eq!(page, 200);
        let (missing, _) = http(addr, "GET", "/s/nope", None).await;
        assert_eq!(missing, 404);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn a_session_can_be_killed_and_unknown_ids_are_404() {
        let dir = tempfile::tempdir().unwrap();
        let (addr, _h) = start(dir.path(), &["sh", "-c", "sleep 30"]).await;
        let (status, body) = http(addr, "POST", "/api/sessions", Some("{}")).await;
        assert_eq!(status, 200, "{body}");
        let created: SessionInfo = serde_json::from_str(&body).unwrap();
        assert_eq!(created.title, "claude");
        let (status, _) = http(
            addr,
            "DELETE",
            &format!("/api/sessions/{}", created.id),
            None,
        )
        .await;
        assert_eq!(status, 204);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let (_, info) = get_json(addr, &format!("/api/sessions/{}", created.id)).await;
            if info["alive"] == false || tokio::time::Instant::now() > deadline {
                assert_eq!(info["alive"], false, "{info}");
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let (status, _) = http(addr, "GET", "/api/sessions/nope", None).await;
        assert_eq!(status, 404);
        let (status, _) = http(addr, "DELETE", "/api/sessions/nope", None).await;
        assert_eq!(status, 404);
        // without the upgrade headers the websocket route is a bad request, not a page
        let (status, _) = http(addr, "GET", "/ws/nope", None).await;
        assert_eq!(status, 400);
    }

    #[tokio::test]
    async fn a_launcher_that_cannot_start_is_a_500() {
        let dir = tempfile::tempdir().unwrap();
        let (addr, _h) = start(dir.path(), &["/definitely/not/a/program"]).await;
        let (status, body) = http(addr, "POST", "/api/sessions", Some("{}")).await;
        assert_eq!(status, 500, "{body}");
        assert!(body.contains("session:"), "{body}");
        let (status, _) = http(addr, "GET", "/", None).await;
        assert_eq!(status, 200);
    }

    #[tokio::test]
    async fn the_libraries_are_served_from_the_binary() {
        let dir = tempfile::tempdir().unwrap();
        let (addr, _h) = start(dir.path(), &["true"]).await;
        for (name, needle) in [
            ("xterm.min.js", "Terminal"),
            ("xterm.min.css", ".xterm"),
            ("addon-fit.min.js", "FitAddon"),
            ("marked.min.js", "marked"),
            ("mermaid.min.js", "mermaid"),
        ] {
            let (status, body) = http(addr, "GET", &format!("/vendor/{name}"), None).await;
            assert_eq!(status, 200, "{name}");
            assert!(body.contains(needle), "{name} lacks {needle}");
        }
        let (status, _) = http(addr, "GET", "/vendor/evil.js", None).await;
        assert_eq!(status, 404);
        let (_, page) = http(addr, "GET", "/", None).await;
        assert!(!page.contains("cdn."), "no CDN on the index page");
        assert!(
            !pages::SESSION.contains("cdn."),
            "no CDN on the session page"
        );
    }

    #[tokio::test]
    async fn the_pane_reads_entries_from_the_record() {
        let dir = tempfile::tempdir().unwrap();
        let (addr, _h) = start(dir.path(), &["true"]).await;
        let m = Memory::open(&dir.path().join("memory"))
            .unwrap()
            .with_author("agent:test");
        let w = m
            .write(dm_server::WriteRequest {
                body: "# Plan\n\n```mermaid\ngraph TD; A-->B;\n```\n".into(),
                ..dm_server::WriteRequest::default()
            })
            .await
            .unwrap();
        let (_, list) = get_json(addr, "/api/entries").await;
        let list: Vec<Value> = serde_json::from_value(list).unwrap();
        assert!(list.iter().any(|e| e["id"] == w.id), "{list:?}");
        let (_, e) = get_json(addr, &format!("/api/entry/{}", w.id)).await;
        assert_eq!(e["title"], "Plan");
        assert!(e["body"].as_str().unwrap().contains("mermaid"));
        let (missing, _) = http(addr, "GET", "/api/entry/mem-me-nothere", None).await;
        assert_eq!(missing, 404);
    }

    #[tokio::test]
    async fn the_reader_its_assets_and_a_bundle_are_served() {
        let dir = tempfile::tempdir().unwrap();
        let (addr, _h) = start(dir.path(), &["true"]).await;
        let m = Memory::open(&dir.path().join("memory")).unwrap();
        let w = m
            .write(dm_server::WriteRequest {
                title: Some("Retry policy".into()),
                body: "Operation R retries with backoff.\n".into(),
                ..dm_server::WriteRequest::default()
            })
            .await
            .unwrap();
        let (status, page) = http(addr, "GET", "/reader", None).await;
        assert_eq!(status, 200);
        assert!(page.contains("memory reader") && page.contains("reader/dm_reader.js"));
        let (status, glue) = http(addr, "GET", "/reader/dm_reader.js", None).await;
        assert_eq!(status, 200);
        assert!(glue.contains("parse_entry"));
        let (status, _) = http(addr, "GET", "/reader/dm_reader_bg.wasm", None).await;
        assert_eq!(status, 200);
        let (status, _) = http(addr, "GET", "/reader/other.js", None).await;
        assert_eq!(status, 404);
        for v in ["sqlite3.mjs", "sqlite3.wasm"] {
            let (status, _) = http(addr, "GET", &format!("/vendor/{v}"), None).await;
            assert_eq!(status, 200, "{v}");
        }
        let (status, domains) = get_json(addr, "/api/domains").await;
        assert_eq!((status, domains), (200, serde_json::json!(["dom-me"])));

        let (status, manifest) =
            http(addr, "GET", "/bundle/dom-me/internal/manifest.yaml", None).await;
        assert_eq!(status, 200, "{manifest}");
        assert!(manifest.contains("durable-memory/bundle/0") && manifest.contains(&w.id));
        let (status, text) = http(
            addr,
            "GET",
            &format!("/bundle/dom-me/internal/{}.md", w.id),
            None,
        )
        .await;
        assert_eq!(status, 200);
        assert!(
            text.starts_with("---\n") && text.contains("backoff"),
            "{text}"
        );
        let (status, public) = http(addr, "GET", "/bundle/dom-me/public/manifest.yaml", None).await;
        assert_eq!(status, 200);
        assert!(
            !public.contains(&w.id),
            "an internal entry stays out of a public bundle"
        );
        let (status, _) = http(addr, "GET", "/bundle/dom-me/secret/manifest.yaml", None).await;
        assert_eq!(status, 400);
        let (status, _) = http(
            addr,
            "GET",
            "/bundle/dom-me/internal/..%2Fgrants.toml",
            None,
        )
        .await;
        assert_eq!(status, 404, "a path is not a file name");
        let (status, _) = http(addr, "GET", "/bundle/dom-me/internal/.hidden", None).await;
        assert_eq!(status, 404);
        let (status, _) = http(addr, "GET", "/bundle/dom-else/internal/manifest.yaml", None).await;
        assert_eq!(status, 403, "a domain the owner does not hold");
    }

    #[tokio::test]
    async fn a_bundle_serves_only_what_its_manifest_lists_under_the_grants() {
        let dir = tempfile::tempdir().unwrap();
        let (addr, _h) = start(dir.path(), &["true"]).await;
        let root = dir.path().join("memory");
        let m = Memory::open(&root).unwrap();
        let w = m
            .write(dm_server::WriteRequest {
                title: Some("Key rotation".into()),
                body: "Rotate the signing key quarterly.\n".into(),
                classification: Some(dm_core::Label::Restricted),
                ..dm_server::WriteRequest::default()
            })
            .await
            .unwrap();
        // the grants file sits three directories above a bundle
        let (status, body) = http(
            addr,
            "GET",
            "/bundle/dom-me/restricted/..%2F..%2F..%2Fgrants.toml",
            None,
        )
        .await;
        assert_eq!(status, 404);
        assert!(!body.contains("owner ="), "a traversal reached the grants");
        let (status, _) = http(addr, "GET", "/bundle/dom-me/restricted/manifest.yaml", None).await;
        assert_eq!(status, 200);
        let bundle = root.join("state/bundles/dom-me-restricted");
        std::fs::write(
            bundle.join("stray.md"),
            "---\nid: mem-me-stray\n---\nnot exported\n",
        )
        .unwrap();
        let (status, _) = http(addr, "GET", "/bundle/dom-me/restricted/stray.md", None).await;
        assert_eq!(
            status, 404,
            "a file the manifest does not list is not served"
        );
        let (status, _) = http(addr, "GET", "/bundle/dom-me/restricted/C:x.md", None).await;
        assert_eq!(status, 404, "a drive letter is not a file name");
        let (status, _) = http(addr, "GET", "/bundle/dom-else/restricted/x.md", None).await;
        assert_eq!(status, 403, "the domain is checked on every file");
        let file = format!("/bundle/dom-me/restricted/{}.md", w.id);
        let (status, text) = http(addr, "GET", &file, None).await;
        assert_eq!(status, 200);
        assert!(text.contains("quarterly"));

        // the handling drops to internal: a second server over the same root must not
        // serve the restricted export that is still on disk
        let grants = root.join("grants.toml");
        let lowered = std::fs::read_to_string(&grants)
            .unwrap()
            .replace("handling = \"restricted\"", "handling = \"internal\"");
        assert!(lowered.contains("handling = \"internal\""));
        std::fs::write(&grants, lowered).unwrap();
        let app = router(config(dir.path(), &["true"])).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr2 = listener.local_addr().unwrap();
        let _h2 = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let (status, _) = http(addr2, "GET", &file, None).await;
        assert_eq!(
            status, 403,
            "a stale export above the handling is not served"
        );
        let (status, _) = http(
            addr2,
            "GET",
            "/bundle/dom-me/restricted/manifest.yaml",
            None,
        )
        .await;
        assert_eq!(status, 403);

        // an export replaces its directory rather than adding to it
        let (status, _) = http(addr2, "GET", "/bundle/dom-me/internal/manifest.yaml", None).await;
        assert_eq!(status, 200);
        let stray = root.join("state/bundles/dom-me-internal/stray.md");
        std::fs::write(&stray, "x").unwrap();
        let (status, _) = http(addr2, "GET", "/bundle/dom-me/internal/manifest.yaml", None).await;
        assert_eq!(status, 200);
        assert!(
            !stray.exists(),
            "a bundle directory holds only its last export"
        );
    }
}
