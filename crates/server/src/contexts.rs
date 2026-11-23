//! Decision contexts: what each model call of a session was sent, and what came back,
//! as the egress saw it. The paper's replay uses recorded decision contexts, not
//! transcripts, and this is why: a transcript is the harness's own account, which an
//! agent with a shell could shape and a harness may leave incomplete, while the egress
//! is in the path of every metered call and records what actually left and returned.
//! A context is kept beside the session's domain's entries, at the session's label when
//! the call was made, never indexed or searched, readable by the owner alone, and swept
//! on the transcripts' clock. It holds every entry body the session had read, so erasing
//! an entry erases the contexts that carry it.
//!
//! Each context carries a replay manifest ([`crate::replay_state`]): what the session
//! could see when the call was made, by reference, and the entries it was served since
//! the context before.

use crate::replay_state::ReplayManifest;
use crate::{Error, Memory};
use dm_core::Label;
use dm_store::Area;
use serde::{Deserialize, Serialize};
use std::sync::atomic::Ordering;
use time::{Duration, OffsetDateTime};

/// A response kept up to this much; a request body over this is kept as a stub.
pub const MAX_BYTES: usize = 16 << 20;

/// One model call, as the egress saw it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Context {
    /// Its place in the session, from one.
    pub n: usize,
    /// When.
    pub at: String,
    /// The destination the call went to.
    pub destination: String,
    /// The API path at that destination.
    pub path: String,
    /// The session's label when the call was made.
    pub label: Label,
    /// The request body, exactly as it left; `null` when it was too large to keep.
    pub request: serde_json::Value,
    /// The response as it came back, raw, up to [`MAX_BYTES`].
    pub response: String,
    /// The tools the model asked for in it, by name.
    pub tools: Vec<String>,
    /// Who observed it: the egress, which every model call passes through.
    #[serde(default = "by_egress")]
    pub observed_by: String,
    /// What the session could see at this call, and the entries it was served since
    /// the context before; none on a context kept before manifests were, or when the
    /// state could not be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest: Option<ReplayManifest>,
}

fn by_egress() -> String {
    crate::transcripts::OBSERVED_BY_EGRESS.to_string()
}

/// The tools a model asked for in a response, in the shapes the providers speak:
/// Anthropic's `tool_use` blocks, OpenAI's chat `function` calls, the Responses API's
/// `function_call` items, Gemini's `functionCall` parts; a whole JSON answer or a
/// stream of `data:` events, each read as JSON and walked, so the order of fields in an
/// object does not matter.
#[must_use]
pub fn tools_in(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) {
        walk(&v, &mut out);
    } else {
        for line in raw.lines() {
            let line = line.trim_start_matches("data:").trim();
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                walk(&v, &mut out);
            }
        }
    }
    out
}

/// The text a model answered with, from a whole answer or a stream of events, in the
/// shapes the providers speak: the Responses API's `output_text` (and its deltas),
/// chat completions' `content` (and its deltas), Anthropic's text blocks and
/// `text_delta`s. Tool-call arguments are not text.
#[must_use]
pub fn text_in(raw: &str) -> String {
    let mut out = String::new();
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) {
        text_of(&v, &mut out);
    } else {
        for line in raw.lines() {
            let line = line.trim_start_matches("data:").trim();
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                text_of(&v, &mut out);
            }
        }
    }
    out
}

fn text_of(v: &serde_json::Value, out: &mut String) {
    use serde_json::Value;
    let s = |p: &str| v.pointer(p).and_then(Value::as_str);
    match v.get("type").and_then(Value::as_str) {
        // the Responses API, streamed
        Some("response.output_text.delta") => out.push_str(s("/delta").unwrap_or_default()),
        // Anthropic, streamed
        Some("content_block_delta") if s("/delta/type") == Some("text_delta") => {
            out.push_str(s("/delta/text").unwrap_or_default());
        }
        _ => {
            // chat completions, streamed or whole
            if let Some(choices) = v.get("choices").and_then(Value::as_array) {
                for c in choices {
                    let t = c
                        .pointer("/delta/content")
                        .or_else(|| c.pointer("/message/content"));
                    out.push_str(t.and_then(Value::as_str).unwrap_or_default());
                }
            }
            // whole answers: Responses `output`, Messages `content`
            for key in ["output", "content"] {
                for item in v.get(key).and_then(Value::as_array).into_iter().flatten() {
                    match item.get("type").and_then(Value::as_str) {
                        Some("text" | "output_text") => {
                            out.push_str(
                                item.get("text").and_then(Value::as_str).unwrap_or_default(),
                            );
                        }
                        Some("message") => text_of(item, out),
                        _ => {}
                    }
                }
            }
        }
    }
}

fn push(out: &mut Vec<String>, name: String) {
    if !out.contains(&name) {
        out.push(name);
    }
}

fn walk(v: &serde_json::Value, out: &mut Vec<String>) {
    use serde_json::Value;
    match v {
        Value::Object(o) => {
            let name = o.get("name").and_then(Value::as_str);
            if let (Some("tool_use" | "function_call"), Some(n)) =
                (o.get("type").and_then(Value::as_str), name)
            {
                push(
                    out,
                    match o.get("namespace").and_then(Value::as_str) {
                        Some(ns) => format!("{ns}__{n}"),
                        None => n.to_string(),
                    },
                );
            }
            for (k, inner) in o {
                // a chat completion's call, and a Gemini part's
                if k == "function" || k == "functionCall" {
                    if let Some(n) = inner.get("name").and_then(Value::as_str) {
                        push(out, n.to_string());
                    }
                }
                walk(inner, out);
            }
        }
        Value::Array(a) => {
            for x in a {
                walk(x, out);
            }
        }
        _ => {}
    }
}

fn name_of(n: usize) -> String {
    format!("{n:06}.json")
}

impl Memory {
    /// Keep one model call's context, under this session in its domain: the domain of
    /// the task it holds, else the only granted one; a session with neither keeps none,
    /// and nor does a re-run. The context carries the session's replay manifest, with
    /// the entries served since the context before; a context is kept without one when
    /// the state cannot be read, since the model's input is the evidence that has no
    /// other copy.
    ///
    /// # Errors
    ///
    /// A store error.
    pub async fn keep_context(
        &self,
        destination: &str,
        path: &str,
        request: &[u8],
        response: &[u8],
    ) -> Result<(), Error> {
        if !self.keeps_contexts() {
            return Ok(());
        }
        let Some(domain) = self.recording_domain().await else {
            return Ok(());
        };
        let n = self.contexts_kept.fetch_add(1, Ordering::Relaxed) + 1;
        let request = if request.len() > MAX_BYTES {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(request).unwrap_or(serde_json::Value::Null)
        };
        let response =
            String::from_utf8_lossy(&response[..response.len().min(MAX_BYTES)]).into_owned();
        let c = Context {
            n,
            at: OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_default(),
            destination: destination.to_string(),
            path: path.to_string(),
            label: self.label(),
            request,
            tools: tools_in(&response),
            response,
            observed_by: by_egress(),
            manifest: self.context_manifest().await,
        };
        let bytes = serde_json::to_vec(&c).map_err(|e| Error::Transcript(e.to_string()))?;
        self.store
            .put_object(
                &domain,
                Area::Contexts,
                &[&self.session_id, &name_of(n)],
                bytes,
            )
            .await?;
        Ok(())
    }

    /// The manifest of the context kept now: the session's state, and the entries
    /// served since the context before, which are taken from the read log.
    async fn context_manifest(&self) -> Option<ReplayManifest> {
        let mut manifest = self.replay_manifest().await.ok()?;
        manifest.returned = self.reads.take_returned();
        Some(manifest)
    }

    /// A session's decision contexts, in order. The owner's alone.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for anyone but the owner, [`Error::NotFound`] for a session
    /// with none, or a store error.
    pub async fn contexts(&self, session: &str) -> Result<Vec<Context>, Error> {
        if self.author != self.grants.owner {
            return Err(Error::Denied(format!(
                "decision contexts are the owner's: {}",
                self.grants.owner
            )));
        }
        for domain in self.domains() {
            let names = self
                .store
                .objects(&domain, Area::Contexts, &[session], false)
                .await?;
            if names.is_empty() {
                continue;
            }
            let mut out = Vec::new();
            for n in names {
                if let Some(text) = self
                    .store
                    .get_object(&domain, Area::Contexts, &[session, &n])
                    .await?
                {
                    out.push(
                        serde_json::from_str(&text)
                            .map_err(|e| Error::Transcript(e.to_string()))?,
                    );
                }
            }
            return Ok(out);
        }
        Err(Error::NotFound(session.to_string()))
    }

    /// The sessions that kept contexts, by domain.
    ///
    /// # Errors
    ///
    /// A store error.
    pub async fn context_sessions(&self) -> Result<Vec<(String, String)>, Error> {
        let mut out = Vec::new();
        for domain in self.domains() {
            for s in self
                .store
                .objects(&domain, Area::Contexts, &[], true)
                .await?
            {
                out.push((domain.clone(), s));
            }
        }
        Ok(out)
    }

    /// Drop the contexts of every session whose last call is older than its domain's
    /// transcript clock. Returns the sessions swept.
    ///
    /// # Errors
    ///
    /// A store error.
    pub async fn sweep_contexts(&self, now: OffsetDateTime) -> Result<Vec<String>, Error> {
        let mut swept = Vec::new();
        for (domain, session) in self.context_sessions().await? {
            let days = self
                .grants
                .domains
                .get(&domain)
                .map_or(dm_policy::DEFAULT_TRANSCRIPT_RETENTION_DAYS, |g| {
                    g.transcript_retention_days()
                });
            let names = self
                .store
                .objects(&domain, Area::Contexts, &[&session], false)
                .await?;
            let Some(last) = names.last() else { continue };
            let Some(text) = self
                .store
                .get_object(&domain, Area::Contexts, &[&session, last])
                .await?
            else {
                continue;
            };
            let at = serde_json::from_str::<Context>(&text).ok().and_then(|c| {
                OffsetDateTime::parse(&c.at, &time::format_description::well_known::Rfc3339).ok()
            });
            if at.is_some_and(|at| now >= at + Duration::days(i64::from(days))) {
                for n in &names {
                    self.store
                        .remove_object(&domain, Area::Contexts, &[&session, n])
                        .await?;
                }
                swept.push(session);
            }
        }
        Ok(swept)
    }

    /// Erase every kept context that carries `body`: an erased entry's text must not
    /// outlive it in what a model was sent. Matched on a stretch of the body long enough
    /// to be its own; returns the contexts removed, as `<session>/<n>`.
    ///
    /// # Errors
    ///
    /// A store error.
    pub async fn erase_from_contexts(&self, body: &str) -> Result<Vec<String>, Error> {
        let probe: String = body.trim().chars().take(160).collect();
        if probe.chars().count() < 24 {
            return Ok(Vec::new());
        }
        // as it appears inside a JSON string in a request body
        let escaped = serde_json::to_string(&probe).unwrap_or_default();
        let escaped = escaped.trim_matches('"').to_string();
        let mut removed = Vec::new();
        for (domain, session) in self.context_sessions().await? {
            for n in self
                .store
                .objects(&domain, Area::Contexts, &[&session], false)
                .await?
            {
                let Some(text) = self
                    .store
                    .get_object(&domain, Area::Contexts, &[&session, &n])
                    .await?
                else {
                    continue;
                };
                if text.contains(&escaped) || text.contains(&probe) {
                    self.store
                        .remove_object(&domain, Area::Contexts, &[&session, &n])
                        .await?;
                    removed.push(format!("{session}/{n}"));
                }
            }
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WriteRequest;

    async fn wrote(m: &Memory, body: &str) -> String {
        let req = WriteRequest {
            body: body.into(),
            ..WriteRequest::default()
        };
        m.write(req).await.unwrap().id
    }

    async fn keep(m: &Memory) {
        m.keep_context("llmao", "v1/chat/completions", b"{}", b"{}")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn each_context_keeps_a_manifest_and_what_was_served_since_the_one_before() {
        let dir = tempfile::tempdir().unwrap();
        let owner = Memory::init(dir.path(), "human:test:me", "dom-me").unwrap();
        let a = wrote(&owner, "The import has five steps.\n").await;
        let b = wrote(&owner, "The export has two steps.\n").await;
        let c = wrote(&owner, "The deploy runs on Friday.\n").await;
        let agent = Memory::open(dir.path()).unwrap().with_author("agent:a");
        agent.get(&b).await.unwrap();
        agent.get(&a).await.unwrap();
        agent.get(&b).await.unwrap();
        keep(&agent).await;
        // a view of the session reads for it: the read log is the session's
        agent.for_request().unwrap().get(&c).await.unwrap();
        agent.get(&a).await.unwrap();
        keep(&agent).await;
        keep(&agent).await;
        let kept = owner.contexts(agent.session_id()).await.unwrap();
        let returned: Vec<Vec<String>> = kept
            .iter()
            .map(|k| k.manifest.clone().expect("a manifest").returned)
            .collect();
        assert_eq!(
            returned,
            [vec![b, a.clone()], vec![c, a], Vec::new()],
            "each id once, in the order first served, and not again in the next context"
        );
        // the rest of each manifest is the session's state
        let state = agent.replay_manifest().await.unwrap();
        for k in &kept {
            let m = k.manifest.clone().unwrap();
            assert_eq!(
                ReplayManifest {
                    returned: Vec::new(),
                    ..m
                },
                state
            );
        }
        assert_eq!(state.log["dom-me"], 3);
        // a session whose reads are not logged names none
        let quiet = Memory::open(dir.path())
            .unwrap()
            .with_author("agent:b")
            .without_read_log();
        quiet
            .get(&kept[0].manifest.clone().unwrap().returned[0])
            .await
            .unwrap();
        quiet.t0(None).await.unwrap();
        keep(&quiet).await;
        let kept = owner.contexts(quiet.session_id()).await.unwrap();
        let m = kept[0].manifest.clone().unwrap();
        assert!(m.returned.is_empty() && m.t0.is_none(), "{m:?}");
    }

    #[test]
    fn a_context_kept_before_manifests_were_still_parses() {
        let old = r#"{"n":1,"at":"2026-11-01T10:00:00Z","destination":"llmao","path":"v1/chat/completions","label":"internal","request":{"messages":[]},"response":"{}","tools":["search"],"observed_by":"egress"}"#;
        let c: Context = serde_json::from_str(old).unwrap();
        assert_eq!((c.n, c.tools.len()), (1, 1));
        assert!(c.manifest.is_none());
        // and one without a manifest is written as it was
        assert_eq!(serde_json::to_string(&c).unwrap(), old);
    }

    #[test]
    fn tools_are_read_from_each_providers_shape() {
        assert_eq!(
            tools_in(
                r#"{"content":[{"type":"tool_use","id":"t1","name":"mcp__memory__get","input":{}}]}"#
            ),
            vec!["mcp__memory__get"]
        );
        assert_eq!(
            tools_in(
                r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"search","arguments":""}}]}}]}"#
            ),
            vec!["search"]
        );
        assert_eq!(
            tools_in(
                r#"{"output":[{"type":"function_call","call_id":"c","name":"write","arguments":"{}"}]}"#
            ),
            vec!["write"]
        );
        assert_eq!(
            tools_in(
                r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"t0","args":{}}}]}}]}"#
            ),
            vec!["t0"]
        );
        assert!(tools_in(r#"{"choices":[{"message":{"content":"DR-19 is open."}}]}"#).is_empty());
        // the Responses API's stream: the name before the type, and a namespace
        let streamed = "data: {\"type\":\"response.output_item.added\",\"item\":{\"arguments\":\"\",\"name\":\"search\",\"type\":\"function_call\",\"namespace\":\"mcp__memory\"}}\n\ndata: {\"type\":\"response.completed\"}\n";
        assert_eq!(tools_in(streamed), vec!["mcp__memory__search"]);
    }

    #[test]
    fn the_answers_text_is_read_from_streams_and_wholes() {
        let responses = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"DR-19 is \"}\ndata: {\"type\":\"response.function_call_arguments.delta\",\"delta\":\"{}\"}\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"open.\"}\n";
        assert_eq!(text_in(responses), "DR-19 is open.");
        let chat = "data: {\"choices\":[{\"delta\":{\"content\":\"Still \"}}]}\ndata: {\"choices\":[{\"delta\":{\"content\":\"proposed.\"}}]}\n";
        assert_eq!(text_in(chat), "Still proposed.");
        assert_eq!(
            text_in(r#"{"content":[{"type":"text","text":"Open."}]}"#),
            "Open."
        );
        assert_eq!(
            text_in(
                r#"{"output":[{"type":"message","content":[{"type":"output_text","text":"Open."}]}]}"#
            ),
            "Open."
        );
    }
}
