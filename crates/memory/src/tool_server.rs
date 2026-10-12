//! `amem tool-server` (PLAN M19): the tools of a root served over MCP's streamable
//! HTTP, so launchers elsewhere call them instead of carrying the components. The same
//! WASI component runs here as in a launcher, under this root's grant and with the
//! credential this host holds; what differs is who decides each request it makes.
//! There is no calling session here, so the call names the hosts its session may
//! reach, and a request goes only to a host both that list and this root's grant name
//! (`dm_taps::tool::HostGate`): a caller narrows what the server may reach, never
//! widens it. The answer carries every URL the tool reached, for the caller's memory
//! server to decide by its policy before it keeps anything.
//!
//! Off loopback the server wants a bearer token, so it is never an open proxy; in the
//! Foundation's deployment it sits behind the gateway's MCP registry.

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

/// The tools this root serves: granted as tools, run here (not hosted on), installed.
fn served(s: &Served) -> Vec<(&String, &TapGrant)> {
    s.grants
        .taps
        .iter()
        .filter(|(name, g)| {
            g.tool
                && g.hosted.is_none()
                && crate::taps::component_path(&s.root, name).is_ok_and(|p| p.is_file())
        })
        .collect()
}

fn rpc(id: &Value, result: Value) -> Response {
    Json(json!({"jsonrpc": "2.0", "id": id, "result": result})).into_response()
}

fn rpc_error(id: &Value, code: i64, message: &str) -> Response {
    Json(json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}))
        .into_response()
}

fn tool_error(id: &Value, message: &str) -> Response {
    rpc(
        id,
        json!({"isError": true, "content": [{"type": "text", "text": message}]}),
    )
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
        return rpc_error(&Value::Null, -32700, "not JSON");
    };
    let id = msg.get("id").cloned().unwrap_or(Value::Null);
    let method = msg["method"].as_str().unwrap_or_default();
    if msg.get("id").is_none() {
        // a notification: nothing to answer
        return StatusCode::ACCEPTED.into_response();
    }
    match method {
        "initialize" => rpc(
            &id,
            json!({
                "protocolVersion": msg["params"]["protocolVersion"].as_str().unwrap_or("2025-06-18"),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "amem tool-server", "version": env!("CARGO_PKG_VERSION")},
            }),
        ),
        "tools/list" => rpc(
            &id,
            json!({"tools": served(&s).iter().map(|(name, _)| json!({
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
            })).collect::<Vec<_>>()}),
        ),
        "tools/call" => call(&s, &id, &msg["params"]).await,
        _ => rpc_error(&id, -32601, &format!("no method {method}")),
    }
}

async fn call(s: &Served, id: &Value, params: &Value) -> Response {
    let name = params["name"].as_str().unwrap_or_default();
    let Some((_, grant)) = served(s).into_iter().find(|(n, _)| n.as_str() == name) else {
        return tool_error(id, &format!("no tool {name} is served here"));
    };
    let a = &params["arguments"];
    let Some(hosts) = a["hosts"].as_array() else {
        return tool_error(id, "the call names no hosts its session may reach");
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
    let Ok(path) = crate::taps::component_path(&s.root, name) else {
        return tool_error(id, &format!("no component installed as {name}"));
    };
    let c = dm_taps::tool::Call {
        tool: name,
        grant,
        credential: credential.as_deref(),
        config: &config,
        args: &args,
    };
    match dm_taps::tool::call_gated(Arc::new(gate), &path, c, LIMITS).await {
        Ok(done) => {
            let answer =
                serde_json::from_str::<Value>(&done.answer).unwrap_or(Value::String(done.answer));
            let body = json!({"answer": answer, "reached": done.reached});
            rpc(
                id,
                json!({"content": [{"type": "text", "text": body.to_string()}]}),
            )
        }
        Err(e) => tool_error(id, &e.to_string()),
    }
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
    let names: Vec<String> = served(&s).iter().map(|(n, _)| (*n).clone()).collect();
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
    let app = Router::new().route("/mcp", post(mcp)).with_state(s);
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_loopback_goes_without_a_token() {
        assert!(loopback("127.0.0.1:4100"));
        assert!(loopback("[::1]:4100"));
        assert!(loopback("localhost:4100"));
        assert!(!loopback("0.0.0.0:4100"));
        assert!(!loopback("10.1.2.3:4100"));
    }
}
