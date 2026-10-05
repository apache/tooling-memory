//! What a harness says it spent, read from the transcript it left. For a login that
//! travels in a tunnel the egress sees bytes and no tokens, but the harness itself knows:
//! Codex writes a running total into its rollout, Claude Code writes the usage of every
//! model call into its transcript. This is self-reported, and recorded as such, beside
//! what the egress observed.

use dm_egress::Usage;
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};

/// Usage per model, summed over a transcript.
#[derive(Debug, Default, PartialEq)]
pub struct Reported {
    /// Model name to its summed usage.
    pub by_model: BTreeMap<String, Usage>,
    /// For a ChatGPT login: how much of the plan's window Codex says is used, as a
    /// percentage, and the window in minutes.
    pub plan_used_percent: Option<(f64, u64)>,
}

fn u(v: &Value, k: &str) -> u64 {
    v.get(k).and_then(Value::as_u64).unwrap_or(0)
}

/// Read a transcript in any of the shapes the harnesses write.
#[must_use]
pub fn from_transcript(jsonl: &str) -> Reported {
    let mut out = Reported::default();
    let mut seen_claude_messages: HashSet<String> = HashSet::new();
    let mut codex_model = String::from("codex");
    let mut codex_total: Option<Usage> = None;
    for line in jsonl.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let kind = v.get("type").and_then(Value::as_str).unwrap_or_default();
        // Claude Code: one line per content block, the usage repeated per message id
        if kind == "assistant" {
            if let Some(m) = v.get("message") {
                let id = m
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if !id.is_empty() && !seen_claude_messages.insert(id) {
                    continue;
                }
                if let Some(usage) = m.get("usage") {
                    let model = m
                        .get("model")
                        .and_then(Value::as_str)
                        .unwrap_or("claude")
                        .to_string();
                    let e = out.by_model.entry(model).or_default();
                    e.input_tokens += u(usage, "input_tokens");
                    e.output_tokens += u(usage, "output_tokens");
                    e.cache_read_tokens += u(usage, "cache_read_input_tokens");
                    e.cache_write_tokens += u(usage, "cache_creation_input_tokens");
                }
            }
            continue;
        }
        // Codex: the model from the turn context, a running total in token_count events
        if kind == "turn_context" {
            if let Some(m) = v.pointer("/payload/model").and_then(Value::as_str) {
                codex_model = m.to_string();
            }
            continue;
        }
        if kind == "event_msg"
            && v.pointer("/payload/type").and_then(Value::as_str) == Some("token_count")
        {
            if let Some(t) = v.pointer("/payload/info/total_token_usage") {
                let cached = u(t, "cached_input_tokens");
                codex_total = Some(Usage {
                    model: Some(codex_model.clone()),
                    // Codex counts cached input inside input_tokens; keep them apart
                    input_tokens: u(t, "input_tokens").saturating_sub(cached),
                    output_tokens: u(t, "output_tokens"),
                    cache_read_tokens: cached,
                    cache_write_tokens: u(t, "cache_write_input_tokens"),
                });
            }
            if let Some(p) = v.pointer("/payload/rate_limits/primary") {
                if let (Some(pct), Some(win)) = (
                    p.get("used_percent").and_then(Value::as_f64),
                    p.get("window_minutes").and_then(Value::as_u64),
                ) {
                    out.plan_used_percent = Some((pct, win));
                }
            }
        }
    }
    if let Some(t) = codex_total {
        out.by_model.insert(codex_model, t);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_usage_is_summed_once_per_message() {
        let t = concat!(
            r#"{"type":"user","message":{"content":"hi"}}"#,
            "\n",
            r#"{"type":"assistant","message":{"id":"m1","model":"claude-opus-5-5","usage":{"input_tokens":2,"cache_creation_input_tokens":6620,"cache_read_input_tokens":25862,"output_tokens":258},"content":[{"type":"text","text":"a"}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"id":"m1","model":"claude-opus-5-5","usage":{"input_tokens":2,"cache_creation_input_tokens":6620,"cache_read_input_tokens":25862,"output_tokens":258},"content":[{"type":"tool_use","name":"t0"}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"id":"m2","model":"claude-opus-5-5","usage":{"input_tokens":2,"cache_creation_input_tokens":322,"cache_read_input_tokens":32482,"output_tokens":97},"content":[{"type":"text","text":"b"}]}}"#,
            "\n",
        );
        let r = from_transcript(t);
        let u = &r.by_model["claude-opus-5-5"];
        assert_eq!(u.input_tokens, 4);
        assert_eq!(
            u.output_tokens, 355,
            "the second line of m1 is the same call"
        );
        assert_eq!(u.cache_read_tokens, 25862 + 32482);
        assert_eq!(u.cache_write_tokens, 6620 + 322);
        assert!(r.plan_used_percent.is_none());
    }

    #[test]
    fn codex_usage_is_the_last_running_total_with_the_plan_window() {
        let t = concat!(
            r#"{"type":"turn_context","payload":{"model":"gpt-6-luna"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":5000,"cached_input_tokens":4000,"cache_write_input_tokens":0,"output_tokens":20}},"rate_limits":{"primary":{"used_percent":0.5,"window_minutes":43200}}}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":13643,"cached_input_tokens":11008,"cache_write_input_tokens":0,"output_tokens":72}},"rate_limits":{"primary":{"used_percent":1.0,"window_minutes":43200}}}}"#,
            "\n",
        );
        let r = from_transcript(t);
        let u = &r.by_model["gpt-6-luna"];
        assert_eq!(u.input_tokens, 13643 - 11008);
        assert_eq!(u.cache_read_tokens, 11008);
        assert_eq!(u.output_tokens, 72);
        assert_eq!(r.plan_used_percent, Some((1.0, 43200)));
        assert!(from_transcript("not json\n").by_model.is_empty());
    }
}
