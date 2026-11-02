//! Tools hosted elsewhere (PLAN M19): a tool whose grant names `hosted` is called on a
//! tool server over MCP's streamable HTTP instead of run here, so a launcher in a
//! managed environment carries no tool runtime. The server runs the same component
//! under its own grant and credential; the call carries the hosts this session may
//! reach at its label, which the server may only narrow, and the answer comes back
//! with every URL the server reached. Each of those is decided by the policy here, as
//! the tap host decides a request before it leaves, and an answer that names none, or
//! names one the policy refuses, is not kept; what is kept says where it was fetched
//! from.
//!
//! The server is asked once what MCP it speaks (`server/discover`): one of 2026-07-28
//! is called with one stateless request, one before it, which has no such method,
//! after the `initialize` handshake, as every server was before that revision.
//!
//! The call leaves through the egress ([`Egress::post`]), as a model call does: the
//! session's label is held to the tool's ceiling before anything is sent, the server's
//! bearer goes on there, and every request is metered and traced with its status and
//! latency. The harness itself cannot reach the server that way; a tool server is a
//! destination of the launcher's, not of the sandbox's.

use crate::mcp_meta::{self, supported, LEGACY, MODERN, MODERN_ERRORS, UNSUPPORTED_VERSION};
use dm_egress::{Auth, Destination, Egress};
use dm_policy::Grants;
use dm_server::sources::Answered;
use dm_server::Memory;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

/// What a tool server says back for one call: the tool's JSON, and where it went.
#[derive(Debug, PartialEq, Eq)]
pub struct Reply {
    /// The tool's answer, JSON as text.
    pub answer: String,
    /// The URLs its requests reached, in order.
    pub reached: Vec<String>,
}

/// A hosted tool's URL split for the egress: the server's origin, which is the
/// destination, and the path of its MCP endpoint, which each call names.
///
/// # Errors
///
/// A URL without a scheme or a host.
pub fn split_url(url: &str) -> Result<(String, String), String> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| format!("{url:?} is not a URL"))?;
    let (authority, path) = rest.find('/').map_or((rest, ""), |i| rest.split_at(i));
    if authority.is_empty() {
        return Err(format!("{url:?} names no host"));
    }
    Ok((format!("{scheme}://{authority}"), path.to_string()))
}

/// The tool servers the grants name, as egress destinations by tool: each hosted
/// tool's server at its origin, with the bearer the grant's `credential` variable
/// holds, if it is set, injected on the way out.
///
/// # Errors
///
/// A `hosted` URL that cannot be split.
pub fn tool_servers(grants: &Grants) -> Result<HashMap<String, Destination>, String> {
    let mut servers = HashMap::new();
    for (name, grant) in &grants.taps {
        let Some(url) = &grant.hosted else {
            continue;
        };
        let (origin, _) = split_url(url).map_err(|e| format!("[taps.{name}] hosted: {e}"))?;
        let bearer = grant
            .credential
            .as_deref()
            .and_then(|var| crate::present(std::env::var(var).ok()));
        let auth = match bearer {
            Some(b) => Auth::Inject {
                header: axum::http::header::AUTHORIZATION,
                value: format!("Bearer {b}"),
            },
            None => Auth::PassThrough,
        };
        servers.insert(name.clone(), Destination::forward(origin, auth));
    }
    Ok(servers)
}

/// The arguments of a tool server's `tools/call`, as [`call`] sends them and
/// `amem tool-server` reads them.
#[must_use]
pub fn arguments(args: &str, config: &str, hosts: &[String]) -> Value {
    let as_json = |t: &str| serde_json::from_str::<Value>(t).unwrap_or(Value::String(t.into()));
    json!({ "args": as_json(args), "config": as_json(config), "hosts": hosts })
}

/// Read one JSON-RPC message from a response body, plain JSON or an event stream.
fn message(content_type: &str, body: &str) -> Result<Value, String> {
    if content_type.starts_with("text/event-stream") {
        body.lines()
            .filter_map(|l| l.strip_prefix("data:"))
            .filter_map(|d| serde_json::from_str::<Value>(d.trim()).ok())
            .find(|v| v.get("id").is_some())
            .ok_or_else(|| "the tool server's event stream held no reply".into())
    } else {
        serde_json::from_str(body).map_err(|e| format!("the tool server answered no JSON: {e}"))
    }
}

/// The tool's reply out of a `tools/call` result.
///
/// # Errors
///
/// A JSON-RPC error, a result marked as an error, or one that is not the shape
/// `amem tool-server` answers in.
pub fn reply(message: &Value) -> Result<Reply, String> {
    if let Some(e) = message.get("error") {
        return Err(format!("the tool server refused the call: {e}"));
    }
    let result = &message["result"];
    crate::mcp_meta::complete(result).map_err(|e| format!("the tool server: {e}"))?;
    let text = result["content"]
        .as_array()
        .and_then(|c| c.iter().find_map(|b| b["text"].as_str()))
        .unwrap_or_default();
    if result["isError"] == true {
        return Err(text.to_string());
    }
    let v: Value = serde_json::from_str(text)
        .map_err(|e| format!("the tool server's answer is not the tool's JSON: {e}"))?;
    let answer = match &v["answer"] {
        Value::String(s) => s.clone(),
        Value::Null => return Err("the tool server's answer carries no `answer`".into()),
        other => other.to_string(),
    };
    let reached = v["reached"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|u| u.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    Ok(Reply { answer, reached })
}

/// Hold a reply to the policy: it must say where it went, and every place must be one
/// this session's policy lets the tool reach. What passes is the answer with the URLs
/// it was fetched from, kept on the source.
///
/// # Errors
///
/// No URL reached, or one the policy refuses, with the policy's reason.
pub fn checked(memory: &Memory, tool: &str, r: Reply) -> Result<Answered, String> {
    if r.reached.is_empty() {
        return Err(format!(
            "the tool server answered {tool} without saying what it reached; an answer from nowhere is not kept"
        ));
    }
    for url in &r.reached {
        memory.check_reached(tool, url).map_err(|why| {
            format!("the tool server reached {url}, which this session may not: {why}")
        })?;
    }
    Ok(Answered {
        answer: r.answer,
        reached: r.reached,
    })
}

/// Which revision a tool server speaks: 2026-07-28, called statelessly, or one before
/// it, called after the `initialize` handshake.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Era {
    Stateless,
    Handshake,
}

/// What each tool server was found to speak, by URL, for the life of this process: a
/// client keeps that per server rather than asking before every call. A call that
/// fails on the way, or is answered with a reply not of that revision ([`unspoken`]),
/// forgets it, so the next one asks again.
fn eras() -> &'static Mutex<HashMap<String, Era>> {
    static ERAS: OnceLock<Mutex<HashMap<String, Era>>> = OnceLock::new();
    ERAS.get_or_init(Mutex::default)
}

fn known(url: &str) -> Option<Era> {
    eras()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(url)
        .copied()
}

fn learn(url: &str, era: Option<Era>) {
    let mut eras = eras()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match era {
        Some(e) => eras.insert(url.to_string(), e),
        None => eras.remove(url),
    };
}

/// What a tool server's answer to `server/discover` says it speaks. A complete result
/// listing 2026-07-28 is that revision, and one of another kind is no list at all; one
/// listing only revisions with a handshake, or that revision's refusal of the version
/// naming only those, is one of them. A server from before it has no such method: it
/// answers with a status of a request it cannot take ([`before`]) and none of that
/// revision's errors, or with a success carrying "method not found", as `amem
/// tool-server` did. Anything else says nothing of the revision, an error page, a limit,
/// a refusal of the caller, and is not taken as one.
///
/// # Errors
///
/// That revision's refusal of the request itself, a result of a kind not taken,
/// versions none of which this launcher speaks, or an answer that names no revision,
/// with its status.
fn era(status: u16, content_type: &str, body: &str) -> Result<Era, String> {
    let m = message(content_type, body).ok();
    let error = m.as_ref().and_then(|m| m.get("error"));
    let code = error.and_then(|e| e["code"].as_i64());
    if let (Some(e), Some(c)) = (error, code) {
        if c == UNSUPPORTED_VERSION {
            return named(&e["data"]["supported"], false);
        }
        if MODERN_ERRORS.contains(&c) {
            return Err(format!("the tool server refused server/discover: {e}"));
        }
    }
    let listed = m
        .as_ref()
        .map(|m| &m["result"])
        .filter(|r| r["supportedVersions"].is_array());
    match listed {
        Some(r) if (200..300).contains(&status) => {
            mcp_meta::complete(r)
                .map_err(|e| format!("the tool server's answer to server/discover: {e}"))?;
            return named(&r["supportedVersions"], true);
        }
        _ if (200..300).contains(&status) && code == Some(-32601) => return Ok(Era::Handshake),
        _ if before(status) => return Ok(Era::Handshake),
        _ => {}
    }
    let said: String = body.trim().chars().take(300).collect();
    Err(format!(
        "the tool server answered server/discover with {status} and named no MCP revision: {said}"
    ))
}

/// A status a server from before 2026-07-28 answers a method it has no such thing as
/// with: a bad request, not found, a method not allowed, and the 415 and 422 that rmcp's
/// session servers answer a request they cannot read or one before `initialize` with.
/// A refusal of the caller (401, 403), a limit (429), or a failure on the way (5xx) says
/// nothing of the revision.
fn before(status: u16) -> bool {
    matches!(status, 400 | 404 | 405 | 415 | 422)
}

/// The revision a list of versions leaves: 2026-07-28 when it is listed and
/// `stateless` allows it, else the handshake when a revision with one is listed.
fn named(listed: &Value, stateless: bool) -> Result<Era, String> {
    let listed: Vec<&str> = listed
        .as_array()
        .map(|l| l.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if stateless && listed.contains(&MODERN) {
        Ok(Era::Stateless)
    } else if listed.iter().any(|v| LEGACY.contains(v)) {
        Ok(Era::Handshake)
    } else {
        Err(format!(
            "the tool server speaks MCP {}, and this launcher {}",
            listed.join(", "),
            supported().join(", ")
        ))
    }
}

/// Whether a reply to a call says the revision kept for its server is not the one it
/// speaks: no JSON-RPC result or error, a result of a kind not taken, or an error with
/// the request's form or protocol (a parse error, an invalid request, a method not
/// found, or one of 2026-07-28's own) rather than with the call. A tool's refusal, an
/// unknown tool, or a failure in the server says nothing of the revision.
fn unspoken(message: &Value) -> bool {
    match (message.get("error"), message.get("result")) {
        (Some(e), _) => e["code"]
            .as_i64()
            .is_none_or(|c| [-32700, -32600, -32601].contains(&c) || MODERN_ERRORS.contains(&c)),
        (None, Some(r)) => !r.is_object() || mcp_meta::complete(r).is_err(),
        (None, None) => true,
    }
}

/// One POST to a tool server, as it answered.
struct Sent {
    status: u16,
    content_type: String,
    session: Option<String>,
    body: String,
}

impl Sent {
    /// The answer, when its status is a success.
    fn ok(self) -> Result<Sent, String> {
        if (200..300).contains(&self.status) {
            Ok(self)
        } else {
            Err(format!(
                "the tool server answered {}: {}",
                self.status,
                self.body.trim()
            ))
        }
    }
}

/// The way to one tool server: through the egress, under the tool's name, to the path
/// of its MCP endpoint.
struct Server<'a> {
    egress: &'a Egress,
    tool: &'a str,
    path: String,
}

impl Server<'_> {
    async fn post(&self, body: &Value, extra: &[(&'static str, String)]) -> Result<Sent, String> {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("application/json"),
        );
        headers.insert(
            axum::http::header::ACCEPT,
            axum::http::HeaderValue::from_static("application/json, text/event-stream"),
        );
        for (name, value) in extra {
            let value = axum::http::HeaderValue::from_str(value)
                .map_err(|_| format!("{name}: {value:?} is not a header value"))?;
            headers.insert(axum::http::HeaderName::from_static(name), value);
        }
        let r = self
            .egress
            .post(self.tool, &self.path, headers, body.to_string().into())
            .await
            .map_err(|e| e.to_string())?;
        let text = |name: &str| {
            r.headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        Ok(Sent {
            status: r.status,
            content_type: text("content-type").unwrap_or_default(),
            session: text("mcp-session-id"),
            body: r.body,
        })
    }

    /// Ask the server what it speaks.
    async fn discover(&self) -> Result<Era, String> {
        let request = mcp_meta::request(1, "server/discover", Value::Null, "amem");
        let sent = self.post(&request, &mcp_meta::headers(&request)).await?;
        era(sent.status, &sent.content_type, &sent.body)
    }

    /// One `tools/call` of 2026-07-28: one request, no session.
    async fn stateless(&self, params: Value) -> Result<Sent, String> {
        let request = mcp_meta::request(1, "tools/call", params, "amem");
        self.post(&request, &mcp_meta::headers(&request))
            .await?
            .ok()
    }

    /// One `tools/call` after the handshake of the revisions before 2026-07-28, in the
    /// session the server opens, if it opens one, at the version it answers with.
    async fn handshake(&self, params: Value) -> Result<Sent, String> {
        let opened = self
            .post(
                &json!({
                    "jsonrpc": "2.0", "id": 1, "method": "initialize",
                    "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                               "clientInfo": {"name": "amem", "version": env!("CARGO_PKG_VERSION")}}
                }),
                &[],
            )
            .await?
            .ok()?;
        let mut headers = Vec::new();
        if let Some(s) = opened.session.clone() {
            headers.push(("mcp-session-id", s));
        }
        let version = message(&opened.content_type, &opened.body)
            .ok()
            .and_then(|m| m["result"]["protocolVersion"].as_str().map(str::to_string));
        if let Some(v) = version {
            headers.push((mcp_meta::VERSION_HEADER, v));
        }
        self.post(
            &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            &headers,
        )
        .await?
        .ok()?;
        self.post(
            &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": params}),
            &headers,
        )
        .await?
        .ok()
    }
}

/// Call `tool` on its tool server for the session `memory` is, through `egress`. A
/// server is asked once what it speaks (`server/discover`): one of 2026-07-28 is
/// called statelessly, one before it after the `initialize` handshake.
///
/// # Errors
///
/// The egress refusing the call (no server for the tool, or the session above its
/// ceiling), the server unreachable or refusing, the tool failing there, or a reply
/// the policy does not accept ([`checked`]).
pub async fn call(
    egress: &Egress,
    memory: Arc<Memory>,
    tool: &str,
    config: &str,
    args: &str,
) -> Result<Answered, String> {
    let url = memory
        .grants()
        .taps
        .get(tool)
        .and_then(|g| g.hosted.clone())
        .ok_or_else(|| format!("[taps.{tool}] names no tool server"))?;
    let (_, path) = split_url(&url)?;
    let server = Server { egress, tool, path };
    let era = match known(&url) {
        Some(e) => e,
        None => {
            let e = server.discover().await?;
            learn(&url, Some(e));
            e
        }
    };
    let hosts = memory.fetch_hosts(tool);
    let params = json!({"name": tool, "arguments": arguments(args, config, &hosts)});
    let sent = match era {
        Era::Stateless => server.stateless(params).await,
        Era::Handshake => server.handshake(params).await,
    }
    .inspect_err(|_| learn(&url, None))?;
    let answered = message(&sent.content_type, &sent.body).inspect_err(|_| learn(&url, None))?;
    if unspoken(&answered) {
        learn(&url, None);
    }
    checked(&memory, tool, reply(&answered)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use std::io::Write as _;
    use std::sync::atomic::{AtomicBool, AtomicU16, AtomicUsize, Ordering::SeqCst};

    /// A tool server of 2026-07-28 on loopback, counting what it is asked. Discovery is
    /// answered with `discover` when set, a status, a content type and a body, and with
    /// the version list otherwise; a call fails with a 502 while `failing` is set, and
    /// is answered with `answer` when set; the handshake is refused.
    #[derive(Default)]
    struct Fake {
        discover: Mutex<Option<(u16, &'static str, &'static str)>>,
        answer: Mutex<Option<(u16, &'static str, &'static str)>>,
        failing: AtomicBool,
        port: AtomicU16,
        discovers: AtomicUsize,
        handshakes: AtomicUsize,
        calls: AtomicUsize,
    }

    async fn fake(State(f): State<Arc<Fake>>, body: String) -> axum::response::Response {
        let m: Value = serde_json::from_str(&body).unwrap_or_default();
        match m["method"].as_str().unwrap_or_default() {
            "server/discover" => {
                f.discovers.fetch_add(1, SeqCst);
                let set = *f.discover.lock().unwrap();
                if let Some((status, kind, text)) = set {
                    let status = StatusCode::from_u16(status).unwrap();
                    return (status, [("content-type", kind)], text).into_response();
                }
                axum::Json(
                    json!({"jsonrpc": "2.0", "id": 1, "result": {"resultType": "complete",
                    "supportedVersions": ["2026-07-28"], "capabilities": {"tools": {}}}}),
                )
                .into_response()
            }
            "tools/call" => {
                f.calls.fetch_add(1, SeqCst);
                if f.failing.load(SeqCst) {
                    return (StatusCode::BAD_GATEWAY, "upstream down").into_response();
                }
                let set = *f.answer.lock().unwrap();
                if let Some((status, kind, text)) = set {
                    let status = StatusCode::from_u16(status).unwrap();
                    return (status, [("content-type", kind)], text).into_response();
                }
                let page = format!("http://127.0.0.1:{}/page", f.port.load(SeqCst));
                let text = json!({"answer": {"text": "ok"}, "reached": [page]}).to_string();
                axum::Json(
                    json!({"jsonrpc": "2.0", "id": 1, "result": {"resultType": "complete",
                    "content": [{"type": "text", "text": text}]}}),
                )
                .into_response()
            }
            _ => {
                f.handshakes.fetch_add(1, SeqCst);
                (StatusCode::BAD_REQUEST, "no handshake here").into_response()
            }
        }
    }

    /// A root whose fetch is hosted by a [`Fake`], the egress to it, and the fake.
    async fn hosted_fetch(dir: &std::path::Path) -> (Egress, Arc<Memory>, Arc<Fake>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let f = Arc::new(Fake::default());
        f.port.store(port, SeqCst);
        let app = axum::Router::new()
            .route("/mcp", axum::routing::post(fake))
            .with_state(f.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let root = dir.join("m");
        Memory::init(&root, "human:test:me", "dom-me").unwrap();
        let mut g = std::fs::OpenOptions::new()
            .append(true)
            .open(root.join("grants.toml"))
            .unwrap();
        write!(
            g,
            "\n[taps.fetch]\ndomain = \"dom-me\"\ntool = true\nceiling = \"public\"\nhosts = [\"127.0.0.1\"]\nloopback = true\nhosted = \"http://127.0.0.1:{port}/mcp\"\n"
        )
        .unwrap();
        let memory = Arc::new(Memory::open(&root).unwrap().without_read_log());
        let egress = Egress::with_tool_servers(
            memory.clone(),
            HashMap::new(),
            tool_servers(memory.grants()).unwrap(),
            &dir.join("t.db"),
        )
        .unwrap();
        (egress, memory, f)
    }

    async fn fetch(egress: &Egress, memory: &Arc<Memory>) -> Result<Answered, String> {
        let args = "{\"url\": \"http://127.0.0.1:1/page\"}";
        call(egress, memory.clone(), "fetch", "{}", args).await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn what_a_server_speaks_is_asked_once_and_again_after_a_call_fails() {
        let dir = tempfile::tempdir().unwrap();
        let (egress, memory, f) = hosted_fetch(dir.path()).await;
        fetch(&egress, &memory).await.unwrap();
        fetch(&egress, &memory).await.unwrap();
        assert_eq!(f.discovers.load(SeqCst), 1);
        assert_eq!(f.calls.load(SeqCst), 2);
        f.failing.store(true, SeqCst);
        let e = fetch(&egress, &memory).await.unwrap_err();
        assert!(e.contains("502"), "{e}");
        assert_eq!(f.discovers.load(SeqCst), 1);
        f.failing.store(false, SeqCst);
        fetch(&egress, &memory).await.unwrap();
        assert_eq!(f.discovers.load(SeqCst), 2);
        assert_eq!(f.handshakes.load(SeqCst), 0);
    }

    /// An answer to discovery that says nothing of the revision, an error page, a limit,
    /// a refusal of the caller, is an error, and is not taken for a server from before.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_answer_to_discovery_that_names_no_revision_is_an_error_and_not_kept() {
        let dir = tempfile::tempdir().unwrap();
        let (egress, memory, f) = hosted_fetch(dir.path()).await;
        for (status, kind, body, says) in [
            (502, "text/html", "<html>Bad Gateway</html>", "502"),
            (429, "text/plain", "slow down", "429"),
            (
                200,
                "application/json",
                r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32001,"message":"unauthorized"}}"#,
                "unauthorized",
            ),
        ] {
            *f.discover.lock().unwrap() = Some((status, kind, body));
            let e = fetch(&egress, &memory).await.unwrap_err();
            assert!(e.contains(says), "{status}: {e}");
            assert_eq!(f.handshakes.load(SeqCst), 0, "{status}: {e}");
        }
        // none of them was kept as what the server speaks: the next answer is asked for
        *f.discover.lock().unwrap() = None;
        fetch(&egress, &memory).await.unwrap();
        assert_eq!(f.discovers.load(SeqCst), 4);
        assert_eq!(f.handshakes.load(SeqCst), 0);
    }

    /// A result of a kind this client does not take is no list of versions, whatever it
    /// carries, and is not kept as what the server speaks.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_discovery_result_of_a_kind_not_taken_is_refused_and_not_kept() {
        let dir = tempfile::tempdir().unwrap();
        let (egress, memory, f) = hosted_fetch(dir.path()).await;
        let more = r#"{"jsonrpc":"2.0","id":1,"result":{"resultType":"input_required","supportedVersions":["2026-07-28"],"inputRequests":{}}}"#;
        *f.discover.lock().unwrap() = Some((200, "application/json", more));
        let e = fetch(&egress, &memory).await.unwrap_err();
        assert!(e.contains("input_required"), "{e}");
        assert_eq!(f.calls.load(SeqCst), 0);
        *f.discover.lock().unwrap() = None;
        fetch(&egress, &memory).await.unwrap();
        assert_eq!(f.discovers.load(SeqCst), 2);
    }

    /// A reply that is not one of the revision kept for the server forgets it, so the
    /// next call asks again; a refusal of the call itself, with a success, does not.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_reply_not_of_the_revision_kept_asks_again_and_a_refused_call_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let (egress, memory, f) = hosted_fetch(dir.path()).await;
        fetch(&egress, &memory).await.unwrap();
        let j = "application/json";
        let unspoken = [
            (j, "<html>not JSON</html>"),
            ("text/event-stream", "data: nothing\n\n"),
            (j, r#"{"jsonrpc":"2.0","id":1}"#),
            (j, r#"{"jsonrpc":"2.0","id":1,"result":"done"}"#),
            (
                j,
                r#"{"jsonrpc":"2.0","id":1,"result":{"resultType":"input_required","inputRequests":{}}}"#,
            ),
            (
                j,
                r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32022,"message":"m","data":{"supported":["2027-01-01"]}}}"#,
            ),
            (
                j,
                r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"no such method"}}"#,
            ),
            (
                j,
                r#"{"jsonrpc":"2.0","id":1,"error":{"message":"no code"}}"#,
            ),
            (
                j,
                r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32700,"message":"parse error"}}"#,
            ),
            (
                j,
                r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32600,"message":"invalid request"}}"#,
            ),
        ];
        for (n, (kind, body)) in unspoken.into_iter().enumerate() {
            *f.answer.lock().unwrap() = Some((200, kind, body));
            assert!(fetch(&egress, &memory).await.is_err(), "{body}");
            *f.answer.lock().unwrap() = None;
            fetch(&egress, &memory).await.unwrap();
            assert_eq!(f.discovers.load(SeqCst), n + 2, "{body}");
        }
        let asked = f.discovers.load(SeqCst);
        let refused = [
            r#"{"jsonrpc":"2.0","id":1,"result":{"resultType":"complete","isError":true,"content":[{"type":"text","text":"refused: x"}]}}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":{"resultType":"complete","content":[{"type":"text","text":"not the tool's JSON"}]}}"#,
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"Unknown tool: fetch"}}"#,
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32603,"message":"internal error"}}"#,
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32001,"message":"unauthorized"}}"#,
        ];
        for body in refused {
            *f.answer.lock().unwrap() = Some((200, j, body));
            assert!(fetch(&egress, &memory).await.is_err(), "{body}");
            *f.answer.lock().unwrap() = None;
            fetch(&egress, &memory).await.unwrap();
            assert_eq!(f.discovers.load(SeqCst), asked, "{body}");
        }
        assert_eq!(f.handshakes.load(SeqCst), 0);
    }

    #[test]
    fn a_reply_is_read_from_json_or_an_event_stream() {
        let ok = json!({"jsonrpc": "2.0", "id": 2, "result": {"content": [{"type": "text",
            "text": json!({"answer": {"text": "hi"}, "reached": ["https://www.apache.org/"]}).to_string()}]}});
        let r = reply(&message("application/json", &ok.to_string()).unwrap()).unwrap();
        assert_eq!(r.reached, vec!["https://www.apache.org/".to_string()]);
        assert_eq!(
            serde_json::from_str::<Value>(&r.answer).unwrap()["text"],
            "hi"
        );
        let sse = format!("event: message\ndata: {ok}\n\n");
        assert_eq!(
            reply(&message("text/event-stream", &sse).unwrap()).unwrap(),
            r
        );
        let failed = json!({"result": {"isError": true, "content": [{"type": "text", "text": "refused: x"}]}});
        assert_eq!(reply(&failed).unwrap_err(), "refused: x");
        assert!(reply(&json!({"error": {"code": -32601}})).is_err());
        assert!(reply(&json!({"result": {"content": [{"type": "text", "text": "{}"}]}})).is_err());
        // a result that says it is complete is one; any other kind is not taken
        let mut done = ok.clone();
        done["result"]["resultType"] = json!("complete");
        assert_eq!(reply(&done).unwrap(), r);
        let mut more = ok.clone();
        more["result"]["resultType"] = json!("input_required");
        let e = reply(&more).unwrap_err();
        assert!(e.contains("input_required"), "{e}");
        assert!(message("application/json", "not json").is_err());
        assert!(message("text/event-stream", "data: nothing\n").is_err());
        let a = arguments("{\"url\": \"u\"}", "not json", &["h".into()]);
        assert_eq!(a["args"]["url"], "u");
        assert_eq!(a["config"], "not json");
        assert_eq!(a["hosts"][0], "h");
    }

    #[test]
    fn what_a_server_speaks_is_read_from_its_answer_to_discovery() {
        let result = |v: Value| {
            json!({"jsonrpc": "2.0", "id": 1,
                   "result": {"resultType": "complete", "supportedVersions": v}})
            .to_string()
        };
        let error = |code: i64, data: Value| {
            json!({"jsonrpc": "2.0", "id": 1, "error": {"code": code, "message": "m", "data": data}})
                .to_string()
        };
        let j = "application/json";
        let ok = |kind: &str, body: &str| era(200, kind, body);
        assert_eq!(ok(j, &result(json!(["2026-07-28"]))), Ok(Era::Stateless));
        let sse = format!("data: {}\n\n", result(json!(["2027-01-01", "2026-07-28"])));
        assert_eq!(ok("text/event-stream", &sse), Ok(Era::Stateless));
        assert_eq!(ok(j, &result(json!(["2025-11-25"]))), Ok(Era::Handshake));
        // refused for its version, naming only revisions with a handshake
        let older = json!({"supported": ["2025-06-18"], "requested": "2026-07-28"});
        assert_eq!(era(400, j, &error(-32022, older)), Ok(Era::Handshake));
        // a server from before: "method not found" in a success, as amem tool-server
        // answered, or a status of a request it cannot take with no error of 2026-07-28
        assert_eq!(ok(j, &error(-32601, Value::Null)), Ok(Era::Handshake));
        for status in [400, 404, 405, 415, 422] {
            assert_eq!(era(status, j, ""), Ok(Era::Handshake), "{status}");
            let words = "Unexpected message, expect initialize request";
            assert_eq!(era(status, "text/plain", words), Ok(Era::Handshake));
        }
        // 2026-07-28 refusing the request itself, or versions none of which are spoken
        let e = era(400, j, &error(-32020, Value::Null)).unwrap_err();
        assert!(e.contains("refused"), "{e}");
        let e = ok(j, &result(json!(["2027-01-01"]))).unwrap_err();
        assert!(e.contains("2027-01-01") && e.contains("2026-07-28"), "{e}");
        let e = era(400, j, &error(-32022, json!({"supported": ["2027-01-01"]}))).unwrap_err();
        assert!(e.contains("2027-01-01"), "{e}");
        // an answer that names no revision: an error page, a limit, a refusal of the
        // caller, a success that is no answer to discovery
        let page = "<html>".to_string() + &"Bad Gateway ".repeat(100) + "</html>";
        let e = era(502, "text/html", &page).unwrap_err();
        assert!(e.contains("502") && e.len() < 500, "{e}");
        for (status, body) in [
            (429, "slow down".to_string()),
            (401, "a bearer token is required".to_string()),
            (403, String::new()),
            (503, String::new()),
            (200, error(-32001, Value::Null)),
            (
                200,
                json!({"jsonrpc": "2.0", "id": 1, "result": {}}).to_string(),
            ),
            (202, String::new()),
        ] {
            let e = era(status, j, &body).unwrap_err();
            assert!(e.contains(&status.to_string()), "{e}");
        }
    }

    #[test]
    fn a_hosted_url_is_a_destination_and_a_path() {
        assert_eq!(
            split_url("https://tools.example.org/mcp").unwrap(),
            ("https://tools.example.org".into(), "/mcp".into())
        );
        assert_eq!(
            split_url("http://127.0.0.1:4100").unwrap(),
            ("http://127.0.0.1:4100".into(), String::new())
        );
        assert!(split_url("tools.example.org/mcp").is_err());
        assert!(split_url("https:///mcp").is_err());
    }
}
