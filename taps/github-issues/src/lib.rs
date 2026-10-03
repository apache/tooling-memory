//! The GitHub issues tap: the open issues of one repository become proposed tasks on the
//! owning domain's ledger, each citing its issue as origin, deduplicated on origin and
//! the issue's last update. Pull requests are not issues and are skipped. The tap holds
//! no credential: the host puts the bearer token on the request to the host the grant
//! names, and the tap sees only the response.
//!
//! Config, JSON: `{"repo": "owner/name", "api": "https://api.github.com"}`; `api` is
//! optional and exists so a test can point the tap at a fake.

wit_bindgen::generate!({
    path: "../../crates/taps/wit",
    world: "tap",
});

use durable_memory::tap::memory::{file_task, Task};
use serde::Deserialize;
use wasi::http::outgoing_handler;
use wasi::http::types::{Fields, Method, OutgoingRequest, Scheme};
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
struct Issue {
    number: u64,
    title: String,
    #[serde(default)]
    body: Option<String>,
    html_url: String,
    state: String,
    updated_at: String,
    #[serde(default)]
    pull_request: Option<serde_json::Value>,
}

/// `scheme`, `authority` and `path` of a URL, enough for an API base.
fn split(url: &str) -> Result<(Scheme, String, String), String> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| format!("api {url:?} has no scheme"))?;
    let scheme = match scheme {
        "https" => Scheme::Https,
        "http" => Scheme::Http,
        other => Scheme::Other(other.to_string()),
    };
    let (authority, path) = match rest.split_once('/') {
        Some((a, p)) => (a.to_string(), format!("/{p}")),
        None => (rest.to_string(), String::new()),
    };
    if authority.is_empty() {
        return Err(format!("api {url:?} has no host"));
    }
    Ok((scheme, authority, path.trim_end_matches('/').to_string()))
}

/// GET a path under the API, the whole body back as text.
fn get(api: &str, path: &str) -> Result<(u16, String), String> {
    let (scheme, authority, base) = split(api)?;
    let headers = Fields::from_list(&[
        (
            "accept".to_string(),
            b"application/vnd.github+json".to_vec(),
        ),
        ("user-agent".to_string(), b"durable-memory-tap/0.1".to_vec()),
    ])
    .map_err(|e| format!("headers: {e}"))?;
    let req = OutgoingRequest::new(headers);
    req.set_method(&Method::Get).map_err(|()| "method")?;
    req.set_scheme(Some(&scheme)).map_err(|()| "scheme")?;
    req.set_authority(Some(&authority))
        .map_err(|()| "authority")?;
    req.set_path_with_query(Some(&format!("{base}{path}")))
        .map_err(|()| "path")?;
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

struct Component;

impl Guest for Component {
    fn run(config: String) -> Result<Report, String> {
        let config: Config = serde_json::from_str(&config).map_err(|e| format!("config: {e}"))?;
        if config.repo.split('/').filter(|s| !s.is_empty()).count() != 2 {
            return Err(format!("repo {:?} is not owner/name", config.repo));
        }
        let (status, text) = get(
            &config.api,
            &format!("/repos/{}/issues?state=open&per_page=100", config.repo),
        )?;
        if status != 200 {
            return Err(format!(
                "{} answered {status}: {}",
                config.api,
                text.chars().take(200).collect::<String>()
            ));
        }
        let issues: Vec<Issue> = serde_json::from_str(&text).map_err(|e| format!("issues: {e}"))?;
        let mut report = Report {
            filed: 0,
            deduplicated: 0,
            proposed: 0,
            note: String::new(),
        };
        let mut skipped = 0u32;
        for issue in issues {
            if issue.pull_request.is_some() {
                skipped += 1;
                continue;
            }
            let body = format!(
                "{}\n\n{}\n",
                issue.html_url,
                issue.body.as_deref().unwrap_or("").trim()
            );
            let filed = file_task(&Task {
                title: format!("#{}: {}", issue.number, issue.title),
                body,
                origin: format!("github:{}#{}", config.repo, issue.number),
                upstream_event: format!("{}:{}", issue.state, issue.updated_at),
                requirements: vec![],
                priority: 0,
            })?;
            if filed.deduplicated {
                report.deduplicated += 1;
            } else {
                report.filed += 1;
            }
        }
        report.note = format!(
            "{}: {} open issues read, {} pull requests skipped",
            config.repo,
            report.filed + report.deduplicated,
            skipped
        );
        Ok(report)
    }
}

export!(Component);
