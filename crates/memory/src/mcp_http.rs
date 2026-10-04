//! A small client for the memory server over MCP's streamable HTTP transport, for the
//! hook that runs inside the sandbox: one tool call, three requests, no session kept.
//! Plain HTTP/1.1 over a socket, since inside the sandbox the only server is the relay
//! on loopback and the only thing to say is one `tools/call`.

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// One request's answer: the status, the headers lower-cased, and the body de-chunked.
struct Answer {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Answer {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// `host:port` and the path of an `http://` URL.
fn split(url: &str) -> Result<(String, String), String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("{url:?} is not an http:// URL"))?;
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    if authority.is_empty() {
        return Err(format!("{url:?} has no host"));
    }
    Ok((authority.to_string(), format!("/{path}")))
}

/// The body of a chunked response, joined.
fn dechunk(raw: &str) -> String {
    let mut out = String::new();
    let mut rest = raw;
    loop {
        let Some((size, after)) = rest.split_once("\r\n") else {
            return out;
        };
        let size = size.split(';').next().unwrap_or("0").trim();
        let Ok(n) = usize::from_str_radix(size, 16) else {
            return out;
        };
        if n == 0 || after.len() < n {
            return out;
        }
        out.push_str(&after[..n]);
        rest = after[n..].strip_prefix("\r\n").unwrap_or("");
    }
}

/// POST `body` as JSON to `url` and read the whole answer; the connection is closed
/// after it, so the end of the stream is the end of the answer.
async fn post(url: &str, session: Option<&str>, body: &str) -> Result<Answer, String> {
    let (authority, path) = split(url)?;
    let mut s = TcpStream::connect(&authority)
        .await
        .map_err(|e| format!("{authority}: {e}"))?;
    let session = session
        .map(|id| format!("mcp-session-id: {id}\r\n"))
        .unwrap_or_default();
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: {authority}\r\nContent-Type: application/json\r\n\
         Accept: application/json, text/event-stream\r\n{session}Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).await.map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&raw);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| "no response".to_string())?;
    let mut lines = head.lines();
    let status: u16 = lines
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| "no status line".to_string())?;
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let chunked = headers
        .iter()
        .any(|(k, v)| k == "transfer-encoding" && v.contains("chunked"));
    let body = if chunked {
        dechunk(body)
    } else {
        body.to_string()
    };
    Ok(Answer {
        status,
        headers,
        body,
    })
}

/// The JSON-RPC messages in an answer: one JSON body, or the `data:` lines of an event
/// stream.
fn messages(answer: &Answer) -> Vec<Value> {
    if answer
        .header("content-type")
        .is_some_and(|c| c.contains("text/event-stream"))
    {
        answer
            .body
            .lines()
            .filter_map(|l| l.strip_prefix("data:"))
            .filter_map(|d| serde_json::from_str(d.trim()).ok())
            .collect()
    } else {
        serde_json::from_str(&answer.body).into_iter().collect()
    }
}

/// Call one tool on the memory server at `url`, the MCP endpoint, as a session of its
/// own. The tool's result on success; on a refusal, the server's words.
///
/// # Errors
///
/// The server unreachable, a protocol answer that is not one, or the tool's error.
pub async fn call_tool(url: &str, tool: &str, arguments: Value) -> Result<Value, String> {
    let init = json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": { "name": "memory-hook", "version": env!("CARGO_PKG_VERSION") }
        }
    });
    let opened = post(url, None, &init.to_string()).await?;
    if !(200..300).contains(&opened.status) {
        return Err(format!("initialize answered {}", opened.status));
    }
    let session = opened.header("mcp-session-id").map(str::to_string);
    let ready = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
    post(url, session.as_deref(), &ready.to_string()).await?;
    let call = json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": { "name": tool, "arguments": arguments }
    });
    let answered = post(url, session.as_deref(), &call.to_string()).await?;
    if !(200..300).contains(&answered.status) {
        return Err(format!(
            "{tool} answered {}: {}",
            answered.status, answered.body
        ));
    }
    let reply = messages(&answered)
        .into_iter()
        .find(|m| m["id"] == 2)
        .ok_or_else(|| format!("{tool}: no reply in {:?}", answered.body))?;
    if let Some(err) = reply.get("error") {
        return Err(err["message"].as_str().unwrap_or("error").to_string());
    }
    let result = reply["result"].clone();
    if result["isError"] == true {
        let text = result["content"][0]["text"].as_str().unwrap_or("refused");
        return Err(text.to_string());
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dm_egress::{Auth, Destination, Egress};
    use dm_server::{Memory, WriteRequest};
    use std::collections::HashMap;
    use std::sync::Arc;

    #[test]
    fn chunks_and_urls_are_read() {
        assert_eq!(dechunk("3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n"), "abcde");
        assert_eq!(dechunk("garbage"), "");
        assert_eq!(
            split("http://127.0.0.1:4100/mcp").unwrap(),
            ("127.0.0.1:4100".into(), "/mcp".into())
        );
        assert!(split("https://x/mcp").is_err());
        assert!(split("http:///mcp").is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_tool_call_reaches_the_server_and_a_refusal_comes_back_in_words() {
        let dir = tempfile::tempdir().unwrap();
        let m = Arc::new(
            Memory::init(&dir.path().join("m"), "human:test:me", "dom-me")
                .unwrap()
                .without_read_log(),
        );
        let w = m
            .write(WriteRequest {
                title: Some("Retry policy".into()),
                body: "Operation R retries with backoff.\n".into(),
                ..WriteRequest::default()
            })
            .await
            .unwrap();
        let mut d = HashMap::new();
        d.insert(
            "anthropic".to_string(),
            Destination::forward("http://127.0.0.1:1", Auth::PassThrough),
        );
        let egress = Egress::new(m.clone(), d, &dir.path().join("t.db")).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let app = egress.router();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let got = call_tool(&url, "get", json!({ "id": w.id })).await.unwrap();
        let text = got.to_string();
        assert!(text.contains("backoff"), "{text}");
        let err = call_tool(&url, "get", json!({ "id": "mem-me-nothere" }))
            .await
            .unwrap_err();
        assert!(err.contains("not found"), "{err}");
        let err = call_tool("http://127.0.0.1:1/mcp", "get", json!({ "id": "x" }))
            .await
            .unwrap_err();
        assert!(err.contains("127.0.0.1:1"), "{err}");
    }
}
