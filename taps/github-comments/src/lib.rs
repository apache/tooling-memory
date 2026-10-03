//! The GitHub comments tap, the outbound half: a completed task that came from an issue
//! of one repository has its handoff posted back on that issue as a comment, once. The
//! host hands the tap only the handoffs it may read, under its ceiling, and records where
//! each was posted so a replay posts nothing twice. A post that fails costs only itself.
//! The tap holds no credential: the host puts the bearer token on the request.
//!
//! Config, JSON: `{"repo": "owner/name", "api": "https://api.github.com"}`; `api` is
//! optional and exists so a test can point the tap at a fake.

wit_bindgen::generate!({
    path: "../../crates/taps/wit",
    world: "tap",
});

use durable_memory::tap::memory::{completed, posted, Done};
use serde::Deserialize;
use wasi::http::outgoing_handler;
use wasi::http::types::{Fields, Method, OutgoingBody, OutgoingRequest, Scheme};
use wasi::io::streams::StreamError;

#[derive(Deserialize)]
struct Config {
    repo: String,
    #[serde(default = "github")]
    api: String,
}

fn github() -> String {
    "https://api.github.com".into()
}

#[derive(Deserialize)]
struct Comment {
    html_url: String,
}

/// `scheme`, `authority` and `path?query` of a URL.
fn split(url: &str) -> Result<(Scheme, String, String), String> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| format!("{url:?} has no scheme"))?;
    let scheme = match scheme {
        "https" => Scheme::Https,
        "http" => Scheme::Http,
        other => Scheme::Other(other.to_string()),
    };
    let (authority, path) = match rest.split_once('/') {
        Some((a, p)) => (a.to_string(), format!("/{p}")),
        None => (rest.to_string(), "/".to_string()),
    };
    if authority.is_empty() {
        return Err(format!("{url:?} has no host"));
    }
    Ok((scheme, authority, path))
}

/// POST a JSON body to one URL; the status and the response text.
fn post(url: &str, json: &str) -> Result<(u16, String), String> {
    let (scheme, authority, path) = split(url)?;
    let headers = Fields::from_list(&[
        (
            "accept".to_string(),
            b"application/vnd.github+json".to_vec(),
        ),
        ("content-type".to_string(), b"application/json".to_vec()),
        ("user-agent".to_string(), b"durable-memory-tap/0.1".to_vec()),
    ])
    .map_err(|e| format!("headers: {e}"))?;
    let req = OutgoingRequest::new(headers);
    req.set_method(&Method::Post).map_err(|()| "method")?;
    req.set_scheme(Some(&scheme)).map_err(|()| "scheme")?;
    req.set_authority(Some(&authority))
        .map_err(|()| "authority")?;
    req.set_path_with_query(Some(&path)).map_err(|()| "path")?;
    let body = req.body().map_err(|()| "body")?;
    {
        let out = body.write().map_err(|()| "body stream")?;
        out.blocking_write_and_flush(json.as_bytes())
            .map_err(|e| format!("write: {e}"))?;
    }
    OutgoingBody::finish(body, None).map_err(|e| format!("finish: {e}"))?;
    let pending = outgoing_handler::handle(req, None).map_err(|e| format!("request: {e}"))?;
    pending.subscribe().block();
    let response = pending
        .get()
        .ok_or("no response")?
        .map_err(|()| "response taken twice")?
        .map_err(|e| format!("response: {e}"))?;
    let status = response.status();
    let body = response.consume().map_err(|()| "body")?;
    let stream = body.stream().map_err(|()| "stream")?;
    let mut bytes = Vec::new();
    loop {
        match stream.blocking_read(64 * 1024) {
            Ok(chunk) => bytes.extend(chunk),
            Err(StreamError::Closed) => break,
            Err(StreamError::LastOperationFailed(e)) => {
                return Err(format!("read: {}", e.to_debug_string()))
            }
        }
    }
    drop(stream);
    Ok((status, String::from_utf8_lossy(&bytes).into_owned()))
}

/// The issue number of an origin in this repository, `github:<repo>#<n>`.
fn issue_number(origin: &str, repo: &str) -> Option<u64> {
    origin
        .strip_prefix("github:")?
        .strip_prefix(repo)?
        .strip_prefix('#')?
        .parse()
        .ok()
}

/// The comment: the handoff as written, and a line saying where it came from.
fn comment(done: &Done) -> String {
    format!(
        "{}\n\n---\nPosted by the memory tap `github-comments`: the handoff `{}` of task `{}`.\n",
        done.text.trim_end(),
        done.handoff,
        done.id
    )
}

struct Component;

impl Guest for Component {
    fn run(config: String) -> Result<Report, String> {
        let config: Config = serde_json::from_str(&config).map_err(|e| format!("config: {e}"))?;
        if config.repo.split('/').filter(|s| !s.is_empty()).count() != 2
            || config.repo.contains("..")
        {
            return Err(format!("repo {:?} is not owner/name", config.repo));
        }
        let api = config.api.trim_end_matches('/');
        let mut report = Report {
            filed: 0,
            deduplicated: 0,
            proposed: 0,
            posted: 0,
            note: String::new(),
        };
        let (mut elsewhere, mut failed) = (0u32, Vec::new());
        for done in completed()? {
            let Some(number) = issue_number(&done.origin, &config.repo) else {
                elsewhere += 1;
                continue;
            };
            let url = format!("{api}/repos/{}/issues/{number}/comments", config.repo);
            let body = serde_json::json!({ "body": comment(&done) }).to_string();
            match post(&url, &body) {
                Ok((201, text)) => {
                    let to = serde_json::from_str::<Comment>(&text)
                        .map(|c| c.html_url)
                        .unwrap_or(url);
                    posted(&done.id, &to)?;
                    report.posted += 1;
                }
                Ok((status, text)) => failed.push(format!(
                    "#{number}: {status} {}",
                    text.chars().take(120).collect::<String>()
                )),
                Err(e) => failed.push(format!("#{number}: {e}")),
            }
        }
        report.note = format!(
            "{}: {} handoffs posted, {elsewhere} done elsewhere{}",
            config.repo,
            report.posted,
            if failed.is_empty() {
                String::new()
            } else {
                format!(", {} failed: {}", failed.len(), failed.join("; "))
            }
        );
        Ok(report)
    }
}

export!(Component);
