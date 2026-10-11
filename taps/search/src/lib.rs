//! The search tool tap: a web search for an agent, handed back as a list of hits.
//!
//! An agent calls it through the memory server mid-turn. It sends one GET to the
//! configured search endpoint with the query and a count, and reads either of two
//! answer shapes: Brave Search's, `{"web": {"results": [{"title", "url",
//! "description"}]}}`, or SearXNG's, `{"results": [{"title", "url", "content"}]}`.
//! Titles and snippets come back as plain text, markup stripped and entities decoded.
//!
//! The tap decides nothing about where it may go. The host decides the request before
//! it leaves: the scheme, the method, the endpoint's host against the tool's grant,
//! the address it resolves to, and the label of what the calling session has read,
//! since the query itself is data leaving. The search provider's key, where it needs
//! one, is put on the request by the host from the variable the grant names; the tap
//! never holds it. What the tap returns is scanned and kept by the memory server before
//! the agent sees it.
//!
//! Config, JSON, all optional: `{"endpoint":
//! "https://api.search.brave.com/res/v1/web/search", "count": 5}`.
//! Args, JSON: `{"query": "...", "count": 5}`, `count` optional and at most 20.
//! Answer, JSON: `{"query", "results": [{"title", "url", "snippet"}]}`.

wit_bindgen::generate!({
    path: "../../crates/taps/wit",
    world: "tool",
});

use serde::Deserialize;
use serde_json::Value;
use wasi::http::outgoing_handler;
use wasi::http::types::{Fields, Method, OutgoingRequest, Scheme};
use wasi::io::streams::StreamError;

const USER_AGENT: &str = "durable-memory-search/0.1";
/// The most hits one call returns.
const MAX_COUNT: usize = 20;
/// The most of an answer read, in bytes.
const MAX_BYTES: usize = 4 << 20;

#[derive(Deserialize)]
struct Config {
    #[serde(default = "brave")]
    endpoint: String,
    #[serde(default = "five")]
    count: usize,
}

fn brave() -> String {
    "https://api.search.brave.com/res/v1/web/search".into()
}

fn five() -> usize {
    5
}

#[derive(Deserialize)]
struct Args {
    query: String,
    #[serde(default)]
    count: Option<usize>,
}

/// `scheme`, `authority` and `path?query` of an http or https URL.
fn split(url: &str) -> Result<(Scheme, String, String), String> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| format!("{url:?} is not an absolute URL"))?;
    let scheme = match scheme.to_ascii_lowercase().as_str() {
        "https" => Scheme::Https,
        "http" => Scheme::Http,
        other => return Err(format!("{other}: is not searched; http and https only")),
    };
    let (authority, path) = match rest.find(['/', '?']) {
        Some(i) if rest.as_bytes()[i] == b'/' => (rest[..i].to_string(), rest[i..].to_string()),
        Some(i) => (rest[..i].to_string(), format!("/{}", &rest[i..])),
        None => (rest.to_string(), "/".to_string()),
    };
    if authority.is_empty() {
        return Err(format!("{url:?} has no host"));
    }
    Ok((scheme, authority, path))
}

/// GET one URL for JSON: its status and body as text, up to [`MAX_BYTES`].
fn get(url: &str) -> Result<(u16, String), String> {
    let (scheme, authority, path) = split(url)?;
    let headers = Fields::from_list(&[
        ("accept".to_string(), b"application/json".to_vec()),
        ("user-agent".to_string(), USER_AGENT.as_bytes().to_vec()),
    ])
    .map_err(|e| format!("headers: {e}"))?;
    let req = OutgoingRequest::new(headers);
    req.set_method(&Method::Get).map_err(|()| "method")?;
    req.set_scheme(Some(&scheme)).map_err(|()| "scheme")?;
    req.set_authority(Some(&authority))
        .map_err(|()| "authority")?;
    req.set_path_with_query(Some(&path)).map_err(|()| "path")?;
    let pending =
        outgoing_handler::handle(req, None).map_err(|e| format!("request to {url}: {e}"))?;
    pending.subscribe().block();
    let response = pending
        .get()
        .ok_or("no response")?
        .map_err(|()| "response taken twice")?
        .map_err(|e| format!("request to {url}: {e}"))?;
    let status = response.status();
    let body = response.consume().map_err(|()| "body")?;
    let stream = body.stream().map_err(|()| "stream")?;
    let mut bytes = Vec::new();
    loop {
        match stream.blocking_read(64 * 1024) {
            Ok(chunk) => {
                bytes.extend(chunk);
                if bytes.len() > MAX_BYTES {
                    return Err(format!("the answer from {url} is over {MAX_BYTES} bytes"));
                }
            }
            Err(StreamError::Closed) => break,
            Err(StreamError::LastOperationFailed(e)) => {
                return Err(format!("reading {url}: {}", e.to_debug_string()))
            }
        }
    }
    drop(stream);
    Ok((status, String::from_utf8_lossy(&bytes).into_owned()))
}

/// A query string value: unreserved characters kept, every other byte as `%XX`, a space
/// as `%20`.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The search URL for a query and a count.
fn search_url(endpoint: &str, query: &str, count: usize) -> String {
    let joiner = if endpoint.contains('?') { '&' } else { '?' };
    format!("{endpoint}{joiner}q={}&count={count}", encode(query))
}

/// One entity's character, without its `&` and `;`, if it is one this tap knows.
fn entity(name: &str) -> Option<char> {
    match name {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some(' '),
        _ => {
            let num = name.strip_prefix('#')?;
            let code = match num.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => num.parse().ok()?,
            };
            char::from_u32(code).map(|c| if c == '\u{a0}' { ' ' } else { c })
        }
    }
}

/// Entities decoded: the named few and every numeric one.
fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        let decoded = after
            .find(';')
            .filter(|&semi| semi > 0 && semi <= 10)
            .and_then(|semi| entity(&after[..semi]).map(|c| (c, semi)));
        match decoded {
            Some((c, semi)) => {
                out.push(c);
                rest = &after[semi + 1..];
            }
            None => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Markup stripped, entities decoded, whitespace collapsed: a title or snippet as text.
fn plain(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    decode_entities(&out)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The hits in a Brave or a SearXNG answer, as `{title, url, snippet}`.
fn hits(answer: &Value) -> Vec<Value> {
    let list = answer
        .pointer("/web/results")
        .or_else(|| answer.get("results"))
        .and_then(Value::as_array);
    let field = |r: &Value, names: &[&str]| -> String {
        names
            .iter()
            .find_map(|n| r.get(*n).and_then(Value::as_str))
            .map(plain)
            .unwrap_or_default()
    };
    list.into_iter()
        .flatten()
        .filter_map(|r| {
            let url = r.get("url").and_then(Value::as_str)?;
            Some(serde_json::json!({
                "title": field(r, &["title"]),
                "url": url,
                "snippet": field(r, &["description", "content", "snippet"]),
            }))
        })
        .collect()
}

struct Component;

impl Guest for Component {
    fn call(config: String, args: String) -> Result<String, String> {
        let config: Config = if config.trim().is_empty() {
            serde_json::from_str("{}")
        } else {
            serde_json::from_str(&config)
        }
        .map_err(|e| format!("config: {e}"))?;
        let args: Args = serde_json::from_str(&args).map_err(|e| format!("args: {e}"))?;
        let query = args.query.trim();
        if query.is_empty() {
            return Err("an empty query".into());
        }
        let count = args.count.unwrap_or(config.count).clamp(1, MAX_COUNT);
        let (status, text) = get(&search_url(&config.endpoint, query, count))?;
        if status >= 400 {
            return Err(format!("HTTP {status} from search"));
        }
        let answer: Value =
            serde_json::from_str(&text).map_err(|e| format!("the search answer: {e}"))?;
        let mut results = hits(&answer);
        results.truncate(count);
        Ok(serde_json::json!({ "query": query, "results": results }).to_string())
    }
}

export!(Component);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_are_percent_encoded_and_appended() {
        assert_eq!(encode("a b&c/é"), "a%20b%26c%2F%C3%A9");
        assert_eq!(
            search_url("https://s.example/search", "x y", 3),
            "https://s.example/search?q=x%20y&count=3"
        );
        assert_eq!(
            search_url("https://s.example/search?format=json", "x", 3),
            "https://s.example/search?format=json&q=x&count=3"
        );
    }

    #[test]
    fn brave_and_searxng_answers_are_both_read_as_text() {
        let brave = serde_json::json!({"web": {"results": [
            {"title": "A &amp; B", "url": "https://a.example/", "description": "<strong>one</strong> two"}
        ]}});
        let hits = hits(&brave);
        assert_eq!(hits[0]["title"], "A & B");
        assert_eq!(hits[0]["snippet"], "one two");
        let searx = serde_json::json!({"results": [
            {"title": "C", "url": "https://c.example/", "content": "three"},
            {"title": "no url"}
        ]});
        let hits = super::hits(&searx);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["snippet"], "three");
    }
}
