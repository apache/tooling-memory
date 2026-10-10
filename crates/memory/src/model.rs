//! One prompt to one small model through the egress, in whichever request shape the
//! destination speaks: Messages, chat completions, or generateContent. What mining and
//! the recall deputy share.

use serde_json::{json, Value};

/// The default model per destination, small and cheap: a librarian's or a deputy's job.
#[must_use]
pub fn default_model(destination: &str) -> &'static str {
    match destination {
        "anthropic" | "llmao" => "claude-haiku-4-5-20251001",
        "gemini" | "google" => "gemini-2.5-flash",
        "xai" => "grok-4-fast",
        "dashscope" => "qwen-flash",
        _ => "gpt-5-mini",
    }
}

/// The model to use on a destination: the one given, else for the gateway the one
/// `LLMAO_MODEL` names, else the smallest-looking model the gateway lists, else the
/// destination's default. A gateway serves whatever its operator put behind it, so a
/// fixed name is a guess that fails with a 400 on a gateway that does not serve it; the
/// Foundation's serves open models only.
pub async fn resolve(egress: &dm_egress::Egress, destination: &str, given: Option<&str>) -> String {
    if let Some(m) = given {
        return m.to_string();
    }
    if destination == "llmao" {
        if let Ok(m) = std::env::var("LLMAO_MODEL") {
            if !m.trim().is_empty() {
                return m.trim().to_string();
            }
        }
        if let Ok(v) = egress.get(destination, "v1/models").await {
            if let Some(m) = smallest(&model_ids(&v)) {
                return m;
            }
        }
    }
    default_model(destination).to_string()
}

/// The model a name says is small, by the markers vendors use, else the first listed.
#[must_use]
pub fn smallest(ids: &[String]) -> Option<String> {
    const SMALL: [&str; 8] = ["haiku", "mini", "flash", "lite", "small", "8b", "7b", "4b"];
    SMALL
        .iter()
        .find_map(|marker| ids.iter().find(|id| id.to_lowercase().contains(marker)))
        .or_else(|| ids.first())
        .cloned()
}

/// Model ids out of a listing in OpenAI's shape (`data[].id`), Google's
/// (`models[].name`, prefixed `models/`) or a plain `models[].id`.
#[must_use]
pub fn model_ids(v: &Value) -> Vec<String> {
    let items = v
        .get("data")
        .or_else(|| v.get("models"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut ids: Vec<String> = items
        .iter()
        .filter_map(|m| {
            m.get("id")
                .or_else(|| m.get("name"))
                .and_then(Value::as_str)
                .map(|s| s.trim_start_matches("models/").to_string())
        })
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

/// The path and body of one user-turn request for a destination's API shape.
#[must_use]
pub fn request(destination: &str, model: &str, prompt: &str) -> (String, Value) {
    match destination {
        "anthropic" => (
            "v1/messages".into(),
            json!({ "model": model, "max_tokens": 4000, "messages": [{ "role": "user", "content": prompt }] }),
        ),
        "gemini" | "google" => (
            format!("v1beta/models/{model}:generateContent"),
            json!({ "contents": [{ "role": "user", "parts": [{ "text": prompt }] }] }),
        ),
        _ => (
            "v1/chat/completions".into(),
            json!({ "model": model, "messages": [{ "role": "user", "content": prompt }] }),
        ),
    }
}

/// As [`request`], with the sampling pinned in the destination's own names: what the
/// drift probes send, so a prompt is asked the same way on every run. Mining, the
/// checker and the deputy keep the destination's defaults through [`request`].
#[must_use]
pub fn request_sampled(
    destination: &str,
    model: &str,
    prompt: &str,
    temperature: f64,
    max_tokens: u32,
) -> (String, Value) {
    let (path, mut body) = request(destination, model, prompt);
    match destination {
        "gemini" | "google" => {
            body["generationConfig"] =
                json!({ "temperature": temperature, "maxOutputTokens": max_tokens });
        }
        // OpenAI's own API counts output in max_completion_tokens; the gateways and the
        // other chat-completions destinations still take max_tokens
        "openai" => {
            body["temperature"] = json!(temperature);
            body["max_completion_tokens"] = json!(max_tokens);
        }
        _ => {
            body["temperature"] = json!(temperature);
            body["max_tokens"] = json!(max_tokens);
        }
    }
    (path, body)
}

/// The model's text out of a response in any of the three shapes.
#[must_use]
pub fn answer_text(destination: &str, v: &Value) -> String {
    let path = match destination {
        "anthropic" => v.pointer("/content/0/text"),
        "gemini" | "google" => v.pointer("/candidates/0/content/parts/0/text"),
        _ => v.pointer("/choices/0/message/content"),
    };
    path.and_then(Value::as_str).unwrap_or_default().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_follow_the_destination_shape() {
        let (p, b) = request("anthropic", "claude-haiku-4-5-20251001", "ask");
        assert_eq!(p, "v1/messages");
        assert_eq!(b["model"], "claude-haiku-4-5-20251001");
        assert_eq!(b["messages"][0]["content"], "ask");
        let (p, b) = request("gemini", "gemini-2.5-flash", "ask");
        assert_eq!(p, "v1beta/models/gemini-2.5-flash:generateContent");
        assert_eq!(b["contents"][0]["parts"][0]["text"], "ask");
        let (p, b) = request("llmao", "m", "ask");
        assert_eq!(p, "v1/chat/completions");
        assert_eq!(b["model"], "m");
        assert_eq!(default_model("anthropic"), "claude-haiku-4-5-20251001");
        assert_eq!(default_model("openai"), "gpt-5-mini");
    }

    #[test]
    fn the_smallest_listed_model_is_the_default_on_a_gateway() {
        let ids = |v: &[&str]| v.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        assert_eq!(
            smallest(&ids(&["gemma4-26b", "qwen3-8b", "qwen3.8-27b"])).as_deref(),
            Some("qwen3-8b")
        );
        assert_eq!(
            smallest(&ids(&["claude-haiku-4-5", "gpt-5-mini"])).as_deref(),
            Some("claude-haiku-4-5")
        );
        assert_eq!(smallest(&ids(&["big-model"])).as_deref(), Some("big-model"));
        assert_eq!(smallest(&[]), None);
        let listing = serde_json::json!({"data": [{"id": "b"}, {"id": "a"}, {"id": "a"}]});
        assert_eq!(model_ids(&listing), ids(&["a", "b"]));
    }

    #[test]
    fn sampled_requests_pin_temperature_and_length_in_each_shape() {
        let (_, b) = request_sampled("anthropic", "m", "ask", 0.0, 64);
        assert_eq!(b["temperature"], 0.0);
        assert_eq!(b["max_tokens"], 64);
        let (_, b) = request_sampled("google", "m", "ask", 0.0, 64);
        assert_eq!(b["generationConfig"]["maxOutputTokens"], 64);
        let (_, b) = request_sampled("openai", "m", "ask", 0.0, 64);
        assert_eq!(b["max_completion_tokens"], 64);
        let (p, b) = request_sampled("llmao", "m", "ask", 0.0, 64);
        assert_eq!(p, "v1/chat/completions");
        assert_eq!(
            (b["temperature"].as_f64(), b["max_tokens"].as_u64()),
            (Some(0.0), Some(64))
        );
        assert_eq!(b["messages"][0]["content"], "ask");
        // the unsampled request is what it was
        assert!(request("llmao", "m", "ask").1.get("temperature").is_none());
    }

    #[test]
    fn answers_are_read_from_each_shape() {
        let a = json!({ "content": [{ "type": "text", "text": "hi" }] });
        assert_eq!(answer_text("anthropic", &a), "hi");
        let g = json!({ "candidates": [{ "content": { "parts": [{ "text": "g" }] } }] });
        assert_eq!(answer_text("google", &g), "g");
        let o = json!({ "choices": [{ "message": { "content": "o" } }] });
        assert_eq!(answer_text("openai", &o), "o");
        assert_eq!(answer_text("openai", &json!({ "error": "x" })), "");
    }
}
