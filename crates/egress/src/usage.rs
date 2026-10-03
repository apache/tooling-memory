//! Reading token usage out of a model response as it streams past.
//!
//! Every provider reports what a call cost in tokens, in the response body, whether or
//! not the caller pays per token. The sniffer sees the bytes the harness sees, keeps the
//! tail it needs, and when the body ends says what it found. It never changes a byte.
//!
//! Shapes known:
//! - Anthropic Messages, JSON: `usage` at the top level, `model` beside it.
//! - Anthropic Messages, SSE: `message_start` carries `message.usage` and `message.model`;
//!   each `message_delta` carries cumulative `usage.output_tokens`.
//! - OpenAI Responses, JSON: `usage.input_tokens` / `output_tokens`, `model`.
//! - OpenAI Responses, SSE: the `response.completed` event carries `response.usage`.
//! - OpenAI Chat Completions, JSON or the final chunk: `usage.prompt_tokens` /
//!   `completion_tokens`.

use serde_json::Value;

/// The usage a complete JSON response carries, in any of the three providers' shapes, or
/// `None` when it carries none.
#[must_use]
pub fn from_value(v: &serde_json::Value) -> Option<Usage> {
    let mut s = Sniffer::new(None);
    s.take_json(v);
    s.found.then_some(s.usage)
}

/// Token counts for one model call.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    /// The model the provider says answered.
    pub model: Option<String>,
    /// Input tokens, excluding cache reads where the provider separates them.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Input tokens served from a prompt cache.
    pub cache_read_tokens: u64,
    /// Input tokens written to a prompt cache.
    pub cache_write_tokens: u64,
}

impl Usage {
    fn is_empty(&self) -> bool {
        self.input_tokens == 0
            && self.output_tokens == 0
            && self.cache_read_tokens == 0
            && self.cache_write_tokens == 0
    }

    fn absorb(&mut self, v: &Value) {
        let n = |k: &str| v.get(k).and_then(Value::as_u64);
        if let Some(x) = n("input_tokens").or_else(|| n("prompt_tokens")) {
            self.input_tokens = x;
        }
        if let Some(x) = n("output_tokens").or_else(|| n("completion_tokens")) {
            self.output_tokens = x;
        }
        if let Some(x) = n("cache_read_input_tokens") {
            self.cache_read_tokens = x;
        }
        if let Some(x) = n("cache_creation_input_tokens") {
            self.cache_write_tokens = x;
        }
        if let Some(x) = v
            .pointer("/input_tokens_details/cached_tokens")
            .or_else(|| v.pointer("/prompt_tokens_details/cached_tokens"))
            .and_then(Value::as_u64)
        {
            self.cache_read_tokens = x;
        }
        // Gemini's usageMetadata
        if let Some(x) = n("promptTokenCount") {
            self.input_tokens = x;
        }
        if let Some(x) = n("candidatesTokenCount") {
            self.output_tokens = x;
        }
        if let Some(x) = n("cachedContentTokenCount") {
            self.cache_read_tokens = x;
        }
    }
}

/// Accumulates a response body and extracts [`Usage`] at the end.
pub struct Sniffer {
    sse: bool,
    buf: Vec<u8>,
    usage: Usage,
    found: bool,
    cap: usize,
}

/// How much of a non-streaming body the sniffer keeps. Usage sits at the top level of a
/// JSON object and a response is rarely larger than this; past it, the sniffer gives up
/// rather than hold the whole body.
const JSON_CAP: usize = 8 * 1024 * 1024;

impl Sniffer {
    /// A sniffer for a body with the given content type.
    #[must_use]
    pub fn new(content_type: Option<&str>) -> Sniffer {
        let sse = content_type.is_some_and(|c| c.contains("text/event-stream"));
        Sniffer {
            sse,
            buf: Vec::new(),
            usage: Usage::default(),
            found: false,
            cap: JSON_CAP,
        }
    }

    /// Feed the next chunk of the body.
    pub fn feed(&mut self, chunk: &[u8]) {
        if self.sse {
            self.buf.extend_from_slice(chunk);
            self.drain_sse_events();
        } else if self.buf.len() < self.cap {
            self.buf.extend_from_slice(chunk);
        }
    }

    /// The body has ended. What was found, if anything.
    #[must_use]
    pub fn finish(mut self) -> Option<Usage> {
        if self.sse {
            if !self.buf.is_empty() {
                let rest = std::mem::take(&mut self.buf);
                self.take_event(&rest);
            }
        } else if let Ok(v) = serde_json::from_slice::<Value>(&self.buf) {
            self.take_json(&v);
        }
        (self.found && !self.usage.is_empty()).then_some(self.usage)
    }

    fn drain_sse_events(&mut self) {
        loop {
            let Some(end) = find_event_end(&self.buf) else {
                return;
            };
            let event: Vec<u8> = self.buf.drain(..end.0).collect();
            self.buf.drain(..end.1);
            self.take_event(&event);
        }
    }

    fn take_event(&mut self, event: &[u8]) {
        let Ok(text) = std::str::from_utf8(event) else {
            return;
        };
        let data: String = text
            .lines()
            .filter_map(|l| l.strip_prefix("data:"))
            .map(str::trim_start)
            .collect::<Vec<_>>()
            .join("\n");
        if data.is_empty() || data == "[DONE]" {
            return;
        }
        if let Ok(v) = serde_json::from_str::<Value>(&data) {
            self.take_json(&v);
        }
    }

    fn take_json(&mut self, v: &Value) {
        // Anthropic message_start wraps the message; Responses wraps in `response`.
        let inner = v.get("message").or_else(|| v.get("response")).unwrap_or(v);
        if let Some(m) = inner
            .get("model")
            .or_else(|| inner.get("modelVersion"))
            .and_then(Value::as_str)
        {
            self.usage.model = Some(m.to_string());
        }
        let usage = inner
            .get("usage")
            .or_else(|| v.get("usage"))
            .or_else(|| inner.get("usageMetadata"))
            .filter(|u| u.is_object());
        if let Some(u) = usage {
            self.usage.absorb(u);
            self.found = true;
        }
    }
}

/// Where the first complete SSE event ends: (length of the event, length of the blank
/// line terminator after it).
fn find_event_end(buf: &[u8]) -> Option<(usize, usize)> {
    let mut i = 0;
    while i + 1 < buf.len() {
        if buf[i] == b'\n' && buf[i + 1] == b'\n' {
            return Some((i + 1, 1));
        }
        if i + 3 < buf.len() && &buf[i..i + 4] == b"\r\n\r\n" {
            return Some((i + 2, 2));
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(ct: &str, chunks: &[&str]) -> Option<Usage> {
        let mut s = Sniffer::new(Some(ct));
        for c in chunks {
            s.feed(c.as_bytes());
        }
        s.finish()
    }

    #[test]
    fn anthropic_json() {
        let u = run(
            "application/json",
            &[r#"{"id":"msg_1","model":"claude-opus-5-5","content":[],"usage":{"input_tokens":12,"output_tokens":34,"cache_read_input_tokens":5,"cache_creation_input_tokens":0}}"#],
        )
        .unwrap();
        assert_eq!(u.model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(
            (u.input_tokens, u.output_tokens, u.cache_read_tokens),
            (12, 34, 5)
        );
    }

    #[test]
    fn anthropic_sse_takes_the_final_output_count_and_survives_split_chunks() {
        let events = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-sonnet-5-5\",\"usage\":{\"input_tokens\":100,\"output_tokens\":1,\"cache_read_input_tokens\":40}}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"hi\"}}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":57}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
        );
        let mid = events.len() / 2;
        let u = run(
            "text/event-stream; charset=utf-8",
            &[&events[..mid], &events[mid..]],
        )
        .unwrap();
        assert_eq!(u.model.as_deref(), Some("claude-sonnet-5-5"));
        assert_eq!(u.input_tokens, 100);
        assert_eq!(u.output_tokens, 57, "the delta's cumulative count wins");
        assert_eq!(u.cache_read_tokens, 40);
    }

    #[test]
    fn openai_responses_sse_and_chat_json() {
        let sse = "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"model\":\"gpt-5\"}}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"model\":\"gpt-5\",\"usage\":{\"input_tokens\":20,\"output_tokens\":9,\"input_tokens_details\":{\"cached_tokens\":8}}}}\n\n";
        let u = run("text/event-stream", &[sse]).unwrap();
        assert_eq!(
            (u.input_tokens, u.output_tokens, u.cache_read_tokens),
            (20, 9, 8)
        );
        assert_eq!(u.model.as_deref(), Some("gpt-5"));
        let chat = run(
            "application/json",
            &[r#"{"model":"gpt-5-mini","choices":[],"usage":{"prompt_tokens":7,"completion_tokens":3}}"#],
        )
        .unwrap();
        assert_eq!((chat.input_tokens, chat.output_tokens), (7, 3));
    }

    #[test]
    fn gemini_json_and_sse() {
        let u = run(
            "application/json",
            &[r#"{"candidates":[],"usageMetadata":{"promptTokenCount":15,"candidatesTokenCount":6,"cachedContentTokenCount":3,"totalTokenCount":21},"modelVersion":"gemini-2.5-pro"}"#],
        )
        .unwrap();
        assert_eq!(u.model.as_deref(), Some("gemini-2.5-pro"));
        assert_eq!(
            (u.input_tokens, u.output_tokens, u.cache_read_tokens),
            (15, 6, 3)
        );
        let sse = "data: {\"candidates\":[],\"usageMetadata\":{\"promptTokenCount\":15,\"candidatesTokenCount\":2},\"modelVersion\":\"gemini-2.5-flash\"}\r\n\r\ndata: {\"candidates\":[],\"usageMetadata\":{\"promptTokenCount\":15,\"candidatesTokenCount\":9},\"modelVersion\":\"gemini-2.5-flash\"}\r\n\r\n";
        let u = run("text/event-stream", &[sse]).unwrap();
        assert_eq!(
            (u.input_tokens, u.output_tokens),
            (15, 9),
            "the last event's cumulative count"
        );
    }

    #[test]
    fn nothing_found_is_none() {
        assert!(run(
            "application/json",
            &[r#"{"error":{"type":"authentication_error"}}"#]
        )
        .is_none());
        assert!(run("text/event-stream", &["data: [DONE]\n\n"]).is_none());
        assert!(run("application/json", &["not json"]).is_none());
    }
}
