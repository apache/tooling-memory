//! `amem tool-server` (PLAN M19): the tools of a root served over MCP's streamable
//! HTTP, so launchers elsewhere call them instead of carrying the components. The same
//! WASI component runs here as in a launcher, under this root's grant and with the
//! credential this host holds, or, for a tool this project writes and trusts, fetch
//! today, the binary's own native code behind the same gate when no component is
//! installed under the name ([`dm_taps::fetch`]); what differs is who decides each
//! request it makes.
//! There is no calling session here, so the call names the hosts its session may
//! reach, and a request goes only to a host both that list and this root's grant name
//! (`dm_taps::tool::HostGate`): a caller narrows what the server may reach, never
//! widens it. The answer carries every URL the tool reached, for the caller's memory
//! server to decide by its policy before it keeps anything.
//!
//! Off loopback the server wants a bearer token, so it is never an open proxy; in the
//! Foundation's deployment it sits behind the gateway's MCP registry.

use crate::mcp_meta::{
    decode, named_by, supported, CLIENT_CAPABILITIES, HEADER_MISMATCH, LEGACY, METHOD_HEADER,
    MODERN, NAME_HEADER, PROTOCOL_VERSION, SERVER_INFO, UNSUPPORTED_VERSION, VERSION_HEADER,
};
use crate::Fallible;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use dm_policy::{Grants, TapGrant};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// One call may take this long, and its component grow to this much memory.
const LIMITS: dm_taps::Limits = dm_taps::Limits {
    wall: std::time::Duration::from_secs(60),
    memory: 128 << 20,
};

struct Served {
    root: PathBuf,
    grants: Grants,
    token: Option<String>,
}

/// The component installed under the root as `name`, if one is.
fn installed(root: &Path, name: &str) -> Option<PathBuf> {
    crate::taps::component_path(root, name)
        .ok()
        .filter(|p| p.is_file())
}

/// The tools this root serves: granted as tools, run here (not hosted on, and not the
/// owner's browser's), and either installed as a component or native to this binary.
fn served(s: &Served) -> Vec<(&String, &TapGrant)> {
    s.grants
        .taps
        .iter()
        .filter(|(name, g)| {
            g.tool
                && g.hosted.is_none()
                && !g.browser
                && (installed(&s.root, name).is_some() || dm_taps::fetch::is_native(name))
        })
        .collect()
}

/// Which revision a request is served under. One that names its version in `_meta`,
/// or asks `server/discover`, which only 2026-07-28 has, or mirrors a version newer
/// than the handshake's in its header, is served statelessly by 2026-07-28; anything
/// else as the revisions with `initialize` answer it, as this server always has.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Era {
    Handshake,
    Stateless,
}

fn era(method: &str, params: &Value, headers: &HeaderMap) -> Era {
    if method == "initialize" {
        return Era::Handshake;
    }
    let newer = match header(headers, VERSION_HEADER) {
        Ok(v) => v.is_some_and(|v| !LEGACY.contains(&v.as_str())),
        Err(_) => true,
    };
    if params["_meta"].get(PROTOCOL_VERSION).is_some() || method == "server/discover" || newer {
        Era::Stateless
    } else {
        Era::Handshake
    }
}

/// The one value of header `name`, if it was sent; more than one, or one that is not
/// text, is an error.
fn header(headers: &HeaderMap, name: &str) -> Result<Option<String>, Refusal> {
    let mut all = headers.get_all(name).iter();
    let Some(first) = all.next() else {
        return Ok(None);
    };
    if all.next().is_some() {
        return Err(Refusal::mismatch(format!("{name} is sent more than once")));
    }
    first
        .to_str()
        .map(|v| Some(v.to_string()))
        .map_err(|_| Refusal::mismatch(format!("{name} is not text")))
}

/// A request refused, before it is served.
struct Refusal {
    code: i64,
    message: String,
    data: Option<Value>,
}

impl Refusal {
    fn mismatch(message: String) -> Refusal {
        Refusal {
            code: HEADER_MISMATCH,
            message,
            data: None,
        }
    }

    fn invalid(message: String) -> Refusal {
        Refusal {
            code: -32602,
            message,
            data: None,
        }
    }

    /// A request without a handshake that names a version other than the one spoken
    /// that way. The versions it lists are the ones such a request may name; the older
    /// ones are reached through `initialize`, which the message says.
    fn unsupported(requested: &str) -> Refusal {
        Refusal {
            code: UNSUPPORTED_VERSION,
            message: format!(
                "Unsupported protocol version {requested}: a request without a handshake speaks {MODERN}; {} are reached through initialize",
                LEGACY.join(", ")
            ),
            data: Some(json!({"supported": [MODERN], "requested": requested})),
        }
    }

    /// The JSON-RPC error. A stateless request learns what kind from the status too:
    /// malformed, at odds with its headers, or of a version not spoken is a 400, a
    /// method unknown a 404. After a handshake every error went with a 200 as before,
    /// but headers at odds with the body and a body that is not one request, which are
    /// a 400 whoever sends them.
    fn answer(self, era: Era, id: &Value) -> Response {
        let mut error = json!({"code": self.code, "message": self.message});
        if let Some(data) = self.data {
            error["data"] = data;
        }
        let status = match (era, self.code) {
            (_, HEADER_MISMATCH | -32600)
            | (Era::Stateless, -32602 | -32021 | UNSUPPORTED_VERSION) => StatusCode::BAD_REQUEST,
            (Era::Stateless, -32601) => StatusCode::NOT_FOUND,
            _ => StatusCode::OK,
        };
        (
            status,
            Json(json!({"jsonrpc": "2.0", "id": id, "error": error})),
        )
            .into_response()
    }
}

fn server_info() -> Value {
    json!({"name": "amem tool-server", "version": env!("CARGO_PKG_VERSION")})
}

/// A result. To a stateless request it says it is complete and names this server,
/// since no handshake did.
fn answer(era: Era, id: &Value, mut result: Value) -> Response {
    if era == Era::Stateless {
        result["resultType"] = json!("complete");
        result["_meta"] = json!({ SERVER_INFO: server_info() });
    }
    Json(json!({"jsonrpc": "2.0", "id": id, "result": result})).into_response()
}

fn tool_error(message: &str) -> Value {
    json!({"isError": true, "content": [{"type": "text", "text": message}]})
}

/// What 2026-07-28 asks of a request before it is served: a version this server speaks
/// in `_meta`, mirrored in its header; the client's capabilities; the method, and the
/// name a call names, mirrored in theirs. A header missing is refused as one at odds:
/// that revision requires them, and something between may route on them.
fn checked(method: &str, params: &Value, headers: &HeaderMap) -> Result<(), Refusal> {
    let meta = &params["_meta"];
    let sent = header(headers, VERSION_HEADER)?;
    let Some(version) = meta[PROTOCOL_VERSION].as_str() else {
        return Err(match sent.filter(|v| !supported().contains(&v.as_str())) {
            Some(v) => Refusal::unsupported(&v),
            None => Refusal::invalid(format!("the request's _meta names no {PROTOCOL_VERSION}")),
        });
    };
    if version != MODERN {
        return Err(Refusal::unsupported(version));
    }
    match sent {
        Some(v) if v == version => {}
        Some(v) => {
            return Err(Refusal::mismatch(format!(
                "MCP-Protocol-Version {v} is not the {version} the body names"
            )))
        }
        None => return Err(Refusal::mismatch("MCP-Protocol-Version is missing".into())),
    }
    if !meta[CLIENT_CAPABILITIES].is_object() {
        return Err(Refusal::invalid(format!(
            "the request's _meta carries no {CLIENT_CAPABILITIES}"
        )));
    }
    if header(headers, METHOD_HEADER)?.is_none() {
        return Err(Refusal::mismatch("Mcp-Method is missing".into()));
    }
    if named_by(method).is_some() && header(headers, NAME_HEADER)?.is_none() {
        return Err(Refusal::mismatch("Mcp-Name is missing".into()));
    }
    Ok(())
}

/// Whether the method and name headers, where sent, say what the body says. Whoever
/// sends them, a request whose headers name another method or tool than its body is
/// refused: whatever routed on the headers acted on a different request than this one.
/// A client of the handshake may leave them out. The name is decoded before it is
/// compared; the method is not, since 2026-07-28 allows the base64 sentinel in
/// `Mcp-Name` and `Mcp-Param-*` only, and whatever routed on `Mcp-Method` read it as
/// sent.
fn mirrored(method: &str, params: &Value, headers: &HeaderMap) -> Result<(), Refusal> {
    if let Some(m) = header(headers, METHOD_HEADER)? {
        if m != method {
            return Err(Refusal::mismatch(format!(
                "Mcp-Method {m} is not the {method} the body names"
            )));
        }
    }
    if let Some(sent) = header(headers, NAME_HEADER)? {
        let named = named_by(method).and_then(|key| params[key].as_str());
        if named.is_none() || decode(&sent).as_deref() != named {
            return Err(Refusal::mismatch(format!(
                "Mcp-Name {sent} is not the {} the body names",
                named.unwrap_or("nothing")
            )));
        }
    }
    Ok(())
}

/// The answer to `initialize`: the version asked when it is one with a handshake this
/// server speaks, else the newest of those, for the client to take or leave.
fn initialized(params: &Value) -> Value {
    let asked = params["protocolVersion"].as_str().unwrap_or_default();
    let version = LEGACY
        .iter()
        .find(|v| **v == asked)
        .copied()
        .unwrap_or(LEGACY[0]);
    json!({
        "protocolVersion": version,
        "capabilities": {"tools": {}},
        "serverInfo": server_info(),
    })
}

/// The answer to `server/discover`. Nothing in it is promised fresh for any time, and
/// with a token in front of the server none of it is for a shared cache.
fn discovered() -> Value {
    json!({
        "supportedVersions": supported(),
        "capabilities": {"tools": {}},
        "ttlMs": 0,
        "cacheScope": "private",
    })
}

/// The answer to `tools/list`. What is served follows what is installed under the root
/// at the time of asking, so it is not promised fresh either.
fn listed(s: &Served) -> Value {
    json!({
        "tools": served(s).iter().map(|(name, _)| json!({
            "name": name,
            "description": format!("The {name} tool, run under this server's grant; answers with the URLs it reached"),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "args": {"type": "object", "description": "the tool's own arguments"},
                    "config": {"description": "the caller's configuration for the tool"},
                    "hosts": {"type": "array", "items": {"type": "string"},
                              "description": "the hosts the calling session may reach"},
                },
                "required": ["args", "hosts"],
            },
        })).collect::<Vec<_>>(),
        "ttlMs": 0,
        "cacheScope": "private",
    })
}

async fn mcp(State(s): State<Arc<Served>>, headers: HeaderMap, body: String) -> Response {
    if let Some(t) = &s.token {
        let given = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        if given != Some(t.as_str()) {
            return (StatusCode::UNAUTHORIZED, "a bearer token is required").into_response();
        }
    }
    let Ok(msg) = serde_json::from_str::<Value>(&body) else {
        let refused = Refusal {
            code: -32700,
            message: "not JSON".into(),
            data: None,
        };
        return refused.answer(Era::Handshake, &Value::Null);
    };
    if !msg.is_object() {
        // one request a POST: a batch, or JSON that is no request, is not one
        let refused = Refusal {
            code: -32600,
            message: "the body is not one JSON-RPC request".into(),
            data: None,
        };
        return refused.answer(Era::Handshake, &Value::Null);
    }
    let id = msg.get("id").cloned().unwrap_or(Value::Null);
    let method = msg["method"].as_str().unwrap_or_default();
    if msg.get("id").is_none() {
        // a notification: nothing to answer
        return StatusCode::ACCEPTED.into_response();
    }
    let params = &msg["params"];
    let era = era(method, params, &headers);
    let ready = match era {
        Era::Stateless => {
            mirrored(method, params, &headers).and_then(|()| checked(method, params, &headers))
        }
        Era::Handshake => mirrored(method, params, &headers),
    };
    if let Err(refused) = ready {
        return refused.answer(era, &id);
    }
    match method {
        "initialize" => answer(era, &id, initialized(params)),
        "server/discover" => answer(era, &id, discovered()),
        "tools/list" => answer(era, &id, listed(&s)),
        "tools/call" => answer(era, &id, call(&s, params).await),
        _ => Refusal {
            code: -32601,
            message: format!("no method {method}"),
            data: None,
        }
        .answer(era, &id),
    }
}

async fn call(s: &Served, params: &Value) -> Value {
    let name = params["name"].as_str().unwrap_or_default();
    let Some((_, grant)) = served(s).into_iter().find(|(n, _)| n.as_str() == name) else {
        return tool_error(&format!("no tool {name} is served here"));
    };
    let a = &params["arguments"];
    let Some(hosts) = a["hosts"].as_array() else {
        return tool_error("the call names no hosts its session may reach");
    };
    let text = |v: &Value| match v {
        Value::String(t) => t.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    };
    let config = match &a["config"] {
        Value::Null => grant.config.clone().unwrap_or_else(|| "{}".into()),
        v => text(v),
    };
    let args = text(&a["args"]);
    let mut granted = grant.hosts.clone();
    if let Some(t) = &grant.trusted {
        granted.extend(t.hosts.iter().cloned());
    }
    let gate = dm_taps::tool::HostGate {
        granted,
        caller: hosts
            .iter()
            .filter_map(|h| h.as_str().map(str::to_string))
            .collect(),
        methods: grant.methods.clone(),
        loopback: grant.loopback,
    };
    let credential = grant
        .credential
        .as_deref()
        .and_then(|var| crate::present(std::env::var(var).ok()));
    let c = dm_taps::tool::Call {
        tool: name,
        grant,
        credential: credential.as_deref(),
        config: &config,
        args: &args,
    };
    let gate = Arc::new(gate);
    // a component installed under the name runs; without one, the binary's own fetch
    let done = match installed(&s.root, name) {
        Some(path) => dm_taps::tool::call_gated(gate, &path, c, LIMITS).await,
        None => dm_taps::fetch::call(gate, c, LIMITS).await,
    };
    match done {
        Ok(done) => {
            let answer =
                serde_json::from_str::<Value>(&done.answer).unwrap_or(Value::String(done.answer));
            let body = json!({"answer": answer, "reached": done.reached});
            json!({"content": [{"type": "text", "text": body.to_string()}]})
        }
        Err(e) => tool_error(&e.to_string()),
    }
}

/// The MCP endpoint, at `/mcp`.
fn router(s: Arc<Served>) -> Router {
    Router::new().route("/mcp", post(mcp)).with_state(s)
}

/// Whether an address to listen on is this machine's loopback.
fn loopback(listen: &str) -> bool {
    listen
        .parse::<std::net::SocketAddr>()
        .is_ok_and(|a| a.ip().is_loopback())
        || listen.starts_with("localhost:")
}

/// `amem tool-server`: serve this root's tools until stopped.
///
/// # Errors
///
/// No record at `root`, a token variable that is unset, a listen address off loopback
/// without a token, or one that cannot be bound.
pub async fn serve(root: &Path, listen: &str, token_env: Option<&str>) -> Fallible {
    let grants = Grants::load(&root.join("grants.toml"))?;
    let token = match token_env {
        Some(var) => Some(
            crate::present(std::env::var(var).ok())
                .ok_or_else(|| format!("--token-env {var}: the variable is not set"))?,
        ),
        None => None,
    };
    if token.is_none() && !loopback(listen) {
        return Err(format!(
            "{listen} is off loopback: a tool server there wants --token-env, so it is never an open proxy"
        )
        .into());
    }
    let s = Arc::new(Served {
        root: root.to_path_buf(),
        grants,
        token,
    });
    let names: Vec<String> = served(&s)
        .iter()
        .map(|(n, _)| match installed(root, n) {
            Some(_) => (*n).clone(),
            None => format!("{n} (native)"),
        })
        .collect();
    let listener = tokio::net::TcpListener::bind(listen).await?;
    eprintln!(
        "tool-server: serving {} at http://{}/mcp",
        if names.is_empty() {
            "no tools".to_string()
        } else {
            names.join(", ")
        },
        listener.local_addr()?
    );
    axum::serve(listener, router(s)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_meta::{self, MODERN, PROTOCOL_VERSION};
    use tower::ServiceExt as _;

    /// A server for fetch, the binary's own, on a root with nothing installed.
    fn app() -> Router {
        let grants = Grants::parse(
            "owner = \"human:test:me\"\n[domains.dom-ts]\n\
             [taps.fetch]\ndomain = \"dom-ts\"\ntool = true\nceiling = \"public\"\nhosts = [\"127.0.0.1\"]\n",
        )
        .unwrap();
        router(Arc::new(Served {
            root: PathBuf::from("/nonexistent"),
            grants,
            token: None,
        }))
    }

    /// POST `body` with `headers`; the status and the JSON answered, `null` for none.
    async fn post_to(app: &Router, headers: &[(&str, String)], body: &Value) -> (u16, Value) {
        let mut req = axum::http::Request::post("/mcp")
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream");
        for (k, v) in headers {
            req = req.header(*k, v);
        }
        let resp = app
            .clone()
            .oneshot(req.body(axum::body::Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = resp.status().as_u16();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    /// A 2026-07-28 request and the headers it goes with.
    fn modern(method: &str, params: Value) -> (Value, Vec<(&'static str, String)>) {
        let r = mcp_meta::request(1, method, params, "test");
        let h = mcp_meta::headers(&r);
        (r, h)
    }

    fn call_fetch() -> Value {
        json!({"name": "fetch", "arguments": {"args": {"url": "http://127.0.0.1:1/"}, "hosts": []}})
    }

    #[tokio::test]
    async fn discovery_names_every_version_spoken_and_the_tools() {
        let app = app();
        let (r, h) = modern("server/discover", Value::Null);
        let (status, got) = post_to(&app, &h, &r).await;
        assert_eq!(status, 200, "{got}");
        let result = &got["result"];
        assert_eq!(result["resultType"], "complete", "{got}");
        assert_eq!(
            result["supportedVersions"],
            json!(["2026-07-28", "2025-11-25", "2025-06-18", "2025-03-26"]),
            "{got}"
        );
        assert!(result["capabilities"]["tools"].is_object(), "{got}");
        assert_eq!(
            result["_meta"][mcp_meta::SERVER_INFO]["name"],
            "amem tool-server",
            "{got}"
        );
    }

    #[tokio::test]
    async fn a_stateless_request_is_served_and_its_result_says_it_is_complete() {
        let app = app();
        let (r, h) = modern("tools/list", Value::Null);
        let (status, got) = post_to(&app, &h, &r).await;
        assert_eq!(status, 200, "{got}");
        assert_eq!(got["result"]["resultType"], "complete", "{got}");
        assert_eq!(got["result"]["tools"][0]["name"], "fetch", "{got}");
        assert_eq!(got["result"]["ttlMs"], 0, "{got}");
        assert_eq!(got["result"]["cacheScope"], "private", "{got}");
        // a tool's refusal is a complete result too, marked as an error
        let (r, h) = modern("tools/call", call_fetch());
        let (status, got) = post_to(&app, &h, &r).await;
        assert_eq!(status, 200, "{got}");
        assert_eq!(got["result"]["resultType"], "complete", "{got}");
        assert_eq!(got["result"]["isError"], true, "{got}");
        // a name that is not plain in a header goes as base64 and is read back
        let (r, mut h) = modern("tools/call", call_fetch());
        h.retain(|(k, _)| *k != mcp_meta::NAME_HEADER);
        h.push((mcp_meta::NAME_HEADER, "=?base64?ZmV0Y2g=?=".into()));
        let (status, got) = post_to(&app, &h, &r).await;
        assert_eq!(status, 200, "{got}");
    }

    #[tokio::test]
    async fn an_unsupported_version_is_refused_naming_the_supported_ones() {
        let app = app();
        let (mut r, mut h) = modern("tools/list", Value::Null);
        r["params"]["_meta"][PROTOCOL_VERSION] = json!("1900-01-01");
        h[0].1 = "1900-01-01".into();
        let (status, got) = post_to(&app, &h, &r).await;
        assert_eq!(status, 400, "{got}");
        assert_eq!(got["error"]["code"], -32022, "{got}");
        assert_eq!(got["error"]["data"]["requested"], "1900-01-01", "{got}");
        // a stateless request is answered with the stateless versions only; the older
        // ones are reached through initialize, which the message says
        assert_eq!(got["error"]["data"]["supported"], json!([MODERN]), "{got}");
        let said = got["error"]["message"].as_str().unwrap_or_default();
        assert!(said.contains("initialize"), "{got}");
        // a version with a handshake named in _meta is no stateless version either
        let (mut r, mut h) = modern("tools/list", Value::Null);
        r["params"]["_meta"][PROTOCOL_VERSION] = json!("2025-06-18");
        h[0].1 = "2025-06-18".into();
        let (status, got) = post_to(&app, &h, &r).await;
        assert_eq!(status, 400, "{got}");
        assert_eq!(got["error"]["code"], -32022, "{got}");
        assert_eq!(got["error"]["data"]["supported"], json!([MODERN]), "{got}");
    }

    #[tokio::test]
    async fn a_body_that_is_not_one_request_is_refused_as_invalid() {
        let app = app();
        let one = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
        for body in [json!([one]), json!([]), json!("tools/list"), json!(7)] {
            let (status, got) = post_to(&app, &[], &body).await;
            assert_eq!(status, 400, "{body}: {got}");
            assert_eq!(got["error"]["code"], -32600, "{body}: {got}");
            assert_eq!(got["id"], Value::Null, "{body}: {got}");
        }
    }

    /// A name header at odds with the body is refused whoever sends it; without one, a
    /// client of the handshake is served as before.
    #[tokio::test]
    async fn a_name_header_at_odds_with_the_body_is_refused_after_a_handshake_too() {
        let app = app();
        let call =
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": call_fetch()});
        let method = (mcp_meta::METHOD_HEADER, "tools/call".to_string());
        let other = (mcp_meta::NAME_HEADER, "search".to_string());
        let version = (mcp_meta::VERSION_HEADER, "2025-06-18".to_string());
        for headers in [
            vec![method.clone(), other.clone()],
            vec![method.clone(), other.clone(), version.clone()],
            vec![other.clone()],
        ] {
            let (status, got) = post_to(&app, &headers, &call).await;
            assert_eq!(status, 400, "{headers:?}: {got}");
            assert_eq!(got["error"]["code"], mcp_meta::HEADER_MISMATCH, "{got}");
        }
        let same = (mcp_meta::NAME_HEADER, "=?base64?ZmV0Y2g=?=".to_string());
        for headers in [vec![], vec![version, same]] {
            let (status, got) = post_to(&app, &headers, &call).await;
            assert_eq!(status, 200, "{headers:?}: {got}");
            assert_eq!(got["result"]["isError"], true, "{got}");
        }
    }

    /// Each way a 2026-07-28 request's headers fail its body, and what is answered.
    #[tokio::test]
    async fn headers_missing_or_at_odds_with_the_body_are_refused() {
        let app = app();
        let refused = |status: u16, got: &Value, code: i64| {
            assert_eq!(status, 400, "{got}");
            assert_eq!(got["error"]["code"], code, "{got}");
            assert_eq!(got["id"], 1, "{got}");
        };
        let without = |h: &[(&'static str, String)], name: &str| {
            h.iter()
                .filter(|(k, _)| *k != name)
                .cloned()
                .collect::<Vec<_>>()
        };
        let (r, h) = modern("tools/call", call_fetch());
        for name in [
            mcp_meta::VERSION_HEADER,
            mcp_meta::METHOD_HEADER,
            mcp_meta::NAME_HEADER,
        ] {
            let (status, got) = post_to(&app, &without(&h, name), &r).await;
            refused(status, &got, mcp_meta::HEADER_MISMATCH);
        }
        let mut other = without(&h, mcp_meta::NAME_HEADER);
        other.push((mcp_meta::NAME_HEADER, "search".into()));
        let (status, got) = post_to(&app, &other, &r).await;
        refused(status, &got, mcp_meta::HEADER_MISMATCH);
        let mut other = without(&h, mcp_meta::METHOD_HEADER);
        other.push((mcp_meta::METHOD_HEADER, "tools/list".into()));
        let (status, got) = post_to(&app, &other, &r).await;
        refused(status, &got, mcp_meta::HEADER_MISMATCH);
        let mut other = without(&h, mcp_meta::VERSION_HEADER);
        other.push((mcp_meta::VERSION_HEADER, "2025-06-18".into()));
        let (status, got) = post_to(&app, &other, &r).await;
        refused(status, &got, mcp_meta::HEADER_MISMATCH);
        // the capabilities every request carries are not optional
        let mut bare = r.clone();
        bare["params"]["_meta"]
            .as_object_mut()
            .unwrap()
            .remove(mcp_meta::CLIENT_CAPABILITIES);
        let (status, got) = post_to(&app, &h, &bare).await;
        refused(status, &got, -32602);
        // and a method this version does not have is not found
        let (r, h) = modern("ping", Value::Null);
        let (status, got) = post_to(&app, &h, &r).await;
        assert_eq!(status, 404, "{got}");
        assert_eq!(got["error"]["code"], -32601, "{got}");
    }

    /// 2026-07-28 lets the sentinel stand for a value in `Mcp-Name` and `Mcp-Param-*`
    /// only, so a method header in it names another method than the body's.
    #[tokio::test]
    async fn a_method_header_is_compared_as_sent_and_never_decoded() {
        let app = app();
        let (r, mut h) = modern("tools/list", Value::Null);
        h.retain(|(k, _)| *k != mcp_meta::METHOD_HEADER);
        h.push((
            mcp_meta::METHOD_HEADER,
            "=?base64?dG9vbHMvbGlzdA==?=".into(),
        ));
        let (status, got) = post_to(&app, &h, &r).await;
        assert_eq!(status, 400, "{got}");
        assert_eq!(got["error"]["code"], mcp_meta::HEADER_MISMATCH, "{got}");
    }

    #[tokio::test]
    async fn a_client_that_opens_with_initialize_is_answered_as_before() {
        let app = app();
        let init = |v: &str| {
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                   "params": {"protocolVersion": v, "capabilities": {},
                              "clientInfo": {"name": "t", "version": "0"}}})
        };
        let (status, got) = post_to(&app, &[], &init("2025-06-18")).await;
        assert_eq!(status, 200, "{got}");
        assert_eq!(got["result"]["protocolVersion"], "2025-06-18", "{got}");
        assert!(got["result"].get("resultType").is_none(), "{got}");
        // a version it does not speak is answered with the newest it does
        let (_, got) = post_to(&app, &[], &init("2024-11-05")).await;
        assert_eq!(got["result"]["protocolVersion"], "2025-11-25", "{got}");
        let (_, got) = post_to(&app, &[], &init(MODERN)).await;
        assert_eq!(got["result"]["protocolVersion"], "2025-11-25", "{got}");
        // a method header that contradicts the body is refused on this path too
        let (status, got) = post_to(
            &app,
            &[(mcp_meta::METHOD_HEADER, "tools/call".into())],
            &init("2025-06-18"),
        )
        .await;
        assert_eq!(status, 400, "{got}");
        assert_eq!(got["error"]["code"], mcp_meta::HEADER_MISMATCH, "{got}");
        let list = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"});
        let (status, got) = post_to(
            &app,
            &[(mcp_meta::VERSION_HEADER, "2025-06-18".into())],
            &list,
        )
        .await;
        assert_eq!(status, 200, "{got}");
        assert_eq!(got["result"]["tools"][0]["name"], "fetch", "{got}");
        assert!(got["result"].get("resultType").is_none(), "{got}");
    }

    #[test]
    fn only_loopback_goes_without_a_token() {
        assert!(loopback("127.0.0.1:4100"));
        assert!(loopback("[::1]:4100"));
        assert!(loopback("localhost:4100"));
        assert!(!loopback("0.0.0.0:4100"));
        assert!(!loopback("10.1.2.3:4100"));
    }
}
