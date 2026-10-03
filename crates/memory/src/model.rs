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
        _ => "gpt-5-mini",
    }
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
