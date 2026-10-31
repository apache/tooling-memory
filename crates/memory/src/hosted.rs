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
/// fails on the way forgets it, so the next one asks again.
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

/// What a tool server's answer to `server/discover` says it speaks. A result listing
/// 2026-07-28 is that revision; one listing only revisions with a handshake, or that
/// revision's refusal of the version naming only those, is one of them. Anything else
/// no server of 2026-07-28 answers, so it comes from one before it that has no such
/// method: an error status without one of that revision's errors, a method unknown,
/// nothing at all.
///
/// # Errors
///
/// That revision's refusal of the request itself, or a list of versions none of
/// which this launcher speaks.
fn era(content_type: &str, body: &str) -> Result<Era, String> {
    let Ok(m) = message(content_type, body) else {
        return Ok(Era::Handshake);
    };
    let listed = if let Some(e) = m.get("error") {
        let code = e["code"].as_i64().unwrap_or_default();
        if !MODERN_ERRORS.contains(&code) {
            return Ok(Era::Handshake);
        }
        if code != UNSUPPORTED_VERSION {
            return Err(format!("the tool server refused server/discover: {e}"));
        }
        &e["data"]["supported"]
    } else {
        &m["result"]["supportedVersions"]
    };
    let Some(listed) = listed.as_array() else {
        return Ok(Era::Handshake);
    };
    let listed: Vec<&str> = listed.iter().filter_map(Value::as_str).collect();
    if listed.contains(&MODERN) && m.get("error").is_none() {
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
        era(&sent.content_type, &sent.body)
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
    checked(
        &memory,
        tool,
        reply(&message(&sent.content_type, &sent.body)?)?,
    )
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
        assert_eq!(era(j, &result(json!(["2026-07-28"]))), Ok(Era::Stateless));
        let sse = format!("data: {}\n\n", result(json!(["2027-01-01", "2026-07-28"])));
        assert_eq!(era("text/event-stream", &sse), Ok(Era::Stateless));
        assert_eq!(era(j, &result(json!(["2025-11-25"]))), Ok(Era::Handshake));
        // refused for its version, naming only revisions with a handshake
        let older = json!({"supported": ["2025-06-18"], "requested": "2026-07-28"});
        assert_eq!(era(j, &error(-32022, older)), Ok(Era::Handshake));
        // a server from before: a method unknown, nothing, a refusal in words, no list
        assert_eq!(era(j, &error(-32601, Value::Null)), Ok(Era::Handshake));
        assert_eq!(era(j, ""), Ok(Era::Handshake));
        assert_eq!(
            era("text/plain", "Bad Request: Session not found"),
            Ok(Era::Handshake)
        );
        let bare = json!({"jsonrpc": "2.0", "id": 1, "result": {}}).to_string();
        assert_eq!(era(j, &bare), Ok(Era::Handshake));
        // 2026-07-28 refusing the request itself, or versions none of which are spoken
        let e = era(j, &error(-32020, Value::Null)).unwrap_err();
        assert!(e.contains("refused"), "{e}");
        let e = era(j, &result(json!(["2027-01-01"]))).unwrap_err();
        assert!(e.contains("2027-01-01") && e.contains("2026-07-28"), "{e}");
        let e = era(j, &error(-32022, json!({"supported": ["2027-01-01"]}))).unwrap_err();
        assert!(e.contains("2027-01-01"), "{e}");
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
