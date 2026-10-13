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
//! The call leaves through the egress ([`Egress::post`]), as a model call does: the
//! session's label is held to the tool's ceiling before anything is sent, the server's
//! bearer goes on there, and every request is metered and traced with its status and
//! latency. The harness itself cannot reach the server that way; a tool server is a
//! destination of the launcher's, not of the sandbox's.

use dm_egress::{Auth, Destination, Egress};
use dm_policy::Grants;
use dm_server::sources::Answered;
use dm_server::Memory;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

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

/// Call `tool` on its tool server for the session `memory` is, through `egress`.
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
    let post = |body: Value, session: Option<String>| {
        let path = path.clone();
        async move {
            let mut headers = axum::http::HeaderMap::new();
            headers.insert(
                "content-type",
                "application/json".parse().map_err(|_| "header")?,
            );
            headers.insert(
                "accept",
                "application/json, text/event-stream"
                    .parse()
                    .map_err(|_| "header")?,
            );
            if let Some(s) = session {
                headers.insert(
                    "mcp-session-id",
                    s.parse()
                        .map_err(|_| "the tool server's session id is not a header")?,
                );
            }
            let r = egress
                .post(tool, &path, headers, body.to_string().into())
                .await
                .map_err(|e| e.to_string())?;
            if !(200..300).contains(&r.status) {
                return Err(format!(
                    "the tool server answered {}: {}",
                    r.status,
                    r.body.trim()
                ));
            }
            let content_type = r
                .headers
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string();
            let id = r
                .headers
                .get("mcp-session-id")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            Ok::<_, String>((content_type, id, r.body))
        }
    };
    let (_, session, _) = post(
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                       "clientInfo": {"name": "amem", "version": env!("CARGO_PKG_VERSION")}}
        }),
        None,
    )
    .await?;
    post(
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        session.clone(),
    )
    .await?;
    let hosts = memory.fetch_hosts(tool);
    let (content_type, _, body) = post(
        json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": tool, "arguments": arguments(args, config, &hosts)}
        }),
        session,
    )
    .await?;
    checked(&memory, tool, reply(&message(&content_type, &body)?)?)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(message("application/json", "not json").is_err());
        assert!(message("text/event-stream", "data: nothing\n").is_err());
        let a = arguments("{\"url\": \"u\"}", "not json", &["h".into()]);
        assert_eq!(a["args"]["url"], "u");
        assert_eq!(a["config"], "not json");
        assert_eq!(a["hosts"][0], "h");
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
