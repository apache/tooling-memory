//! Frozen-context replay, the paper's cheapest mode: the exact recorded context at a
//! logged decision point is presented again, to the model that made it or another, and
//! the next action is scored against the one recorded. Pointwise decision quality; a
//! regression between models; the confirmation step for vendor drift.
//!
//! The decision contexts are the egress's: each model call a session made, the request
//! exactly as it left and the response as it came back, which the agent cannot shape.
//! Replay sends the recorded request again to the same destination and path with only
//! the model changed, and compares the tools the candidate asks for with the tools the
//! upstream returned. A session from before contexts were kept falls back to its
//! transcript, the harness's own account, and the run says which it used. Replay is the
//! owner's, and every run is a line in `state/replay/<session>.jsonl`, pinned by a
//! digest of the contexts and the prompt, so two runs compare like for like.

use dm_egress::Egress;
use dm_server::transcripts::Turn;
use dm_server::Memory;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::BTreeSet;

const INSTRUCTION: &str = "You are an AI agent resuming a recorded session at a decision point. The conversation so far is above. Give only your next step. If you would call a tool, answer `TOOL <tool name>` on the first line and one sentence of why on the second. Otherwise give the reply you would write, in full.";

/// What happened at one decision point, recorded and replayed.
#[derive(Clone, Debug, Serialize)]
pub struct Point {
    /// The turn, from one.
    pub turn: usize,
    /// The recorded agent's tool, or none for a reply.
    pub recorded_tool: Option<String>,
    /// The candidate's tool, or none for a reply.
    pub replayed_tool: Option<String>,
    /// Word overlap of the two replies, when both replied.
    pub overlap: Option<f64>,
    /// Whether the candidate did what was recorded: the same tool, or a reply that
    /// overlaps the recorded one by at least a third.
    pub agrees: bool,
    /// The start of what the candidate said.
    pub replayed: String,
}

/// A replay run.
#[derive(Clone, Debug, Serialize)]
pub struct Run {
    /// `contexts`, the egress's record; or `transcript`, the harness's own account.
    pub source: String,
    /// Who observed what was replayed: `egress` for contexts, `harness` for a transcript,
    /// a self-report.
    pub observed_by: String,
    pub at: String,
    pub session: String,
    pub destination: String,
    pub model: String,
    /// What was pinned on our side: the context and the instruction.
    pub context_digest: String,
    pub prompt_digest: String,
    pub points: Vec<Point>,
    pub agreement: f64,
}

/// The tool a recorded turn used, as the transcript names it: `[used tool <name>]`.
#[must_use]
pub fn recorded_tool(text: &str) -> Option<String> {
    let start = text.find("[used tool ")? + "[used tool ".len();
    let end = text[start..].find(']')? + start;
    Some(short(&text[start..end]))
}

/// The tool a candidate named, `TOOL <name>` on its first line.
#[must_use]
pub fn replayed_tool(text: &str) -> Option<String> {
    let first = text.trim().lines().next()?.trim().trim_matches('`');
    first.strip_prefix("TOOL ").map(|t| short(t.trim()))
}

/// A tool name without its server's prefix, so `mcp__memory__get` and `get` match.
fn short(name: &str) -> String {
    name.rsplit("__")
        .next()
        .unwrap_or(name)
        .trim()
        .to_lowercase()
}

/// One minus nothing: the share of words the two texts have in common.
#[must_use]
pub fn overlap(a: &str, b: &str) -> f64 {
    let words = |s: &str| -> BTreeSet<String> {
        s.split(|c: char| !c.is_alphanumeric())
            .filter(|w| w.len() > 2)
            .map(str::to_lowercase)
            .collect()
    };
    let (wa, wb) = (words(a), words(b));
    if wa.is_empty() || wb.is_empty() {
        return 0.0;
    }
    #[allow(clippy::cast_precision_loss)] // counts of words
    let r = wa.intersection(&wb).count() as f64 / wa.union(&wb).count() as f64;
    r
}

/// The request for one decision point: the turns before it as the conversation, then
/// the instruction, in the shape the destination speaks.
#[must_use]
pub fn request(destination: &str, model: &str, context: &[Turn]) -> (String, Value) {
    let mut messages: Vec<Value> = context
        .iter()
        .map(|t| {
            let role = if t.role == "assistant" {
                "assistant"
            } else {
                "user"
            };
            json!({ "role": role, "content": t.text })
        })
        .collect();
    messages.push(json!({ "role": "user", "content": INSTRUCTION }));
    if destination == "anthropic" {
        (
            "v1/messages".into(),
            json!({ "model": model, "max_tokens": 1024, "temperature": 0, "messages": messages }),
        )
    } else {
        (
            "v1/chat/completions".into(),
            json!({ "model": model, "temperature": 0, "messages": messages }),
        )
    }
}

/// Score one decision point.
#[must_use]
pub fn score(turn: usize, recorded: &str, replayed: &str) -> Point {
    let recorded_tool = recorded_tool(recorded);
    let replayed_tool = replayed_tool(replayed);
    let overlap = match (&recorded_tool, &replayed_tool) {
        (None, None) => Some(overlap(recorded, replayed)),
        _ => None,
    };
    let agrees = match (&recorded_tool, &replayed_tool) {
        (Some(a), Some(b)) => a == b,
        (None, None) => overlap.unwrap_or(0.0) >= 1.0 / 3.0,
        _ => false,
    };
    Point {
        turn,
        recorded_tool,
        replayed_tool,
        overlap,
        agrees,
        replayed: replayed.chars().take(160).collect(),
    }
}

/// Replay a session's decision points, the assistant turns, up to `limit` of them, or
/// only `turn`, through `destination` with `model`, and log the run.
///
/// # Errors
///
/// When the transcript cannot be read for replay, the model cannot be reached, or the
/// log cannot be written.
pub async fn replay(
    egress: &Egress,
    memory: &Memory,
    session: &str,
    destination: &str,
    model: &str,
    turn: Option<usize>,
    limit: usize,
) -> Result<Run, Box<dyn std::error::Error>> {
    let turns = memory.replay_turns(session).await?;
    let points: Vec<&Turn> = turns
        .iter()
        .filter(|t| t.role == "assistant" && t.turn > 1 && turn.is_none_or(|n| t.turn == n))
        .take(limit)
        .collect();
    let mut scored = Vec::new();
    for p in points {
        let context: Vec<Turn> = turns.iter().filter(|t| t.turn < p.turn).cloned().collect();
        let (path, body) = request(destination, model, &context);
        let answer = egress.call(destination, &path, &body).await?;
        scored.push(score(
            p.turn,
            &p.text,
            &crate::model::answer_text(destination, &answer),
        ));
    }
    #[allow(clippy::cast_precision_loss)] // counts of points
    let agreement = if scored.is_empty() {
        0.0
    } else {
        scored.iter().filter(|p| p.agrees).count() as f64 / scored.len() as f64
    };
    let run = Run {
        source: "transcript".into(),
        observed_by: dm_server::transcripts::OBSERVED_BY_HARNESS.into(),
        at: time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default(),
        session: session.to_string(),
        destination: destination.to_string(),
        model: model.to_string(),
        context_digest: dm_core::digest(&serde_json::to_string(&turns)?)[..16].to_string(),
        prompt_digest: dm_core::digest(INSTRUCTION)[..16].to_string(),
        points: scored,
        agreement,
    };
    log(memory, &run)?;
    Ok(run)
}

/// Keep a run's line.
fn log(memory: &Memory, run: &Run) -> Result<(), Box<dyn std::error::Error>> {
    let session = &run.session;
    let dir = memory.root().join("state/replay");
    std::fs::create_dir_all(&dir)?;
    let line = serde_json::to_string(&run)? + "\n";
    std::io::Write::write_all(
        &mut std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(format!("{session}.jsonl")))?,
        line.as_bytes(),
    )?;
    Ok(())
}

/// A recorded request made ready to send again: the candidate's model, and the whole
/// answer at once rather than streamed. Nothing else changes: the context is exact.
#[must_use]
pub fn resend(request: &Value, model: &str) -> Value {
    let mut body = request.clone();
    if let Some(o) = body.as_object_mut() {
        o.insert("model".into(), json!(model));
        o.insert("stream".into(), json!(false));
        o.remove("stream_options");
    }
    body
}

/// Replay a session's recorded decision contexts, up to `limit`, or only the call
/// numbered `turn`, on `model` at the destination each was sent to, and log the run.
///
/// # Errors
///
/// When the contexts cannot be read, the model cannot be reached, or the log cannot be
/// written.
pub async fn replay_contexts(
    egress: &Egress,
    memory: &Memory,
    session: &str,
    model: &str,
    turn: Option<usize>,
    limit: usize,
) -> Result<Run, Box<dyn std::error::Error>> {
    let contexts = memory.contexts(session).await?;
    let mut points = Vec::new();
    let mut destination = String::new();
    for c in contexts
        .iter()
        .filter(|c| !c.request.is_null() && turn.is_none_or(|n| c.n == n))
        .take(limit)
    {
        destination.clone_from(&c.destination);
        let answer = egress
            .call(&c.destination, &c.path, &resend(&c.request, model))
            .await?;
        let replayed_tools = dm_server::contexts::tools_in(&answer.to_string());
        // read again from the raw response, so a context kept by an older parser counts
        let recorded_tools = dm_server::contexts::tools_in(&c.response);
        let recorded_tool = recorded_tools.first().map(|t| short(t));
        let replayed_tool = replayed_tools.first().map(|t| short(t));
        let text = crate::model::answer_text(&c.destination, &answer);
        let overlap = (recorded_tool.is_none() && replayed_tool.is_none())
            .then(|| overlap(&dm_server::contexts::text_in(&c.response), &text));
        let agrees = match (&recorded_tool, &replayed_tool) {
            (Some(a), Some(b)) => a == b,
            (None, None) => overlap.unwrap_or(0.0) >= 1.0 / 3.0,
            _ => false,
        };
        let shown = if text.trim().is_empty() {
            replayed_tools.join(", ")
        } else {
            text
        };
        points.push(Point {
            turn: c.n,
            recorded_tool,
            replayed_tool,
            overlap,
            agrees,
            replayed: shown.chars().take(160).collect(),
        });
    }
    #[allow(clippy::cast_precision_loss)] // counts of points
    let agreement = if points.is_empty() {
        0.0
    } else {
        points.iter().filter(|p| p.agrees).count() as f64 / points.len() as f64
    };
    let pinned: Vec<&Value> = contexts.iter().map(|c| &c.request).collect();
    let run = Run {
        source: "contexts".into(),
        observed_by: dm_server::transcripts::OBSERVED_BY_EGRESS.into(),
        at: time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default(),
        session: session.to_string(),
        destination,
        model: model.to_string(),
        context_digest: dm_core::digest(&serde_json::to_string(&pinned)?)[..16].to_string(),
        prompt_digest: "exact".into(),
        points,
        agreement,
    };
    log(memory, &run)?;
    Ok(run)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_are_read_from_both_sides_without_their_server_prefix() {
        assert_eq!(
            recorded_tool("[used tool mcp__memory__get_task]").as_deref(),
            Some("get_task")
        );
        assert_eq!(
            recorded_tool("thinking, then [used tool search] and more").as_deref(),
            Some("search")
        );
        assert_eq!(recorded_tool("a plain reply"), None);
        assert_eq!(
            replayed_tool("TOOL get_task\nto see it").as_deref(),
            Some("get_task")
        );
        assert_eq!(
            replayed_tool("`TOOL mcp__memory__search`").as_deref(),
            Some("search")
        );
        assert_eq!(replayed_tool("I would answer that DR-19 is open."), None);
    }

    #[test]
    fn a_point_agrees_on_the_same_tool_or_an_overlapping_reply() {
        assert!(score(3, "[used tool mcp__memory__get_task]", "TOOL get_task\nwhy").agrees);
        assert!(!score(3, "[used tool mcp__memory__get_task]", "TOOL search\nwhy").agrees);
        assert!(!score(3, "[used tool search]", "DR-19 is open.").agrees);
        let p = score(
            5,
            "DR-19 is still proposed and MoinMoin stays",
            "DR-19 remains proposed; MoinMoin stays for now",
        );
        assert!(p.agrees && p.overlap.unwrap() > 0.3, "{p:?}");
    }

    #[test]
    fn the_context_goes_as_the_conversation_and_the_instruction_last() {
        let ctx = vec![
            Turn {
                turn: 1,
                role: "user".into(),
                text: "Do the task.".into(),
            },
            Turn {
                turn: 2,
                role: "assistant".into(),
                text: "[used tool search]".into(),
            },
        ];
        let (path, body) = request("llmao", "m", &ctx);
        assert_eq!(path, "v1/chat/completions");
        assert_eq!(body["temperature"], 0);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[1]["role"], "assistant");
        assert!(msgs[2]["content"].as_str().unwrap().contains("TOOL"));
    }
}
