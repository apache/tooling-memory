//! Fetch in the owner's browser (PLAN M19, third cut): a tool whose grant says
//! `browser = true` is fetched by the companion extension beside the web launcher's
//! page, with the browser's own cookies, so a page the owner is signed in to answers
//! the session as it would answer them, and no credential is copied into a grant.
//!
//! The browser is the third host of fetch, beside the component and the tool server,
//! and the guarantees stay where they are: the policy decides the URL at the session's
//! label before the browser is asked, as the tap host decides a request before it
//! leaves; the ask carries the hosts the session may reach, which the extension does
//! not go outside; the URL that answered is decided again before anything is kept;
//! and the answer is shaped by [`dm_fetch::answer`], the same as the other two hosts',
//! then scanned and kept as a source observed by `browser`. What the browser does not
//! give is the path between: the hops of a redirect are the browser's, so the answer
//! names where it started and where it ended, and the call goes to the web launcher on
//! loopback, not through the egress, so it is not metered there.
//!
//! The way in is the running web launcher over this root, found as `amem` finds it,
//! by `state/web.json` and the nonce it must answer under the token; the ask is a
//! `POST` to [`dm_web::companion::FETCH_PATH`] with the token.

use dm_server::sources::Answered;
use dm_server::Memory;
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// How long the whole exchange with the web launcher may take: the companion's own
/// minute, and the round trips around it.
const WAIT: Duration = dm_web::companion::WAIT.saturating_add(Duration::from_secs(10));

/// The web launcher running over `root`, proven to hold the token: its address, and
/// the token to ask with.
async fn launcher(root: &Path) -> Result<(SocketAddr, String), String> {
    let state = dm_web::state::load(root)
        .map_err(|e| format!("the web launcher's state: {e}"))?
        .state;
    let Some(url) = state.url.as_deref() else {
        return Err(
            "no web launcher is running over this root: the browser fetches only while `amem` runs with its companion connected"
                .into(),
        );
    };
    let running = crate::open::running(url, &state.token, root)
        .await
        .ok_or_else(|| {
            format!("the web launcher at {url} is not answering; run `amem` to start it")
        })?;
    Ok((running.addr(), state.token))
}

/// One `POST` of JSON to the web launcher on loopback, with the token: the status and
/// the body.
async fn post(
    addr: SocketAddr,
    path: &str,
    token: &str,
    body: &str,
) -> Result<(u16, String), String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let go = async {
        let mut s = tokio::net::TcpStream::connect(addr)
            .await
            .map_err(|e| format!("connecting to the web launcher at {addr}: {e}"))?;
        let req = format!(
            "POST {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        s.write_all(req.as_bytes())
            .await
            .map_err(|e| format!("asking the web launcher: {e}"))?;
        let mut out = Vec::new();
        s.read_to_end(&mut out)
            .await
            .map_err(|e| format!("reading the web launcher's answer: {e}"))?;
        let text = String::from_utf8_lossy(&out).into_owned();
        let (head, body) = text
            .split_once("\r\n\r\n")
            .ok_or_else(|| "the web launcher answered no HTTP".to_string())?;
        let status: u16 = head
            .split(' ')
            .nth(1)
            .and_then(|c| c.parse().ok())
            .ok_or_else(|| "the web launcher answered no status".to_string())?;
        Ok::<_, String>((status, body.to_string()))
    };
    tokio::time::timeout(WAIT, go)
        .await
        .map_err(|_| "the web launcher did not answer in time".to_string())?
}

/// What the browser said, as [`call`] reads it.
#[derive(Debug, PartialEq, Eq)]
struct Got {
    status: u16,
    content_type: String,
    final_url: String,
    body: String,
    truncated: bool,
}

/// The browser's answer out of the web launcher's JSON: what it fetched, or why not.
fn got(url: &str, v: &Value) -> Result<Got, String> {
    if let Some(e) = v["error"].as_str() {
        return Err(format!("the browser did not fetch {url}: {e}"));
    }
    let status = v["status"]
        .as_u64()
        .and_then(|s| u16::try_from(s).ok())
        .ok_or_else(|| format!("the browser's answer for {url} carries no status"))?;
    Ok(Got {
        status,
        content_type: v["content_type"].as_str().unwrap_or_default().to_string(),
        final_url: v["final_url"]
            .as_str()
            .filter(|f| !f.is_empty())
            .unwrap_or(url)
            .to_string(),
        body: v["body"].as_str().unwrap_or_default().to_string(),
        truncated: v["truncated"].as_bool().unwrap_or(false),
    })
}

/// The answer and the URLs reached, once the URL that answered has passed the policy:
/// where it started, and where it ended when that differs.
fn answered(memory: &Memory, tool: &str, url: &str, g: &Got) -> Result<Answered, String> {
    let mut reached = vec![url.to_string()];
    let mut redirects = Vec::new();
    if g.final_url != url {
        memory.check_reached(tool, &g.final_url).map_err(|why| {
            format!(
                "{url} answered from {}, which this session may not reach: {why}",
                g.final_url
            )
        })?;
        reached.push(g.final_url.clone());
        redirects.push(g.final_url.clone());
    }
    let answer = dm_fetch::answer(&dm_fetch::Fetched {
        url,
        final_url: &g.final_url,
        status: g.status,
        content_type: &g.content_type,
        body: g.body.as_bytes(),
        truncated: g.truncated,
        redirects: &redirects,
    })?;
    Ok(Answered { answer, reached })
}

/// Fetch `args` in the owner's browser for the session `memory` is.
///
/// # Errors
///
/// Arguments or configuration that are not the tool's; the policy refusing the URL, or
/// the one that answered; no web launcher running, or no companion connected to it; or
/// the browser's own failure to fetch.
pub async fn call(
    memory: Arc<Memory>,
    tool: &str,
    config: &str,
    args: &str,
) -> Result<Answered, String> {
    let config = dm_fetch::Config::parse(config)?;
    let args = dm_fetch::Args::parse(args)?;
    // decided here before the browser is asked, as a request is before it leaves
    memory
        .check_reached(tool, &args.url)
        .map_err(|why| format!("request to {}: refused: {why}", args.url))?;
    let hosts = memory.fetch_hosts(tool);
    let (addr, token) = launcher(memory.root()).await?;
    let ask = json!({ "url": args.url, "hosts": hosts, "max_bytes": config.max_bytes });
    let (status, body) = post(
        addr,
        dm_web::companion::FETCH_PATH,
        &token,
        &ask.to_string(),
    )
    .await?;
    if status != 200 {
        return Err(format!(
            "the web launcher answered {status}: {}",
            body.trim()
        ));
    }
    let v: Value = serde_json::from_str(&body)
        .map_err(|e| format!("the web launcher answered no JSON: {e}"))?;
    answered(&memory, tool, &args.url, &got(&args.url, &v)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_browser_s_answer_is_read_or_its_refusal_is() {
        let v = json!({"status": 200, "content_type": "text/html", "final_url": "https://a.example/b", "body": "<p>x</p>", "truncated": true});
        assert_eq!(
            got("https://a.example/a", &v).unwrap(),
            Got {
                status: 200,
                content_type: "text/html".into(),
                final_url: "https://a.example/b".into(),
                body: "<p>x</p>".into(),
                truncated: true,
            }
        );
        let bare = got("https://a.example/a", &json!({"status": 204})).unwrap();
        assert_eq!(bare.final_url, "https://a.example/a");
        assert_eq!(bare.body, "");
        assert!(got("u", &json!({"error": "no"}))
            .unwrap_err()
            .contains("did not fetch u: no"));
        assert!(got("u", &json!({"body": "x"}))
            .unwrap_err()
            .contains("no status"));
    }
}
