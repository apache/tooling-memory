//! A web terminal on the launcher.
//!
//! Each session is a pseudo-terminal running `memory launch`, kept alive by this server
//! whether or not a browser is attached. Its output goes into a ring buffer and to every
//! attached websocket; a browser that connects, or reconnects, gets the ring first, so a
//! refresh or a dropped connection lands in the same place. Beside the terminal a reading
//! pane renders entries from the record as they land. Loopback only, and behind a token:
//! see [`access`].

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Json, Response};
use axum::routing::{get, post};
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

pub mod access;
mod pages;
pub mod state;

pub use access::Access;

/// What can go wrong.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The pseudo-terminal could not be opened or the launcher could not be started.
    #[error("session: {0}")]
    Session(String),
    /// A request the server cannot act on as given: no harness, say.
    #[error("{0}")]
    Request(String),
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
    /// The harness the form preselects, if any; a request names its own.
    pub harness: Option<String>,
    /// Default auth mode.
    pub auth: String,
    /// Default backend.
    pub backend: String,
    /// For tests: run this command verbatim instead of the launcher.
    pub command_override: Option<Vec<String>>,
    /// Who may call: the token, and origins besides the server's own.
    pub access: Access,
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
    /// The model, in the harness's own flag.
    #[serde(default)]
    pub model: Option<String>,
    /// Where a subagent the harness would spawn runs: harness or board.
    #[serde(default)]
    pub subagents: Option<String>,
    /// Non-interactive: a prompt.
    #[serde(default)]
    pub prompt: Option<String>,
    /// Task mode.
    #[serde(default)]
    pub work: bool,
    /// Arguments for the harness.
    #[serde(default)]
    pub args: Vec<String>,
    /// A profile from `profiles.toml`, or a harness's own name; what the request sets
    /// itself wins over it.
    #[serde(default)]
    pub profile: Option<String>,
    /// Where the harness runs: auto, namespaces, image, wslc or none.
    #[serde(default)]
    pub backend: Option<String>,
}

/// The backends a request may name.
pub const BACKENDS: &[&str] = &["auto", "namespaces", "image", "wslc", "none"];

/// Whether `name` may name a profile: lowercase letters, digits, `-` and `_`, starting
/// with a letter, as the launcher's own check has it.
fn profile_name(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
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
    /// What it was started with, for the session's bar: the harness, or the profile
    /// that names one, and whatever the request set itself.
    #[serde(default)]
    pub launch: Launched,
}

/// What a session was started with, as the request said it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Launched {
    /// The profile, when one was named.
    pub profile: Option<String>,
    /// The harness, when the request named one.
    pub harness: Option<String>,
    /// The auth mode, when the request named one.
    pub auth: Option<String>,
    /// The destination, when the request named one.
    pub provider: Option<String>,
    /// The model, when the request named one.
    pub model: Option<String>,
    /// The backend, when the request named one.
    pub backend: Option<String>,
    /// interactive, prompt or work.
    pub mode: String,
}

impl Launched {
    fn of(req: &NewSession) -> Launched {
        let some = |v: &Option<String>| v.clone().filter(|v| !v.trim().is_empty());
        Launched {
            profile: some(&req.profile),
            harness: some(&req.harness),
            auth: some(&req.auth),
            provider: some(&req.provider),
            model: some(&req.model),
            backend: some(&req.backend),
            mode: if req.work {
                "work"
            } else if req.prompt.as_deref().is_some_and(|p| !p.trim().is_empty()) {
                "prompt"
            } else {
                "interactive"
            }
            .into(),
        }
    }
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

/// A session id: unguessable, since a websocket to it is a keyboard on the harness.
fn new_id() -> Result<String, Error> {
    Ok(access::random_hex::<16>()?)
}

/// The harness a request names, or the one the server preselects; a request that
/// names none where the server has none is refused, since there is no default harness.
fn harness_of(config: &Config, req: &NewSession) -> Result<String, String> {
    req.harness
        .clone()
        .filter(|h| !h.trim().is_empty())
        .or_else(|| config.harness.clone())
        .ok_or_else(|| "say which harness: there is no default".to_string())
}

/// The launcher's command line for a request. With a profile, the request's own fields
/// are passed only where it sets them, so the profile's stand where it does not.
fn launch_argv(config: &Config, req: &NewSession) -> Result<Vec<String>, String> {
    if let Some(c) = &config.command_override {
        return Ok(c.clone());
    }
    let set = |v: &Option<String>| {
        v.as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    let profile = set(&req.profile);
    if let Some(p) = &profile {
        if !profile_name(p) {
            return Err(format!("`{p}` is not a profile name"));
        }
    }
    let backend = set(&req.backend);
    if let Some(b) = &backend {
        if !BACKENDS.contains(&b.as_str()) {
            return Err(format!("no backend named {b}: {}", BACKENDS.join(", ")));
        }
    }
    let mut a = vec![
        config.memory_binary.display().to_string(),
        "--root".into(),
        config.root.display().to_string(),
        if req.work { "work" } else { "launch" }.into(),
    ];
    let mut flag = |name: &str, value: Option<String>| {
        if let Some(v) = value {
            a.extend([name.to_string(), v]);
        }
    };
    match profile {
        Some(p) => {
            flag("--profile", Some(p));
            flag("--harness", set(&req.harness));
            flag("--auth", set(&req.auth));
            flag("--backend", backend);
        }
        None => {
            flag("--harness", Some(harness_of(config, req)?));
            flag(
                "--auth",
                Some(set(&req.auth).unwrap_or_else(|| config.auth.clone())),
            );
            flag(
                "--backend",
                Some(backend.unwrap_or_else(|| config.backend.clone())),
            );
        }
    }
    flag("--provider", set(&req.provider));
    flag("--model", set(&req.model));
    flag("--subagents", set(&req.subagents));
    if !req.work {
        if let Some(p) = &req.prompt {
            a.extend(["--prompt".to_string(), p.clone()]);
        }
        if !req.args.is_empty() {
            a.push("--".into());
            a.extend(req.args.iter().cloned());
        }
    }
    Ok(a)
}

fn title_for(req: &NewSession, config: &Config) -> String {
    let named = req.profile.clone().filter(|p| !p.trim().is_empty());
    let mut h = harness_of(config, req)
        .ok()
        .filter(|_| req.harness.is_some() || named.is_none())
        .or(named)
        .unwrap_or_else(|| "?".into());
    if let Some(m) = req.model.as_deref().filter(|m| !m.trim().is_empty()) {
        h = format!("{h} on {}", m.trim());
    }
    match (&req.prompt, req.work) {
        (_, true) => format!("{h}: next task"),
        (Some(p), _) => format!("{h}: {}", p.chars().take(60).collect::<String>()),
        (None, _) => h,
    }
}

/// What the form offers: the harnesses, the auth modes, the destinations each auth mode
/// may name, where subagents may go, and the server's defaults. One list, in Rust, so
/// the page and the launcher agree.
fn options(config: &Config) -> Value {
    json!({
        "harnesses": ["claude", "codex", "gemini", "antigravity", "qwen", "grok", "muse", "pi", "copilot", "opencode"],
        "auth": ["subscription", "key", "gateway"],
        "destinations": {
            "subscription": [],
            "key": ["anthropic", "openai", "xai", "gemini", "dashscope", "meta"],
            "gateway": ["llmao"]
        },
        "subagents": ["harness", "board"],
        "backends": BACKENDS,
        "defaults": { "harness": config.harness, "auth": config.auth, "backend": config.backend }
    })
}

/// The models a destination lists, asked of the launcher itself: `memory models` opens
/// the record, routes as a launch would, and prints what the destination's own model
/// list says. A login mode has no list, since the login is the harness's.
async fn models_via_launcher(
    config: &Config,
    auth: &str,
    provider: Option<&str>,
) -> Result<Value, String> {
    if auth == "subscription" {
        return Ok(
            json!({ "models": [], "note": "a login is the harness's own; name the model, or leave it to the harness" }),
        );
    }
    let mut cmd = tokio::process::Command::new(&config.memory_binary);
    cmd.arg("--root")
        .arg(&config.root)
        .arg("models")
        .arg("--auth")
        .arg(auth);
    if let Some(p) = provider.filter(|p| !p.is_empty()) {
        cmd.arg("--provider").arg(p);
    }
    let out = tokio::time::timeout(std::time::Duration::from_secs(30), cmd.output())
        .await
        .map_err(|_| "the model list did not come back in time".to_string())?
        .map_err(|e| format!("could not run the launcher: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    serde_json::from_slice(&out.stdout).map_err(|e| format!("not a model list: {e}"))
}

/// What the launcher prints as JSON for `args`, asked of it as `models` is: the same
/// binary over the same root, within `secs`.
async fn launcher_json(config: &Config, args: &[&str], secs: u64) -> Result<Value, String> {
    let mut cmd = tokio::process::Command::new(&config.memory_binary);
    cmd.arg("--root")
        .arg(&config.root)
        .args(args)
        .kill_on_drop(true);
    let out = tokio::time::timeout(std::time::Duration::from_secs(secs), cmd.output())
        .await
        .map_err(|_| format!("`{}` did not answer in time", args.join(" ")))?
        .map_err(|e| format!("could not run the launcher: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    serde_json::from_slice(&out.stdout).map_err(|e| format!("not JSON: {e}"))
}

/// `?online=1` asks the gateway too.
#[derive(Debug, Default, Deserialize)]
struct DoctorQuery {
    #[serde(default)]
    online: Option<String>,
}

/// What `amem doctor` says: per harness and auth mode, whether a launch has what it
/// needs. Offline unless asked, since asking the gateway can take seconds.
async fn doctor_api(
    State(app): State<App>,
    axum::extract::Query(q): axum::extract::Query<DoctorQuery>,
) -> Response {
    let online = q.online.as_deref().is_some_and(|v| v == "1" || v == "true");
    let args: &[&str] = if online {
        &["doctor", "--json"]
    } else {
        &["doctor", "--json", "--offline"]
    };
    match launcher_json(&app.config, args, 30).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, e).into_response(),
    }
}

/// The profiles, from `profiles.toml` and the harnesses' own names.
async fn profiles_api(State(app): State<App>) -> Response {
    match launcher_json(&app.config, &["profiles", "--json"], 10).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, e).into_response(),
    }
}

/// What the session's launcher says about itself, for its strip: an empty object until
/// it has said anything, which a session started outside the launcher never does.
async fn session_status(State(app): State<App>, Path(id): Path<String>) -> Response {
    if app.get(&id).is_none() {
        return (StatusCode::NOT_FOUND, "no such session").into_response();
    }
    // the id is one this server made, 32 hex digits, before it is a file name
    let path = app.config.root.join("state/web").join(format!("{id}.json"));
    match tokio::fs::read_to_string(&path).await {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(v) => Json(v).into_response(),
            Err(_) => Json(json!({})).into_response(),
        },
        Err(_) => Json(json!({})).into_response(),
    }
}

impl App {
    fn start(&self, req: &NewSession) -> Result<Arc<Session>, Error> {
        let argv = launch_argv(&self.config, req).map_err(Error::Request)?;
        let id = new_id()?;
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
        // the launcher writes its status under this id, which the strip reads
        cmd.env("AMEM_WEB_SESSION", &id);
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
        let session = Arc::new(Session {
            info: Mutex::new(SessionInfo {
                id: id.clone(),
                title: title_for(req, &self.config),
                alive: true,
                exit: None,
                started: now(),
                launch: Launched::of(req),
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

/// `?nonce=`: that this is a server over a record, and proof that it holds the token,
/// for a launcher deciding whether to start one. The token is never sent to ask.
#[derive(Deserialize)]
struct Hello {
    nonce: String,
}

async fn hello(
    State(app): State<App>,
    axum::extract::Query(q): axum::extract::Query<Hello>,
) -> Response {
    if q.nonce.len() < 16 || q.nonce.len() > 128 {
        return (StatusCode::BAD_REQUEST, "a nonce of 16 to 128 characters").into_response();
    }
    match app.config.access.proof(&q.nonce) {
        Some(proof) => Json(json!({
            "server": "amem",
            "version": env!("CARGO_PKG_VERSION"),
            "proof": proof,
        }))
        .into_response(),
        None => (StatusCode::SERVICE_UNAVAILABLE, "not bound yet").into_response(),
    }
}

/// A pairing code for a browser, for the token's holder, with where sessions start.
async fn pair(State(app): State<App>) -> Response {
    match app.config.access.pairing_code() {
        Ok(code) => Json(json!({
            "code": code,
            "workdir": app.config.workdir.display().to_string(),
        }))
        .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct Redeem {
    code: String,
}

/// A browser's key for a pairing code.
async fn redeem(State(app): State<App>, Json(r): Json<Redeem>) -> Response {
    access::redeem_response(&app.config.access, &r.code)
}

async fn amem_js() -> Response {
    (
        [
            (axum::http::header::CONTENT_TYPE, "text/javascript"),
            (axum::http::header::CACHE_CONTROL, "no-cache"),
        ],
        pages::AMEM_JS,
    )
        .into_response()
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

/// The session page by file name; the session is `?id=` in its address.
async fn session_file() -> Html<&'static str> {
    Html(pages::SESSION)
}

async fn tokens_css() -> Response {
    (
        [
            (axum::http::header::CONTENT_TYPE, "text/css"),
            (axum::http::header::CACHE_CONTROL, "no-cache"),
        ],
        pages::TOKENS,
    )
        .into_response()
}

async fn session_page(State(app): State<App>, Path(id): Path<String>) -> Response {
    match app.get(&id) {
        Some(_) => Html(pages::SESSION).into_response(),
        None => (StatusCode::NOT_FOUND, "no such session").into_response(),
    }
}

async fn ws(State(app): State<App>, Path(id): Path<String>, up: WebSocketUpgrade) -> Response {
    match app.get(&id) {
        // a page offers its key as a second subprotocol; the answer names the first
        Some(s) => up
            .protocols([access::PROTOCOL])
            .on_upgrade(move |socket| ws_session(s, socket)),
        None => (StatusCode::NOT_FOUND, "no such session").into_response(),
    }
}

async fn options_api(State(app): State<App>) -> Json<Value> {
    Json(options(&app.config))
}

/// `?auth=<mode>&provider=<destination>`.
#[derive(Debug, Default, Deserialize)]
struct ModelsQuery {
    #[serde(default)]
    auth: Option<String>,
    #[serde(default)]
    provider: Option<String>,
}

async fn models_api(
    State(app): State<App>,
    axum::extract::Query(q): axum::extract::Query<ModelsQuery>,
) -> Response {
    let auth = q.auth.clone().unwrap_or_else(|| app.config.auth.clone());
    match models_via_launcher(&app.config, &auth, q.provider.as_deref()).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, e).into_response(),
    }
}

async fn list_sessions(State(app): State<App>) -> Json<Vec<SessionInfo>> {
    let v: Vec<SessionInfo> = lock(&app.sessions).values().map(|s| s.snapshot()).collect();
    Json(v)
}

async fn create_session(State(app): State<App>, Json(req): Json<NewSession>) -> Response {
    match app.start(&req) {
        Ok(s) => Json(s.snapshot()).into_response(),
        Err(Error::Request(why)) => (StatusCode::BAD_REQUEST, why).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// What the session has printed so far, the ring as text: what a browser replays, for
/// a script or a test that has no websocket.
async fn scrollback(State(app): State<App>, Path(id): Path<String>) -> Response {
    match app.get(&id) {
        Some(s) => {
            let bytes: Vec<u8> = lock(&s.ring).iter().copied().collect();
            (
                [(
                    axum::http::header::CONTENT_TYPE,
                    "text/plain; charset=utf-8",
                )],
                String::from_utf8_lossy(&bytes).into_owned(),
            )
                .into_response()
        }
        None => (StatusCode::NOT_FOUND, "no such session").into_response(),
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

/// The board: tasks by status, each under the task it was filed from.
async fn board() -> Html<&'static str> {
    Html(pages::BOARD)
}

fn task_error(e: &dm_server::Error) -> Response {
    let status = match e {
        dm_server::Error::NotFound(_) => StatusCode::NOT_FOUND,
        dm_server::Error::Denied(_) => StatusCode::FORBIDDEN,
        dm_server::Error::Ledger(_) => StatusCode::CONFLICT,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, e.to_string()).into_response()
}

/// Every task of the domain, whatever its status, newest first within a status, with
/// the domain named: what the board lists and nests by origin.
async fn tasks(State(app): State<App>) -> Response {
    use dm_ledger::Status;
    let domain = match app.memory.resolve_domain(None) {
        Ok(d) => d,
        Err(e) => return task_error(&e),
    };
    let mut all = Vec::new();
    for status in [
        Status::Claimed,
        Status::Admitted,
        Status::Proposed,
        Status::Completed,
        Status::Invalidated,
    ] {
        match app.memory.tasks(Some(&domain), status).await {
            Ok(mut t) => all.append(&mut t),
            Err(e) => return task_error(&e),
        }
    }
    Json(json!({ "domain": domain, "tasks": all })).into_response()
}

/// A task filed from the board, by the person.
#[derive(Debug, Default, Deserialize)]
struct FileTask {
    title: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    requirements: Vec<String>,
    #[serde(default)]
    priority: Option<i64>,
}

async fn file_task(State(app): State<App>, Json(req): Json<FileTask>) -> Response {
    if req.title.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "a task needs a title").into_response();
    }
    let spec = dm_ledger::TaskSpec {
        domain: String::new(),
        title: req.title.trim().to_string(),
        body: req.body,
        requirements: req.requirements,
        depends_on: Vec::new(),
        origin: None,
        upstream_event: None,
        created_by: String::new(),
        classification: dm_core::Label::Public,
        priority: req.priority.unwrap_or(0),
    };
    match app.memory.create_task(spec).await {
        Ok(c) => Json(json!({ "task": c.task, "deduplicated": c.deduplicated })).into_response(),
        Err(e) => task_error(&e),
    }
}

async fn task_tree(State(app): State<App>, Path(id): Path<String>) -> Response {
    match app.memory.task_tree(&id).await {
        Ok(t) => Json(json!(t)).into_response(),
        Err(e) => task_error(&e),
    }
}

async fn admit_task(State(app): State<App>, Path(id): Path<String>) -> Response {
    match app.memory.admit(&id).await {
        Ok(t) => Json(json!({ "task": t })).into_response(),
        Err(e) => task_error(&e),
    }
}

/// The person claims a task, for half an hour at a time; the board's own heartbeat is
/// the person coming back to complete it.
async fn claim_task(State(app): State<App>, Path(id): Path<String>) -> Response {
    match app.memory.claim(&id, time::Duration::minutes(30)).await {
        Ok(c) => Json(json!({ "task": c.task })).into_response(),
        Err(e) => task_error(&e),
    }
}

#[derive(Debug, Default, Deserialize)]
struct Handoff {
    #[serde(default)]
    handoff: String,
}

/// Complete a task the person holds: the handoff text becomes a handoff entry, and the
/// task completes with it, as a harness's run would.
async fn complete_task(
    State(app): State<App>,
    Path(id): Path<String>,
    Json(h): Json<Handoff>,
) -> Response {
    if h.handoff.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "a handoff needs a body").into_response();
    }
    let task = match app.memory.get_task(&id).await {
        Ok(t) => t,
        Err(e) => return task_error(&e),
    };
    let written = app
        .memory
        .write(dm_server::WriteRequest {
            domain: Some(task.spec.domain.clone()),
            entry_type: dm_core::EntryType::Handoff,
            title: Some(format!("Handoff: {}", task.spec.title)),
            body: h.handoff,
            ..dm_server::WriteRequest::default()
        })
        .await;
    let written = match written {
        Ok(w) => w,
        Err(e) => return task_error(&e),
    };
    match app.memory.complete(&id, &written.id).await {
        Ok(t) => Json(json!({ "task": t, "handoff": written.id })).into_response(),
        Err(e) => task_error(&e),
    }
}

#[derive(Debug, Default, Deserialize)]
struct Reason {
    #[serde(default)]
    reason: Option<String>,
}

async fn invalidate_task(
    State(app): State<App>,
    Path(id): Path<String>,
    Json(r): Json<Reason>,
) -> Response {
    let reason = r.reason.unwrap_or_else(|| "closed from the board".into());
    match app.memory.invalidate(&id, &reason, None).await {
        Ok(t) => Json(json!({ "task": t })).into_response(),
        Err(e) => task_error(&e),
    }
}

async fn retry_task(State(app): State<App>, Path(id): Path<String>) -> Response {
    match app.memory.retry_task(&id).await {
        Ok(t) => Json(json!({ "task": t })).into_response(),
        Err(e) => task_error(&e),
    }
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
    // the browser is the owner, a person: a task addressed to a person, one that
    // requires `human`, is one the board may claim
    let memory = Arc::new(Memory::open(&config.root)?.with_manifest(vec!["human".into()]));
    let access = config.access.clone();
    let app = App {
        config,
        memory,
        sessions: Arc::new(Mutex::new(BTreeMap::new())),
        exports: Arc::new(tokio::sync::Mutex::new(())),
    };
    Ok(Router::new()
        .route("/", get(index))
        .route("/amem.js", get(amem_js))
        .route("/tokens.css", get(tokens_css))
        // the pages by file name too: a hosted copy has only files, and the pages link
        // each other relatively so the same links work from both
        .route("/index.html", get(index))
        .route("/session.html", get(session_file))
        .route("/board.html", get(board))
        .route("/reader.html", get(reader))
        .route("/api/hello", get(hello))
        .route("/api/pair", axum::routing::post(pair))
        .route("/api/pair/redeem", axum::routing::post(redeem))
        .route("/vendor/{name}", get(vendor))
        .route("/s/{id}", get(session_page))
        .route("/ws/{id}", get(ws))
        .route("/api/options", get(options_api))
        .route("/api/models", get(models_api))
        .route("/api/sessions", get(list_sessions).post(create_session))
        .route("/api/sessions/{id}", get(session_info).delete(kill_session))
        .route("/api/sessions/{id}/scrollback", get(scrollback))
        .route("/api/sessions/{id}/status", get(session_status))
        .route("/api/profiles", get(profiles_api))
        .route("/api/doctor", get(doctor_api))
        .route("/api/entries", get(entries))
        .route("/api/entry/{id}", get(entry))
        .route("/api/domains", get(domains))
        .route("/board", get(board))
        .route("/api/tasks", get(tasks).post(file_task))
        .route("/api/tasks/{id}/tree", get(task_tree))
        .route("/api/tasks/{id}/admit", post(admit_task))
        .route("/api/tasks/{id}/claim", post(claim_task))
        .route("/api/tasks/{id}/complete", post(complete_task))
        .route("/api/tasks/{id}/invalidate", post(invalidate_task))
        .route("/api/tasks/{id}/retry", post(retry_task))
        .route("/reader", get(reader))
        .route("/reader/{name}", get(reader_asset))
        .route("/bundle/{domain}/{label}/{file}", get(bundle_file))
        .with_state(app)
        .layer(axum::middleware::from_fn_with_state(access, access::gate)))
}

/// A listener on `listen`, which must be a loopback address: the server has no identity
/// in front of it but the token, and a token on the network is a password in the clear.
///
/// # Errors
///
/// When the address is not loopback or cannot be bound.
pub async fn bind(listen: &str) -> Result<tokio::net::TcpListener, Error> {
    let listener = tokio::net::TcpListener::bind(listen).await?;
    if !listener.local_addr()?.ip().is_loopback() {
        return Err(Error::Request(format!(
            "{listen} is not a loopback address; the web server listens on 127.0.0.1 or ::1 only"
        )));
    }
    Ok(listener)
}

/// Serve on a listener from [`bind`] until the process ends.
///
/// # Errors
///
/// When the record cannot be opened or the listener fails.
pub async fn serve(config: Config, listener: tokio::net::TcpListener) -> Result<(), Error> {
    config.access.bound(listener.local_addr()?);
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
            harness: Some("claude".into()),
            auth: "subscription".into(),
            backend: "auto".into(),
            command_override: Some(command.iter().map(|s| (*s).to_string()).collect()),
            access: Access::with_token(&dir.join("memory"), TOKEN),
        }
    }

    const TOKEN: &str = "test-token";

    #[test]
    fn a_profile_passes_only_what_the_request_sets_and_names_are_checked() {
        let mut c = config(std::path::Path::new("/tmp/x"), &[]);
        c.command_override = None;
        let argv = |r: NewSession| launch_argv(&c, &r).map(|a| a[3..].join(" "));
        assert_eq!(
            argv(NewSession {
                profile: Some("nightly".into()),
                ..NewSession::default()
            })
            .unwrap(),
            "launch --profile nightly"
        );
        assert_eq!(
            argv(NewSession {
                profile: Some("nightly".into()),
                auth: Some("key".into()),
                backend: Some("none".into()),
                model: Some(" m2 ".into()),
                work: true,
                ..NewSession::default()
            })
            .unwrap(),
            "work --profile nightly --auth key --backend none --model m2"
        );
        let plain = argv(NewSession {
            harness: Some("codex".into()),
            backend: Some("image".into()),
            ..NewSession::default()
        })
        .unwrap();
        assert!(
            plain.contains("--harness codex") && plain.contains("--backend image"),
            "{plain}"
        );
        for bad in ["../x", "Nightly", "-p", ""] {
            let r = argv(NewSession {
                profile: Some(bad.into()),
                ..NewSession::default()
            });
            assert!(bad.is_empty() || r.is_err(), "{bad}: {r:?}");
        }
        assert!(argv(NewSession {
            harness: Some("codex".into()),
            backend: Some("chroot".into()),
            ..NewSession::default()
        })
        .unwrap_err()
        .contains("no backend named chroot"));
        let l = Launched::of(&NewSession {
            profile: Some("nightly".into()),
            prompt: Some("go".into()),
            ..NewSession::default()
        });
        assert_eq!(
            (l.profile.as_deref(), l.mode.as_str()),
            (Some("nightly"), "prompt")
        );
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
                ..NewSession::default()
            },
        )
        .unwrap();
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
        )
        .unwrap();
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

    #[test]
    fn a_model_and_the_board_go_into_the_command_line_and_the_title() {
        let mut c = config(std::path::Path::new("/tmp/x"), &[]);
        c.command_override = None;
        let req = NewSession {
            harness: Some("muse".into()),
            auth: Some("gateway".into()),
            provider: Some("llmao".into()),
            model: Some(" qwen3.8-27b ".into()),
            subagents: Some("board".into()),
            prompt: Some("say hi".into()),
            ..NewSession::default()
        };
        let a = launch_argv(&c, &req).unwrap().join(" ");
        assert!(
            a.ends_with(
                "launch --harness muse --auth gateway --backend auto --provider llmao --model qwen3.8-27b --subagents board --prompt say hi"
            ),
            "{a}"
        );
        assert_eq!(title_for(&req, &c), "muse on qwen3.8-27b: say hi");
        // an empty model or destination is none
        let a = launch_argv(
            &c,
            &NewSession {
                harness: Some("codex".into()),
                model: Some("  ".into()),
                provider: Some(String::new()),
                ..NewSession::default()
            },
        )
        .unwrap()
        .join(" ");
        assert!(!a.contains("--model") && !a.contains("--provider"), "{a}");
        // no harness anywhere is refused: there is no default
        c.harness = None;
        let err = launch_argv(&c, &NewSession::default()).unwrap_err();
        assert!(err.contains("no default"), "{err}");
        let o = options(&c);
        assert_eq!(o["destinations"]["gateway"][0], "llmao");
        assert!(o["harnesses"]
            .as_array()
            .unwrap()
            .iter()
            .any(|h| h == "muse"));
        assert!(o["defaults"]["harness"].is_null());
    }

    async fn start(
        dir: &std::path::Path,
        command: &[&str],
    ) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        Memory::init(&dir.join("memory"), "human:test:me", "dom-me").unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let config = config(dir, command);
        let h = tokio::spawn(async move {
            serve(config, listener).await.unwrap();
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
            "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
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

    /// One request as written, headers and all; the status and the head of the reply.
    async fn raw(addr: std::net::SocketAddr, request: &str) -> (u16, String) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(request.as_bytes()).await.unwrap();
        let mut out = vec![0u8; 4096];
        let n = tokio::time::timeout(std::time::Duration::from_secs(5), s.read(&mut out))
            .await
            .unwrap()
            .unwrap();
        let text = String::from_utf8_lossy(&out[..n]).into_owned();
        let status = text
            .split(' ')
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        (status, text)
    }

    #[tokio::test]
    async fn the_gate_wants_a_loopback_host_its_own_origin_and_a_key() {
        let dir = tempfile::tempdir().unwrap();
        let (addr, _h) = start(dir.path(), &["true"]).await;
        let get = |path: &str, extra: &str| {
            format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\n{extra}Connection: close\r\n\r\n")
        };
        let bearer = format!("Authorization: Bearer {TOKEN}\r\n");

        // the record behind the API wants a key; the page that says how to get one does not
        assert_eq!(raw(addr, &get("/api/sessions", "")).await.0, 401);
        assert_eq!(
            raw(
                addr,
                &get("/api/sessions", "Authorization: Bearer nope\r\n")
            )
            .await
            .0,
            401
        );
        assert_eq!(raw(addr, &get("/api/sessions", &bearer)).await.0, 200);
        assert_eq!(raw(addr, &get("/", "")).await.0, 200);
        assert_eq!(
            raw(addr, &get("/bundle/dom-me/internal/manifest.yaml", ""))
                .await
                .0,
            401
        );
        // a cookie is no key: the browser would send it to every port on this host
        let cookie = get(
            "/api/sessions",
            &format!("Cookie: amem-{}={TOKEN}\r\n", addr.port()),
        );
        assert_eq!(raw(addr, &cookie).await.0, 401);

        // a name that rebinds to loopback arrives with its own Host, key or not
        let rebound = format!(
            "GET /api/sessions HTTP/1.1\r\nHost: attacker.example:{}\r\n{bearer}Connection: close\r\n\r\n",
            addr.port()
        );
        assert_eq!(raw(addr, &rebound).await.0, 403);

        // a page from another origin, another port on this host included, is refused
        for origin in ["https://attacker.example", "http://127.0.0.1:1", "null"] {
            let r = get("/api/sessions", &format!("{bearer}Origin: {origin}\r\n"));
            assert_eq!(raw(addr, &r).await.0, 403, "{origin}");
        }
        let own = get(
            "/api/sessions",
            &format!("{bearer}Origin: http://{addr}\r\n"),
        );
        assert_eq!(raw(addr, &own).await.0, 200);
    }

    fn post(addr: std::net::SocketAddr, path: &str, auth: &str, body: &str) -> String {
        format!(
            "POST {path} HTTP/1.1\r\nHost: {addr}\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn json_body(reply: &str) -> Value {
        serde_json::from_str(reply.split_once("\r\n\r\n").unwrap().1).unwrap()
    }

    #[tokio::test]
    async fn a_hosted_page_the_operator_allowed_is_answered_across_origins() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("memory");
        Memory::init(&root, "human:test:me", "dom-me").unwrap();
        let mut c = config(dir.path(), &["true"]);
        c.access = Access::new(
            &root,
            &crate::state::WebState {
                token: TOKEN.into(),
                ..crate::state::WebState::default()
            },
            vec!["https://amem.example".into()],
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { serve(c, listener).await.unwrap() });
        let req = |method: &str, path: &str, extra: &str| {
            format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\n{extra}Connection: close\r\n\r\n")
        };
        let hosted = "Origin: https://amem.example\r\n";
        let preflight = format!(
            "{hosted}Access-Control-Request-Method: GET\r\nAccess-Control-Request-Headers: authorization\r\nAccess-Control-Request-Private-Network: true\r\n"
        );
        let (status, head) = raw(addr, &req("OPTIONS", "/api/sessions", &preflight)).await;
        assert_eq!(status, 204, "a preflight carries no key: {head}");
        let head = head.to_ascii_lowercase();
        assert!(
            head.contains("access-control-allow-origin: https://amem.example"),
            "{head}"
        );
        assert!(
            head.contains("access-control-allow-private-network: true"),
            "{head}"
        );
        assert!(
            head.contains("access-control-allow-headers: authorization, content-type"),
            "{head}"
        );

        let keyed = format!("{hosted}Authorization: Bearer {TOKEN}\r\n");
        let (status, head) = raw(addr, &req("GET", "/api/sessions", &keyed)).await;
        assert_eq!(status, 200);
        assert!(head
            .to_ascii_lowercase()
            .contains("access-control-allow-origin: https://amem.example"));
        let (status, head) = raw(addr, &req("GET", "/api/sessions", hosted)).await;
        assert_eq!(status, 401, "the call itself still wants a key");
        assert!(
            head.to_ascii_lowercase()
                .contains("access-control-allow-origin"),
            "so the page can read why"
        );

        let other = "Origin: https://attacker.example\r\nAccess-Control-Request-Method: GET\r\n";
        assert_eq!(
            raw(addr, &req("OPTIONS", "/api/sessions", other)).await.0,
            403
        );
        // the server's own pages get no cross-origin headers
        let (_, head) = raw(
            addr,
            &req(
                "GET",
                "/api/sessions",
                &format!("Authorization: Bearer {TOKEN}\r\nOrigin: http://{addr}\r\n"),
            ),
        )
        .await;
        assert!(
            !head
                .to_ascii_lowercase()
                .contains("access-control-allow-origin"),
            "{head}"
        );
        // the pages load with no key, by path and by file name
        for page in [
            "/board",
            "/board.html",
            "/session.html",
            "/index.html",
            "/tokens.css",
        ] {
            assert_eq!(raw(addr, &req("GET", page, "")).await.0, 200, "{page}");
        }
    }

    #[tokio::test]
    async fn the_token_mints_a_code_a_browser_trades_once_for_a_key_of_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let (addr, _h) = start(dir.path(), &["true"]).await;
        let bearer = format!("Authorization: Bearer {TOKEN}\r\n");
        assert_eq!(raw(addr, &post(addr, "/api/pair", "", "")).await.0, 401);
        let (status, reply) = raw(addr, &post(addr, "/api/pair", &bearer, "")).await;
        assert_eq!(status, 200, "{reply}");
        let code = json_body(&reply)["code"].as_str().unwrap().to_string();

        let redeem = post(
            addr,
            "/api/pair/redeem",
            "",
            &format!(r#"{{"code":"{code}"}}"#),
        );
        let (status, reply) = raw(addr, &redeem).await;
        assert_eq!(status, 200, "{reply}");
        let key = json_body(&reply)["key"].as_str().unwrap().to_string();
        assert_ne!(key, TOKEN);
        assert_eq!(raw(addr, &redeem).await.0, 401, "a code is single use");

        let with_key = format!("Authorization: Bearer {key}\r\n");
        let list = format!(
            "GET /api/sessions HTTP/1.1\r\nHost: {addr}\r\n{with_key}Connection: close\r\n\r\n"
        );
        assert_eq!(raw(addr, &list).await.0, 200);
        // a browser's key does not mint codes for more browsers
        assert_eq!(
            raw(addr, &post(addr, "/api/pair", &with_key, "")).await.0,
            401
        );
    }

    #[tokio::test]
    async fn hello_proves_the_token_without_it() {
        let dir = tempfile::tempdir().unwrap();
        let (addr, _h) = start(dir.path(), &["true"]).await;
        let nonce = "0123456789abcdef0123";
        let (status, reply) = raw(
            addr,
            &format!("GET /api/hello?nonce={nonce} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"),
        )
        .await;
        assert_eq!(status, 200, "{reply}");
        assert!(!reply.contains(TOKEN));
        let v = json_body(&reply);
        let root = dir.path().join("memory");
        assert_eq!(v["proof"], access::proof(TOKEN, addr, &root, nonce));
        assert!(v.get("root").is_none() && v.get("workdir").is_none(), "{v}");
        assert_eq!(v["server"], "amem");
        let short =
            format!("GET /api/hello?nonce=x HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
        assert_eq!(raw(addr, &short).await.0, 400);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_websocket_carries_its_key_as_a_subprotocol_and_another_origin_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (addr, _h) = start(dir.path(), &["sh", "-c", "sleep 5"]).await;
        let mut ids = Vec::new();
        for _ in 0..2 {
            let (status, body) =
                http(addr, "POST", "/api/sessions", Some(r#"{"auth":"key"}"#)).await;
            assert_eq!(status, 200, "{body}");
            ids.push(
                serde_json::from_str::<Value>(&body).unwrap()["id"]
                    .as_str()
                    .unwrap()
                    .to_string(),
            );
        }
        assert!(ids
            .iter()
            .all(|i| i.len() == 32 && i.bytes().all(|b| b.is_ascii_hexdigit())));
        assert_ne!(ids[0], ids[1]);
        let id = &ids[0];
        let upgrade = |origin: &str, protocols: &str| {
            format!(
                "GET /ws/{id} HTTP/1.1\r\nHost: {addr}\r\nOrigin: {origin}\r\n{protocols}Connection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
            )
        };
        let offered = format!("Sec-WebSocket-Protocol: amem, amem.key.{TOKEN}\r\n");
        let own = format!("http://{addr}");
        assert_eq!(
            raw(addr, &upgrade("https://attacker.example", &offered))
                .await
                .0,
            403
        );
        assert_eq!(
            raw(addr, &upgrade(&own, "Sec-WebSocket-Protocol: amem\r\n"))
                .await
                .0,
            401
        );
        let (status, head) = raw(addr, &upgrade(&own, &offered)).await;
        assert_eq!(status, 101, "{head}");
        assert!(
            head.to_ascii_lowercase()
                .contains("sec-websocket-protocol: amem\r\n"),
            "{head}"
        );
    }

    async fn get_json(addr: std::net::SocketAddr, path: &str) -> (u16, Value) {
        let (status, body) = http(addr, "GET", path, None).await;
        (status, serde_json::from_str(&body).unwrap_or(Value::Null))
    }

    #[tokio::test]
    async fn the_board_files_admits_claims_and_completes_a_person_s_task() {
        let dir = tempfile::tempdir().unwrap();
        let (addr, _h) = start(dir.path(), &["true"]).await;
        let (status, body) = http(
            addr,
            "POST",
            "/api/tasks",
            Some(r#"{"title":"Sign off the release","body":"Check the notes.","requirements":["human"]}"#),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let id = serde_json::from_str::<Value>(&body).unwrap()["task"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let (status, _) = http(addr, "POST", "/api/tasks", Some(r#"{"title":"  "}"#)).await;
        assert_eq!(status, 400);
        let (_, all) = get_json(addr, "/api/tasks").await;
        assert_eq!(all["domain"], "dom-me");
        assert_eq!(all["tasks"][0]["status"], "proposed");
        let (status, _) = http(addr, "POST", &format!("/api/tasks/{id}/admit"), None).await;
        assert_eq!(status, 200);
        // the board is a person, so a task that requires one is claimable from it
        let (status, body) = http(addr, "POST", &format!("/api/tasks/{id}/claim"), None).await;
        assert_eq!(status, 200, "{body}");
        let (status, _) = http(
            addr,
            "POST",
            &format!("/api/tasks/{id}/complete"),
            Some(r#"{"handoff":"  "}"#),
        )
        .await;
        assert_eq!(status, 400, "a handoff needs a body");
        let (status, body) = http(
            addr,
            "POST",
            &format!("/api/tasks/{id}/complete"),
            Some(r#"{"handoff":"Signed off; ship it."}"#),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let done: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(done["task"]["status"], "completed");
        assert!(done["handoff"].as_str().unwrap().starts_with("mem-"));
        let (status, tree) = get_json(addr, &format!("/api/tasks/{id}/tree")).await;
        assert_eq!(status, 200);
        assert_eq!(tree["id"], id);
        assert_eq!(tree["children"].as_array().unwrap().len(), 0);
        let (status, _) = http(addr, "POST", "/api/tasks/task-me-none/retry", None).await;
        assert!(status == 404 || status == 409, "{status}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_session_is_told_its_id_and_its_status_is_what_its_launcher_wrote() {
        let dir = tempfile::tempdir().unwrap();
        let (addr, _h) = start(
            dir.path(),
            &["sh", "-c", "echo id=$AMEM_WEB_SESSION; sleep 5"],
        )
        .await;
        let (status, body) = http(
            addr,
            "POST",
            "/api/sessions",
            Some(r#"{"profile":"nightly"}"#),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let v: Value = serde_json::from_str(&body).unwrap();
        let id = v["id"].as_str().unwrap().to_string();
        assert_eq!(v["launch"]["profile"], "nightly");
        assert_eq!(v["title"], "nightly");
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let (_, text) =
                http(addr, "GET", &format!("/api/sessions/{id}/scrollback"), None).await;
            if text.contains(&format!("id={id}")) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the session was not told its id: {text}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let (status, v) = get_json(addr, &format!("/api/sessions/{id}/status")).await;
        assert_eq!((status, v), (200, json!({})), "nothing written yet");
        let file = dir
            .path()
            .join("memory/state/web")
            .join(format!("{id}.json"));
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, r#"{"destination":"llmao","usage":{"calls":3}}"#).unwrap();
        let (_, v) = get_json(addr, &format!("/api/sessions/{id}/status")).await;
        assert_eq!(v["destination"], "llmao");
        assert_eq!(v["usage"]["calls"], 3);
        let (status, _) = get_json(
            addr,
            "/api/sessions/0123456789abcdef0123456789abcdef/status",
        )
        .await;
        assert_eq!(status, 404, "only a session this server started");
        let (_, o) = get_json(addr, "/api/options").await;
        assert_eq!(o["backends"][0], "auto");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_scrollback_is_the_ring_as_text_and_a_request_without_a_harness_is_400() {
        let dir = tempfile::tempdir().unwrap();
        let (addr, _h) = start(dir.path(), &["sh", "-c", "echo hello-ring"]).await;
        let (status, body) = http(addr, "POST", "/api/sessions", Some(r#"{"auth":"key"}"#)).await;
        assert_eq!(status, 200, "the test config preselects claude: {body}");
        let id = serde_json::from_str::<Value>(&body).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let (_, info) = get_json(addr, &format!("/api/sessions/{id}")).await;
            if info["alive"] == false || tokio::time::Instant::now() > deadline {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let (status, text) =
            http(addr, "GET", &format!("/api/sessions/{id}/scrollback"), None).await;
        assert_eq!(status, 200);
        assert!(text.contains("hello-ring"), "{text}");
        let (status, _) = http(addr, "GET", "/api/sessions/nope/scrollback", None).await;
        assert_eq!(status, 404);
        let (status, _) = get_json(addr, "/api/options").await;
        assert_eq!(status, 200);
    }

    #[cfg(unix)]
    async fn ws_connect(
        addr: std::net::SocketAddr,
        id: &str,
    ) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>
    {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let mut req = format!("ws://{addr}/ws/{id}")
            .into_client_request()
            .unwrap();
        req.headers_mut()
            .insert("authorization", format!("Bearer {TOKEN}").parse().unwrap());
        let (ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
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
        assert!(page.contains("amem reader") && page.contains("reader/dm_reader.js"));
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
