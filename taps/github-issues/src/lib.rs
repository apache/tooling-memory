//! The GitHub issues tap: the open issues of one repository become proposed tasks on the
//! owning domain's ledger, each citing its issue as origin, deduplicated on origin and
//! the issue's last update. Pull requests are not issues and are skipped; an item the
//! tap cannot read costs only itself. Every page the API links to is read. The tap holds
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

/// Pages read at most; GitHub serves a hundred issues a page.
const MAX_PAGES: usize = 50;

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

/// A response: status, headers and the whole body as text.
struct Answer {
    status: u16,
    headers: Vec<(String, String)>,
    text: String,
}

/// GET one URL.
fn get(url: &str) -> Result<Answer, String> {
    let (scheme, authority, path) = split(url)?;
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
    req.set_path_with_query(Some(&path)).map_err(|()| "path")?;
    let pending = outgoing_handler::handle(req, None).map_err(|e| format!("request: {e}"))?;
    pending.subscribe().block();
    let response = pending
        .get()
        .ok_or("no response")?
        .map_err(|()| "response taken twice")?
        .map_err(|e| format!("response: {e}"))?;
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
    Ok(Answer {
        status,
        headers,
        text: String::from_utf8_lossy(&bytes).into_owned(),
    })
}

/// The `rel="next"` URL of a `Link` header, if there is one.
fn next_page(headers: &[(String, String)]) -> Option<String> {
    let link = headers
        .iter()
        .find(|(k, _)| k == "link")
        .map(|(_, v)| v.as_str())?;
    link.split(',').find_map(|part| {
        let (url, params) = part.trim().split_once(';')?;
        params.contains("rel=\"next\"").then(|| {
            url.trim()
                .trim_start_matches('<')
                .trim_end_matches('>')
                .to_string()
        })
    })
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
        let mut url = Some(format!(
            "{}/repos/{}/issues?state=open&per_page=100",
            config.api.trim_end_matches('/'),
            config.repo
        ));
        let mut report = Report {
            filed: 0,
            deduplicated: 0,
            proposed: 0,
            posted: 0,
            note: String::new(),
        };
        let (mut pages, mut skipped, mut unreadable) = (0usize, 0u32, 0u32);
        while let Some(page) = url.take() {
            if pages == MAX_PAGES {
                break;
            }
            let answer = get(&page)?;
            pages += 1;
            if answer.status != 200 {
                return Err(format!(
                    "{page} answered {}: {}",
                    answer.status,
                    answer.text.chars().take(200).collect::<String>()
                ));
            }
            let items: Vec<serde_json::Value> =
                serde_json::from_str(&answer.text).map_err(|e| format!("issues: {e}"))?;
            for item in items {
                let Ok(issue) = serde_json::from_value::<Issue>(item) else {
                    unreadable += 1;
                    continue;
                };
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
            url = next_page(&answer.headers);
        }
        report.note = format!(
            "{}: {} open issues read over {pages} pages, {skipped} pull requests skipped, {unreadable} items unreadable",
            config.repo,
            report.filed + report.deduplicated
        );
        Ok(report)
    }
}

export!(Component);
