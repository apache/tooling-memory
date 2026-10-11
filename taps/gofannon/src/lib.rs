//! The Gofannon tool tap: the agents a Gofannon deployment serves, for an agent to call.
//!
//! Gofannon (apache/tooling-gofannon) runs agents written as Python in its own API
//! process, and serves the deployed ones over REST: `GET /deployments` lists them with
//! their input and output schemas, `POST /rest/<name>/start` starts one detached and
//! answers with its run id, `GET /runs/<id>` reads a run's state and, once it is done,
//! its result, and `POST /runs/<id>/stop` stops it. This tap speaks those four.
//!
//! The tap decides nothing about where it may go: the host decides every request before
//! it leaves, the scheme, the method (the grant must allow POST), the host against the
//! tool's grant, the address, and the calling session's label, since an input carries
//! data out. Gofannon authenticates by its session cookie only; the host puts it on the
//! request from the variable the grant names, in the header the grant names (`Cookie`,
//! the variable holding `gofannon_sid=<value>`), and the tap never holds it. What the tap
//! returns is scanned and kept by the memory server before the agent sees it. Model calls
//! inside Gofannon go through its own gateway credential and are not metered here.
//!
//! Config, JSON: `{"base": "https://gofannon.example.org", "wait_seconds": 30}`; `base`
//! is required, and is where the routes above sit.
//! Args, JSON, one of:
//! - `{"op": "agents"}`: `{"agents": [{"name", "description", "input_schema", "output_schema"}]}`
//! - `{"op": "start", "agent": "<name>", "input": {...}}`: `{"run_id"}`
//! - `{"op": "status", "run_id": "<id>"}`: `{"run_id", "agent", "status", "result", "error", "events"}`
//! - `{"op": "run", "agent": "<name>", "input": {...}}`: start, then the status once the run is
//!   done or `wait_seconds` (at most 50) have passed, whichever is first
//! - `{"op": "stop", "run_id": "<id>"}`: `{"run_id", "stopped": true}`

wit_bindgen::generate!({
    path: "../../crates/taps/wit",
    world: "tool",
});

use serde::Deserialize;
use serde_json::{json, Value};
use wasi::http::outgoing_handler;
use wasi::http::types::{Fields, Method, OutgoingBody, OutgoingRequest, Scheme};
use wasi::io::streams::StreamError;

const USER_AGENT: &str = "durable-memory-gofannon/0.1";
/// The most of an answer read, in bytes.
const MAX_BYTES: usize = 4 << 20;
/// The longest a `run` waits for its run, in seconds, inside the host's wall clock.
const MAX_WAIT: u64 = 50;
/// How often a `run` asks after its run.
const POLL_SECONDS: u64 = 2;
/// How many of a run's trace events come back, the last ones.
const EVENTS: usize = 20;

#[derive(Deserialize)]
struct Config {
    base: String,
    #[serde(default = "thirty")]
    wait_seconds: u64,
}

fn thirty() -> u64 {
    30
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Args {
    Agents,
    Start {
        agent: String,
        #[serde(default)]
        input: Value,
    },
    Status {
        run_id: String,
    },
    Run {
        agent: String,
        #[serde(default)]
        input: Value,
    },
    Stop {
        run_id: String,
    },
}

/// `scheme`, `authority` and `path?query` of an http or https URL.
fn split(url: &str) -> Result<(Scheme, String, String), String> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| format!("{url:?} is not an absolute URL"))?;
    let scheme = match scheme.to_ascii_lowercase().as_str() {
        "https" => Scheme::Https,
        "http" => Scheme::Http,
        other => return Err(format!("{other}: http and https only")),
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (rest[..i].to_string(), rest[i..].to_string()),
        None => (rest.to_string(), "/".to_string()),
    };
    if authority.is_empty() {
        return Err(format!("{url:?} has no host"));
    }
    Ok((scheme, authority, path))
}

/// A name that is one path segment: a deployment's friendly name or a run id.
fn segment(what: &str, s: &str) -> Result<String, String> {
    let ok = !s.is_empty()
        && s.len() <= 128
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && s != "."
        && s != "..";
    if ok {
        Ok(s.to_string())
    } else {
        Err(format!(
            "{what} {s:?} is not a name: letters, digits, '-', '_' and '.'"
        ))
    }
}

/// One request: its status and its body as text, up to [`MAX_BYTES`].
fn send(method: &Method, url: &str, body: Option<&str>) -> Result<(u16, String), String> {
    let (scheme, authority, path) = split(url)?;
    let mut headers = vec![
        ("accept".to_string(), b"application/json".to_vec()),
        ("user-agent".to_string(), USER_AGENT.as_bytes().to_vec()),
    ];
    if body.is_some() {
        headers.push(("content-type".to_string(), b"application/json".to_vec()));
    }
    let headers = Fields::from_list(&headers).map_err(|e| format!("headers: {e}"))?;
    let req = OutgoingRequest::new(headers);
    req.set_method(method).map_err(|()| "method")?;
    req.set_scheme(Some(&scheme)).map_err(|()| "scheme")?;
    req.set_authority(Some(&authority))
        .map_err(|()| "authority")?;
    req.set_path_with_query(Some(&path)).map_err(|()| "path")?;
    let out = req.body().map_err(|()| "body")?;
    if let Some(b) = body {
        let stream = out.write().map_err(|()| "body stream")?;
        stream
            .blocking_write_and_flush(b.as_bytes())
            .map_err(|e| format!("write: {e}"))?;
    }
    OutgoingBody::finish(out, None).map_err(|e| format!("finish: {e}"))?;
    let pending =
        outgoing_handler::handle(req, None).map_err(|e| format!("request to {url}: {e}"))?;
    pending.subscribe().block();
    let response = pending
        .get()
        .ok_or("no response")?
        .map_err(|()| "response taken twice")?
        .map_err(|e| format!("request to {url}: {e}"))?;
    let status = response.status();
    let incoming = response.consume().map_err(|()| "body")?;
    let stream = incoming.stream().map_err(|()| "stream")?;
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

/// A JSON answer from a 2xx, or Gofannon's own word on why not.
fn json_of(status: u16, text: &str, what: &str) -> Result<Value, String> {
    let v: Value = serde_json::from_str(text).unwrap_or(Value::String(text.to_string()));
    if !(200..300).contains(&status) {
        let detail = v.get("detail").cloned().unwrap_or(v);
        return Err(format!("{what}: HTTP {status}: {detail}"));
    }
    Ok(v)
}

/// The deployments, as an agent reads them.
fn agents(listed: &Value) -> Value {
    let rows: Vec<Value> = listed
        .as_array()
        .into_iter()
        .flatten()
        .map(|d| {
            json!({
                "name": d.get("friendlyName").or_else(|| d.get("friendly_name")),
                "description": d.get("description"),
                "input_schema": d.get("inputSchema").or_else(|| d.get("input_schema")),
                "output_schema": d.get("outputSchema").or_else(|| d.get("output_schema")),
            })
        })
        .collect();
    json!({ "agents": rows })
}

/// A run's state, as an agent reads it: the last trace events only.
fn status_of(run_id: &str, full: &Value) -> Value {
    let events: Vec<Value> = full
        .get("events")
        .and_then(Value::as_array)
        .map(|e| e.iter().rev().take(EVENTS).rev().cloned().collect())
        .unwrap_or_default();
    json!({
        "run_id": run_id,
        "agent": full.get("agentName"),
        "status": full.get("status"),
        "result": full.get("result"),
        "error": full.get("error"),
        "events": events,
    })
}

fn running(status: &Value) -> bool {
    status.get("status").and_then(Value::as_str) == Some("running")
}

fn start(base: &str, agent: &str, input: &Value) -> Result<String, String> {
    let agent = segment("agent", agent)?;
    let input = if input.is_null() {
        json!({})
    } else {
        input.clone()
    };
    if !input.is_object() {
        return Err("input is a JSON object, the agent's input".into());
    }
    let (s, t) = send(
        &Method::Post,
        &format!("{base}/rest/{agent}/start"),
        Some(&input.to_string()),
    )?;
    let v = json_of(s, &t, &format!("starting {agent}"))?;
    v.get("runId")
        .or_else(|| v.get("run_id"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("starting {agent}: no run id in {v}"))
}

fn status(base: &str, run_id: &str) -> Result<Value, String> {
    let run_id = segment("run", run_id)?;
    let (s, t) = send(&Method::Get, &format!("{base}/runs/{run_id}"), None)?;
    Ok(status_of(
        &run_id,
        &json_of(s, &t, &format!("run {run_id}"))?,
    ))
}

fn sleep(seconds: u64) {
    wasi::clocks::monotonic_clock::subscribe_duration(seconds * 1_000_000_000).block();
}

struct Component;

impl Guest for Component {
    fn call(config: String, args: String) -> Result<String, String> {
        let config: Config = serde_json::from_str(&config).map_err(|e| format!("config: {e}"))?;
        let base = config.base.trim_end_matches('/');
        split(base)?;
        let args: Args = serde_json::from_str(&args).map_err(|e| format!("args: {e}"))?;
        let answer = match args {
            Args::Agents => {
                let (s, t) = send(&Method::Get, &format!("{base}/deployments"), None)?;
                agents(&json_of(s, &t, "the deployments")?)
            }
            Args::Start { agent, input } => json!({ "run_id": start(base, &agent, &input)? }),
            Args::Status { run_id } => status(base, &run_id)?,
            Args::Run { agent, input } => {
                let run_id = start(base, &agent, &input)?;
                let wait = config.wait_seconds.min(MAX_WAIT);
                let mut waited = 0;
                let mut now = status(base, &run_id)?;
                while running(&now) && waited < wait {
                    sleep(POLL_SECONDS);
                    waited += POLL_SECONDS;
                    now = status(base, &run_id)?;
                }
                now
            }
            Args::Stop { run_id } => {
                let run_id = segment("run", &run_id)?;
                let (s, t) = send(
                    &Method::Post,
                    &format!("{base}/runs/{run_id}/stop"),
                    Some("{}"),
                )?;
                json_of(s, &t, &format!("stopping {run_id}"))?;
                json!({ "run_id": run_id, "stopped": true })
            }
        };
        Ok(answer.to_string())
    }
}

export!(Component);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_one_segment_and_nothing_else() {
        assert!(segment("agent", "asvs_orchestrate").is_ok());
        assert!(segment("run", "run-4f2a.1").is_ok());
        for bad in ["", "..", "a/b", "a?b", "a b", "%2e%2e"] {
            assert!(segment("agent", bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn deployments_and_runs_are_read_in_gofannon_s_camel_case() {
        let listed = json!([{"friendlyName": "triage", "agentId": "a1", "description": "Triage an issue",
            "inputSchema": {"issue": "string"}, "outputSchema": {"label": "string"}}]);
        let a = agents(&listed);
        assert_eq!(a["agents"][0]["name"], "triage");
        assert_eq!(a["agents"][0]["input_schema"]["issue"], "string");
        let events: Vec<Value> = (0..30).map(|i| json!({"n": i})).collect();
        let full = json!({"runId": "r1", "agentName": "triage", "status": "completed",
            "result": {"label": "bug"}, "error": null, "events": events});
        let s = status_of("r1", &full);
        assert_eq!(s["result"]["label"], "bug");
        assert_eq!(s["events"].as_array().unwrap().len(), EVENTS);
        assert_eq!(s["events"][0]["n"], 10, "the last ones");
        assert!(!running(&s));
    }

    #[test]
    fn args_name_their_op() {
        assert!(matches!(
            serde_json::from_str::<Args>(r#"{"op": "run", "agent": "x", "input": {"a": 1}}"#),
            Ok(Args::Run { .. })
        ));
        assert!(serde_json::from_str::<Args>(r#"{"op": "delete"}"#).is_err());
    }
}
