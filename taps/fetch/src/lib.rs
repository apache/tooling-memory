//! The fetch tool tap: one web page, fetched for an agent and handed back as text.
//!
//! An agent calls it through the memory server mid-turn. It sends a GET to the URL,
//! follows redirects itself, each hop a fresh request, reads the body up to a cap, and
//! answers as `dm_fetch` shapes it: HTML made text, plain text, JSON and XML as they
//! are, anything else refused as not text. What is not the request itself lives in
//! that crate, shared with the tool server's native fetch, so the two answer alike.
//!
//! The tap decides nothing about where it may go. The host decides every request
//! before it leaves, the redirects included: the scheme, the method, the host against
//! the tool's grant, the address it resolves to, and the label of what the calling
//! session has read. A refused hop is an error naming the URL. Any credential the grant
//! names is put on the request by the host; the tap never holds it. What the tap
//! returns is scanned and kept by the memory server before the agent sees it.
//!
//! Config, JSON, all optional: `{"max_bytes": 2000000, "max_redirects": 5}`.
//! Args, JSON: `{"url": "https://..."}`.
//! Answer, JSON: `{"url", "final_url", "status", "content_type", "title", "text",
//! "truncated", "redirects"}`, where `redirects` lists the URLs followed to, in order.

wit_bindgen::generate!({
    path: "../../crates/taps/wit",
    world: "tool",
});

use dm_fetch::{Args, Config, Fetched};
use wasi::http::outgoing_handler;
use wasi::http::types::{Fields, Method, OutgoingRequest, Scheme};
use wasi::io::streams::StreamError;

/// A response: status, headers, and the body up to the cap.
struct Answer {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    truncated: bool,
}

impl Answer {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// GET one URL, reading at most `cap` bytes of its body.
fn get(url: &str, cap: usize) -> Result<Answer, String> {
    let (scheme, authority, path) = dm_fetch::split(url)?;
    let scheme = if scheme == "https" {
        Scheme::Https
    } else {
        Scheme::Http
    };
    let headers = Fields::from_list(&[
        ("accept".to_string(), dm_fetch::ACCEPT.as_bytes().to_vec()),
        (
            "user-agent".to_string(),
            dm_fetch::USER_AGENT.as_bytes().to_vec(),
        ),
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
    let headers = response
        .headers()
        .entries()
        .into_iter()
        .map(|(k, v)| {
            (
                k.to_ascii_lowercase(),
                String::from_utf8_lossy(&v).into_owned(),
            )
        })
        .collect();
    let body = response.consume().map_err(|()| "body")?;
    let stream = body.stream().map_err(|()| "stream")?;
    let mut bytes = Vec::new();
    let mut truncated = false;
    loop {
        match stream.blocking_read(64 * 1024) {
            Ok(chunk) => {
                bytes.extend(chunk);
                if bytes.len() > cap {
                    bytes.truncate(cap);
                    truncated = true;
                    break;
                }
            }
            Err(StreamError::Closed) => break,
            Err(StreamError::LastOperationFailed(e)) => {
                return Err(format!("reading {url}: {}", e.to_debug_string()))
            }
        }
    }
    drop(stream);
    Ok(Answer {
        status,
        headers,
        body: bytes,
        truncated,
    })
}

/// GET the URL, following redirects up to the limit. Returns the final answer, the
/// URL it came from, and the URLs followed to.
fn follow(url: &str, config: &Config) -> Result<(Answer, String, Vec<String>), String> {
    let mut at = url.to_string();
    let mut redirects = Vec::new();
    loop {
        let answer = get(&at, config.max_bytes)?;
        if !dm_fetch::is_redirect(answer.status) {
            return Ok((answer, at, redirects));
        }
        let location = answer
            .header("location")
            .ok_or_else(|| format!("HTTP {} from {at} without a Location", answer.status))?;
        if redirects.len() == config.max_redirects {
            return Err(format!(
                "more than {} redirects from {url}",
                config.max_redirects
            ));
        }
        at = dm_fetch::resolve(&at, location);
        redirects.push(at.clone());
    }
}

struct Component;

impl Guest for Component {
    fn call(config: String, args: String) -> Result<String, String> {
        let config = Config::parse(&config)?;
        let args = Args::parse(&args)?;
        let (answer, final_url, redirects) = follow(&args.url, &config)?;
        dm_fetch::answer(&Fetched {
            url: &args.url,
            final_url: &final_url,
            status: answer.status,
            content_type: answer.header("content-type").unwrap_or(""),
            body: &answer.body,
            truncated: answer.truncated,
            redirects: &redirects,
        })
    }
}

export!(Component);
