//! A typed client for the memory server over MCP.
//!
//! The launcher's task mode and, later, an orchestrator reach the ledger and the record
//! through this, over the same tool surface a harness uses. The rules of the ledger live
//! on the server; this crate knows the tool names and the shape of their answers, and
//! nothing else. It is transport-agnostic: in process over a byte pipe, over standard
//! input and output to a `memory serve`, or over HTTP to a remote server, the calls are
//! the same.

use rmcp::model::CallToolRequestParams;
use rmcp::service::{RoleClient, RunningService, ServiceExt};
use rmcp::transport::IntoTransport;
use serde_json::{json, Map, Value};

/// What can go wrong talking to the server.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The transport or the protocol failed.
    #[error("mcp: {0}")]
    Mcp(String),
    /// The server answered a tool call with an error.
    #[error("{tool}: {message}")]
    Tool {
        /// Which tool.
        tool: String,
        /// The server's message.
        message: String,
    },
    /// The answer was not in the shape this client expects.
    #[error("{tool}: unexpected answer: {what}")]
    Shape {
        /// Which tool.
        tool: String,
        /// What was wrong.
        what: String,
    },
}

type Result<T> = std::result::Result<T, Error>;

/// One task as the server reports it, with only the fields a client acts on typed; the
/// whole answer is kept in `raw`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Task {
    /// The task id.
    pub id: String,
    /// One line.
    pub title: String,
    /// What is to be done.
    pub body: String,
    /// The domain it was filed in.
    pub domain: String,
    /// proposed, admitted, claimed, completed, or invalidated.
    pub status: String,
    /// Tasks this one waits for.
    pub depends_on: Vec<String>,
    /// Capabilities a session must carry.
    pub requirements: Vec<String>,
    /// The handoff that closed it, or the one a release or lost lease left behind.
    pub handoff: Option<String>,
    /// The server's whole answer.
    pub raw: Value,
}

impl Task {
    fn from_value(tool: &str, v: &Value) -> Result<Task> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        let list = |k: &str| {
            v.get(k)
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default()
        };
        let need = |k: &str| {
            s(k).ok_or_else(|| Error::Shape {
                tool: tool.to_string(),
                what: format!("task without {k}"),
            })
        };
        Ok(Task {
            id: need("id")?,
            title: need("title")?,
            body: s("body").unwrap_or_default(),
            domain: s("domain").unwrap_or_default(),
            status: need("status")?,
            depends_on: list("depends_on"),
            requirements: list("requirements"),
            handoff: s("handoff"),
            raw: v.clone(),
        })
    }
}

/// An entry as `get` returns it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The entry id.
    pub id: String,
    /// Its title.
    pub title: String,
    /// decision, fact, convention, method, or handoff.
    pub entry_type: String,
    /// The Markdown body.
    pub body: String,
}

/// An entry to write.
#[derive(Clone, Debug, Default)]
pub struct NewEntry<'a> {
    /// decision, fact, convention, method, or handoff.
    pub kind: &'a str,
    /// One line.
    pub title: &'a str,
    /// Markdown.
    pub body: &'a str,
    /// high, medium, or low.
    pub confidence: Option<&'a str>,
    /// Where it was distilled from.
    pub source: Vec<String>,
    /// A granted domain; the only one when omitted.
    pub domain: Option<&'a str>,
}

/// The client. Drop it, or call [`MemoryClient::close`], to end the session.
pub struct MemoryClient {
    service: RunningService<RoleClient, ()>,
}

impl MemoryClient {
    /// Connect over any rmcp transport: a `(reader, writer)` pair, a child process, or
    /// an HTTP transport.
    ///
    /// # Errors
    ///
    /// When the MCP handshake fails.
    pub async fn connect<T, E, A>(transport: T) -> Result<MemoryClient>
    where
        T: IntoTransport<RoleClient, E, A>,
        E: std::error::Error + Send + Sync + 'static,
    {
        let service = ().serve(transport).await.map_err(|e| Error::Mcp(e.to_string()))?;
        Ok(MemoryClient { service })
    }

    /// End the session.
    ///
    /// # Errors
    ///
    /// When the transport fails to close cleanly.
    pub async fn close(self) -> Result<()> {
        self.service
            .cancel()
            .await
            .map(|_| ())
            .map_err(|e| Error::Mcp(e.to_string()))
    }

    /// Call any tool with JSON arguments; the answer is the tool's JSON.
    ///
    /// # Errors
    ///
    /// A transport error, or the tool's refusal.
    pub async fn call(&self, tool: &str, args: Value) -> Result<Value> {
        let arguments: Map<String, Value> = match args {
            Value::Object(m) => m,
            _ => Map::new(),
        };
        let result = self
            .service
            .call_tool(CallToolRequestParams::new(tool.to_string()).with_arguments(arguments))
            .await
            .map_err(|e| match e {
                // the server declined the call: a missing id, a lease held elsewhere
                rmcp::service::ServiceError::McpError(data) => Error::Tool {
                    tool: tool.to_string(),
                    message: data.message.to_string(),
                },
                other => Error::Mcp(other.to_string()),
            })?;
        let text = result
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .unwrap_or_default();
        if result.is_error.unwrap_or(false) {
            return Err(Error::Tool {
                tool: tool.to_string(),
                message: text,
            });
        }
        Ok(serde_json::from_str(&text).unwrap_or(Value::String(text)))
    }

    fn task_of(tool: &str, v: &Value) -> Result<Option<Task>> {
        match v.get("task") {
            None | Some(Value::Null) => Ok(None),
            Some(t) => Task::from_value(tool, t).map(Some),
        }
    }

    /// T0 for a domain, as text.
    ///
    /// # Errors
    ///
    /// A transport or tool error.
    pub async fn t0(&self, domain: Option<&str>) -> Result<String> {
        let v = self.call("t0", json!({ "domain": domain })).await?;
        Ok(match v {
            Value::String(s) => s,
            other => other.to_string(),
        })
    }

    /// One entry in full.
    ///
    /// # Errors
    ///
    /// A transport or tool error, including an id that is not there.
    pub async fn get(&self, id: &str) -> Result<Entry> {
        let v = self.call("get", json!({ "id": id })).await?;
        let s = |k: &str| {
            v.get(k)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        if v.get("id").is_none() {
            return Err(Error::Shape {
                tool: "get".into(),
                what: "no id in the answer".into(),
            });
        }
        Ok(Entry {
            id: s("id"),
            title: s("title"),
            entry_type: s("type"),
            body: s("body"),
        })
    }

    /// Write a handoff entry; returns its id.
    ///
    /// # Errors
    ///
    /// A transport or tool error.
    pub async fn write_handoff(
        &self,
        title: &str,
        body: &str,
        domain: Option<&str>,
    ) -> Result<String> {
        let v = self
            .call(
                "write",
                json!({ "body": body, "title": title, "entry_type": "handoff", "domain": domain }),
            )
            .await?;
        v.get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| Error::Shape {
                tool: "write".into(),
                what: "no id in the answer".into(),
            })
    }

    /// Write an entry of any kind, with a confidence and sources; returns its id. Mined
    /// knowledge lands this way: raw tier, low confidence, the transcript as its source.
    ///
    /// # Errors
    ///
    /// A transport or tool error.
    pub async fn write_entry(&self, entry: &NewEntry<'_>) -> Result<String> {
        let v = self
            .call(
                "write",
                json!({
                    "body": entry.body, "title": entry.title, "entry_type": entry.kind,
                    "confidence": entry.confidence, "source": entry.source, "domain": entry.domain,
                }),
            )
            .await?;
        v.get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| Error::Shape {
                tool: "write".into(),
                what: "no id in the answer".into(),
            })
    }

    /// Handoffs in a domain, newest first, as `(id, title)`.
    ///
    /// # Errors
    ///
    /// A transport or tool error.
    pub async fn handoffs(
        &self,
        domain: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(String, String)>> {
        let v = self
            .call(
                "list",
                json!({ "domain": domain, "kind": "handoff", "limit": limit }),
            )
            .await?;
        let hits = v
            .get("entries")
            .or_else(|| v.get("hits"))
            .and_then(Value::as_array)
            .cloned()
            .or_else(|| v.as_array().cloned())
            .unwrap_or_default();
        Ok(hits
            .iter()
            .filter_map(|h| {
                Some((
                    h.get("id")?.as_str()?.to_string(),
                    h.get("title")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                ))
            })
            .collect())
    }

    /// Claim the next task this session may take, under a lease of `lease_seconds`.
    ///
    /// # Errors
    ///
    /// A transport or tool error.
    pub async fn claim_next(
        &self,
        domain: Option<&str>,
        lease_seconds: i64,
    ) -> Result<Option<Task>> {
        let v = self
            .call(
                "claim_next",
                json!({ "domain": domain, "lease_seconds": lease_seconds }),
            )
            .await?;
        Self::task_of("claim_next", &v)
    }

    /// Claim one named task, under the same rules as [`Client::claim_next`].
    ///
    /// # Errors
    ///
    /// A transport or tool error, including a task that is not claimable.
    pub async fn claim(&self, id: &str, lease_seconds: i64) -> Result<Option<Task>> {
        let v = self
            .call("claim", json!({ "id": id, "lease_seconds": lease_seconds }))
            .await?;
        Self::task_of("claim", &v)
    }

    /// Renew the lease on a task this session holds.
    ///
    /// # Errors
    ///
    /// A transport or tool error, including a lease already lost.
    pub async fn heartbeat(&self, id: &str, lease_seconds: i64) -> Result<Task> {
        let v = self
            .call(
                "heartbeat",
                json!({ "id": id, "lease_seconds": lease_seconds }),
            )
            .await?;
        Self::task_of("heartbeat", &v)?.ok_or_else(|| Error::Shape {
            tool: "heartbeat".into(),
            what: "no task in the answer".into(),
        })
    }

    /// Give a task back, with the handoff the next claimant should start from.
    ///
    /// # Errors
    ///
    /// A transport or tool error.
    pub async fn release(&self, id: &str, handoff: Option<&str>) -> Result<Task> {
        let v = self
            .call("release", json!({ "id": id, "handoff": handoff }))
            .await?;
        Self::task_of("release", &v)?.ok_or_else(|| Error::Shape {
            tool: "release".into(),
            what: "no task in the answer".into(),
        })
    }

    /// Complete a task with the handoff that describes what happened.
    ///
    /// # Errors
    ///
    /// A transport or tool error.
    pub async fn complete(&self, id: &str, handoff: &str) -> Result<Task> {
        let v = self
            .call("complete", json!({ "id": id, "handoff": handoff }))
            .await?;
        Self::task_of("complete", &v)?.ok_or_else(|| Error::Shape {
            tool: "complete".into(),
            what: "no task in the answer".into(),
        })
    }

    /// One task.
    ///
    /// # Errors
    ///
    /// A transport or tool error.
    pub async fn get_task(&self, id: &str) -> Result<Task> {
        let v = self.call("get_task", json!({ "id": id })).await?;
        Self::task_of("get_task", &v)?.ok_or_else(|| Error::Shape {
            tool: "get_task".into(),
            what: "no task in the answer".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dm_server::{mcp::McpServer, Memory};
    use std::sync::Arc;

    /// A server and a client joined by an in-process pipe: the same JSON-RPC as over
    /// stdio, with no process between.
    async fn pair() -> (tempfile::TempDir, Arc<Memory>, MemoryClient) {
        let dir = tempfile::tempdir().unwrap();
        let memory = Arc::new(
            Memory::init(dir.path(), "human:test:me", "dom-me")
                .unwrap()
                .with_author("agent:test"),
        );
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (sr, sw) = tokio::io::split(server_io);
        let server = McpServer::new(memory.clone());
        tokio::spawn(async move {
            if let Ok(s) = server.serve((sr, sw)).await {
                let _ = s.waiting().await;
            }
        });
        let (cr, cw) = tokio::io::split(client_io);
        let client = MemoryClient::connect((cr, cw)).await.unwrap();
        (dir, memory, client)
    }

    #[tokio::test]
    async fn the_ledger_round_trip_through_the_tool_surface() {
        let (_dir, memory, client) = pair().await;
        assert!(client.claim_next(None, 60).await.unwrap().is_none());
        let created = memory
            .create_task(dm_ledger_spec("Finish the import", "Steps 3 to 5."))
            .await
            .unwrap();
        memory.admit(&created.task.id).await.unwrap();

        let t = client.claim_next(None, 60).await.unwrap().unwrap();
        assert_eq!(t.id, created.task.id);
        assert_eq!(t.status, "claimed");
        assert_eq!(t.title, "Finish the import");
        assert_eq!(t.body, "Steps 3 to 5.");
        let t = client.heartbeat(&t.id, 60).await.unwrap();
        assert_eq!(t.status, "claimed");

        let h = client
            .write_handoff("Stopped at step 4", "Step 3 done, 4 half done.", None)
            .await
            .unwrap();
        assert!(h.starts_with("mem-"));
        let t = client.release(&t.id, Some(&h)).await.unwrap();
        assert_eq!(t.status, "admitted");
        assert_eq!(t.handoff.as_deref(), Some(h.as_str()));

        let t = client.claim_next(None, 60).await.unwrap().unwrap();
        let left = client.get(t.handoff.as_deref().unwrap()).await.unwrap();
        assert_eq!(left.entry_type, "handoff");
        assert!(left.body.contains("half done"));
        let mined = client
            .write_entry(&NewEntry {
                kind: "fact",
                title: "Mined: the import has five steps",
                body: "Five steps, the fourth is the slow one.",
                confidence: Some("low"),
                source: vec!["transcript:/home/user/.claude/projects/p/s.jsonl".into()],
                domain: None,
            })
            .await
            .unwrap();
        let e = client.get(&mined).await.unwrap();
        assert_eq!(e.entry_type, "fact");
        let h2 = client
            .write_handoff("Import done", "All five steps.", None)
            .await
            .unwrap();
        let t = client.complete(&t.id, &h2).await.unwrap();
        assert_eq!(t.status, "completed");
        let t = client.get_task(&t.id).await.unwrap();
        assert_eq!(t.handoff.as_deref(), Some(h2.as_str()));

        let t0 = client.t0(None).await.unwrap();
        assert!(t0.contains("Import done"), "{t0}");
        let handoffs = client.handoffs(None, 10).await.unwrap();
        assert!(handoffs.iter().any(|(id, _)| id == &h2), "{handoffs:?}");
        client.close().await.unwrap();
    }

    #[tokio::test]
    async fn a_tool_error_is_reported_as_one() {
        let (_dir, _memory, client) = pair().await;
        let e = client.get("mem-me-nothere").await.unwrap_err();
        assert!(matches!(e, Error::Tool { .. }), "{e}");
        let e = client
            .complete("task-me-nothere", "mem-x")
            .await
            .unwrap_err();
        assert!(matches!(e, Error::Tool { .. }), "{e}");
    }

    fn dm_ledger_spec(title: &str, body: &str) -> dm_ledger::TaskSpec {
        dm_ledger::TaskSpec {
            domain: String::new(),
            title: title.into(),
            body: body.into(),
            requirements: Vec::new(),
            depends_on: Vec::new(),
            origin: None,
            upstream_event: None,
            created_by: "human:test:me".into(),
            priority: 0,
            classification: Default::default(),
        }
    }

    const IMPORT: &str = "# Import\n\nThe import has five steps.\n";

    /// A session that reads T0, writes an entry, searches, opens it and asks for one that
    /// is not there; its tape, and what the write and the first get answered.
    async fn recorded() -> (tempfile::TempDir, dm_server::tape::Tape, Value, Value) {
        let (dir, memory, client) = pair().await;
        memory
            .tape_start(dm_server::tape::TapeStart {
                harness: "claude".into(),
                prompt: Some("Finish the import.".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        client.call("t0", json!({})).await.unwrap();
        let wrote = client
            .call("write", json!({ "body": IMPORT }))
            .await
            .unwrap();
        client
            .call("search", json!({"query": "import"}))
            .await
            .unwrap();
        let first = client
            .call("get", json!({"id": wrote["id"]}))
            .await
            .unwrap();
        assert!(client
            .call("get", json!({"id": "mem-me-nothere"}))
            .await
            .is_err());
        client.close().await.unwrap();
        let tape = Memory::open(dir.path())
            .unwrap()
            .tape(memory.session_id())
            .await
            .unwrap();
        (dir, tape, wrote, first)
    }

    /// A client of a re-run of `tape` over the root at `dir`.
    async fn re_run(
        dir: &std::path::Path,
        tape: dm_server::tape::Tape,
    ) -> (Arc<dm_server::tape::Playback>, MemoryClient) {
        let rerun = Arc::new(Memory::open(dir).unwrap().with_author("agent:test"));
        let play = Arc::new(dm_server::tape::Playback::new(tape));
        rerun.set_playback(play.clone());
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (sr, sw) = tokio::io::split(server_io);
        let server = McpServer::new(rerun);
        tokio::spawn(async move {
            if let Ok(s) = server.serve((sr, sw)).await {
                let _ = s.waiting().await;
            }
        });
        let (cr, cw) = tokio::io::split(client_io);
        (play, MemoryClient::connect((cr, cw)).await.unwrap())
    }

    #[tokio::test]
    async fn a_session_s_calls_and_results_are_kept_on_its_tape_for_the_owner() {
        let (dir, tape, _, _) = recorded().await;
        assert_eq!(
            tape.header.start.prompt.as_deref(),
            Some("Finish the import.")
        );
        let tools: Vec<&str> = tape.calls.iter().map(|c| c.tool.as_str()).collect();
        assert_eq!(tools, ["t0", "write", "search", "get", "get"]);
        assert!(tape.calls[4].is_error, "a refusal is on the tape too");
        let agent = Memory::open(dir.path()).unwrap().with_author("agent:test");
        assert!(
            agent.tape(&tape.header.session).await.is_err(),
            "the owner's alone"
        );
    }

    #[tokio::test]
    async fn a_session_s_tape_answers_its_re_run_until_it_diverges() {
        let (dir, tape, wrote, first) = recorded().await;
        // the re-run: the same calls but the search, answered from the tape
        let (play, again) = re_run(dir.path(), tape).await;
        again.call("t0", json!({})).await.unwrap();
        let rewrote = again
            .call("write", json!({ "body": IMPORT }))
            .await
            .unwrap();
        assert_eq!(rewrote["id"], wrote["id"], "the recorded answer");
        assert_eq!(
            again.call("get", json!({"id": wrote["id"]})).await.unwrap(),
            first
        );
        assert!(again
            .call("get", json!({"id": "mem-me-nothere"}))
            .await
            .is_err());
        // off the tape: a write is captured, a read is served live, nothing is applied
        let captured = again
            .call("write", json!({"body": "# Retry\n\nRetry step four.\n"}))
            .await
            .unwrap();
        assert!(captured["reenactment"]
            .as_str()
            .unwrap()
            .contains("captured"));
        let live = again
            .call("search", json!({"query": "retry"}))
            .await
            .unwrap();
        assert_eq!(live["hits"].as_array().unwrap().len(), 0, "{live}");
        assert!(again
            .call("fetch", json!({"url": "https://example.org/"}))
            .await
            .is_err());
        again.close().await.unwrap();
        let played = play.played();
        assert_eq!((played.steps_to_divergence, played.diverged), (4, true));
        let served: Vec<&str> = played.steps.iter().map(|s| s.served.as_str()).collect();
        assert_eq!(
            served,
            ["tape", "tape", "tape", "tape", "captured", "live", "refused"]
        );
        // the record holds one entry, the original's, and one tape, the original's
        let owner = Memory::open(dir.path()).unwrap();
        assert_eq!(owner.list(None, None, 10).await.unwrap().len(), 1);
        assert_eq!(owner.tape_sessions().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn erasing_an_entry_takes_the_calls_that_served_it_off_the_tape() {
        let body = "The import has five steps, and the fourth rereads every row once more.";
        let (dir2, m2, c2) = pair().await;
        let w = c2
            .call("write", json!({"body": format!("# Steps\n\n{body}\n")}))
            .await
            .unwrap();
        let wid = w["id"].as_str().unwrap().to_string();
        c2.call("get", json!({"id": wid})).await.unwrap();
        c2.call("t0", json!({})).await.unwrap();
        c2.close().await.unwrap();
        let owner2 = Memory::open(dir2.path()).unwrap();
        let s2 = m2.session_id().to_string();
        assert_eq!(owner2.tape(&s2).await.unwrap().calls.len(), 3);
        owner2
            .erase(&wid, dm_core::erasure::Reason::Mistaken)
            .await
            .unwrap();
        let left: Vec<String> = owner2
            .tape(&s2)
            .await
            .unwrap()
            .calls
            .into_iter()
            .map(|c| c.tool)
            .collect();
        assert!(!left.contains(&"get".to_string()), "{left:?}");
    }
}
