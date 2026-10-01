//! The MCP binding: the operations of the paper's Appendix A as tools, and T0 as a
//! resource. Every tool is a thin call into [`Memory`]; nothing lives only here.
//!
//! A harness connects over stdio. It gets `search`, `get`, `write`, `list`, `t0`, the
//! ledger operations, and a `memory://t0/<domain>` resource per granted domain. What it
//! never gets is a path on disk, a credential, or an entry its grants do not admit.

use crate::{Error, Memory, WriteRequest};
use dm_index::Hit;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, Implementation, ListResourcesResult, PaginatedRequestParams,
    ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, Resource,
    ResourceContents, ServerCapabilities, ServerConfig,
};
use rmcp::service::RequestContext;
use rmcp::{
    schemars, tool, tool_handler, tool_router, ErrorData as McpError, RoleServer, ServerHandler,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use time::format_description::well_known::{Iso8601, Rfc3339};
use time::Duration;

const DEFAULT_LEASE_SECONDS: i64 = 30 * 60;
const DEFAULT_LIMIT: usize = 10;
const MAX_LIMIT: usize = 100;

fn mcp_err(e: Error) -> McpError {
    match e {
        Error::Denied(_) | Error::NotFound(_) => McpError::invalid_params(e.to_string(), None),
        Error::Ledger(
            dm_ledger::Error::NotFound(_)
            | dm_ledger::Error::WrongState { .. }
            | dm_ledger::Error::NotHolder { .. },
        ) => McpError::invalid_params(e.to_string(), None),
        other => McpError::internal_error(other.to_string(), None),
    }
}

fn ok(value: Value) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![ContentBlock::json(value)?]))
}

fn rfc3339(t: time::OffsetDateTime) -> String {
    t.format(&Rfc3339).unwrap_or_default()
}

fn hit_json(h: &Hit) -> Value {
    json!({
        "id": h.id,
        "domain": h.domain,
        "type": h.entry_type.to_string(),
        "trust_tier": h.trust_tier.to_string(),
        "classification": h.classification.to_string(),
        "title": h.title,
        "summary": h.summary,
    })
}

fn lease(seconds: Option<i64>) -> Duration {
    Duration::seconds(
        seconds
            .unwrap_or(DEFAULT_LEASE_SECONDS)
            .clamp(10, 24 * 60 * 60),
    )
}

fn limit(n: Option<usize>) -> usize {
    n.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
}

fn parse<T: std::str::FromStr>(what: &str, s: Option<String>) -> Result<Option<T>, McpError>
where
    T::Err: std::fmt::Display,
{
    s.map(|v| {
        v.parse::<T>()
            .map_err(|e| McpError::invalid_params(format!("{what}: {e}"), None))
    })
    .transpose()
}

/// Parameters of `search`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct SearchParams {
    /// Free text. Words are matched as phrases and ANDed; ranking is BM25.
    pub query: String,
    /// A granted domain. Omit for every granted domain.
    #[serde(default)]
    pub domain: Option<String>,
    /// How many hits, 1 to 100. Default 10.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Parameters of `get`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct GetParams {
    /// The entry id, as a hit or T0 names it.
    pub id: String,
}

/// Parameters of `write`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct WriteParams {
    /// The Markdown body. Required.
    pub body: String,
    /// decision, fact, convention, method, or handoff. Default fact.
    #[serde(default)]
    pub entry_type: Option<String>,
    /// A title; else the body's first heading.
    #[serde(default)]
    pub title: Option<String>,
    /// A granted domain; the only one when omitted.
    #[serde(default)]
    pub domain: Option<String>,
    /// Links or citations this was distilled from.
    #[serde(default)]
    pub source: Vec<String>,
    /// high, medium, or low.
    #[serde(default)]
    pub confidence: Option<String>,
    /// public, internal, or restricted. Raised to what this session has read.
    #[serde(default)]
    pub classification: Option<String>,
    /// The id of the entry this one replaces.
    #[serde(default)]
    pub supersedes: Option<String>,
    /// A date, YYYY-MM-DD, to re-check a fact by.
    #[serde(default)]
    pub review_by: Option<String>,
    /// A one-line summary; else the body's first paragraph.
    #[serde(default)]
    pub summary: Option<String>,
}

/// Parameters of `list`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct ListParams {
    /// A granted domain. Omit for every granted domain.
    #[serde(default)]
    pub domain: Option<String>,
    /// decision, fact, convention, method, or handoff. Omit for every kind.
    #[serde(default)]
    pub kind: Option<String>,
    /// How many, 1 to 100. Default 10.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Parameters that name a domain, or let it default.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct DomainParams {
    /// A granted domain; the only one when omitted.
    #[serde(default)]
    pub domain: Option<String>,
}

/// Parameters of `create_task`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct CreateTaskParams {
    /// One line.
    pub title: String,
    /// What is to be done, in Markdown.
    pub body: String,
    /// A granted domain; the only one when omitted.
    #[serde(default)]
    pub domain: Option<String>,
    /// Capabilities a session must carry to claim it, such as `tool:github`.
    #[serde(default)]
    pub requirements: Vec<String>,
    /// Task ids that must complete first.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Higher is claimed first. Default 0.
    #[serde(default)]
    pub priority: Option<i64>,
}

/// Parameters that name a task.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct TaskIdParams {
    /// The task id.
    pub id: String,
}

/// Parameters of `claim_next`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct ClaimNextParams {
    /// A granted domain; the only one when omitted.
    #[serde(default)]
    pub domain: Option<String>,
    /// Lease length in seconds, 10 to 86400. Default 1800. Heartbeat before it runs out.
    #[serde(default)]
    pub lease_seconds: Option<i64>,
}

/// Parameters of `claim` and `heartbeat`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct LeaseParams {
    /// The task id.
    pub id: String,
    /// Lease length in seconds, 10 to 86400. Default 1800.
    #[serde(default)]
    pub lease_seconds: Option<i64>,
}

/// Parameters of `complete`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct CompleteParams {
    /// The task id.
    pub id: String,
    /// The id of the handoff entry you wrote with `write` describing what happened.
    pub handoff: String,
}

/// Parameters of `invalidate_task`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct InvalidateParams {
    /// The task id.
    pub id: String,
    /// obsolete, invalid, or duplicate, with a few words.
    pub reason: String,
    /// The task that replaces it, when a duplicate.
    #[serde(default)]
    pub superseded_by: Option<String>,
}

/// Parameters of `list_tasks`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct ListTasksParams {
    /// A granted domain; the only one when omitted.
    #[serde(default)]
    pub domain: Option<String>,
    /// proposed, admitted, claimed, completed, or invalidated. Default admitted.
    #[serde(default)]
    pub status: Option<String>,
}

fn task_status(s: Option<String>) -> Result<dm_ledger::Status, McpError> {
    Ok(match s.as_deref().unwrap_or("admitted") {
        "proposed" => dm_ledger::Status::Proposed,
        "admitted" => dm_ledger::Status::Admitted,
        "claimed" => dm_ledger::Status::Claimed,
        "completed" => dm_ledger::Status::Completed,
        "invalidated" => dm_ledger::Status::Invalidated,
        other => {
            return Err(McpError::invalid_params(
                format!(
                    "status {other:?} is not one of proposed, admitted, claimed, completed, invalidated"
                ),
                None,
            ))
        }
    })
}

fn confidence(s: Option<&str>) -> Result<Option<dm_core::Confidence>, McpError> {
    Ok(match s {
        None => None,
        Some("high") => Some(dm_core::Confidence::High),
        Some("medium") => Some(dm_core::Confidence::Medium),
        Some("low") => Some(dm_core::Confidence::Low),
        Some(other) => {
            return Err(McpError::invalid_params(
                format!("confidence {other:?} is not high, medium, or low"),
                None,
            ))
        }
    })
}

/// The MCP server over one [`Memory`].
#[derive(Clone)]
pub struct McpServer {
    memory: Arc<Memory>,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl McpServer {
    /// A server over an opened memory.
    #[must_use]
    pub fn new(memory: Arc<Memory>) -> Self {
        Self {
            memory,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Ranked search over memory, summaries only. Open a hit with `get` if its summary warrants it. Search before writing a fact that may already exist."
    )]
    async fn search(&self, p: Parameters<SearchParams>) -> Result<CallToolResult, McpError> {
        let p = p.0;
        let hits = self
            .memory
            .search(&p.query, p.domain.as_deref(), limit(p.limit))
            .await
            .map_err(mcp_err)?;
        ok(json!({ "hits": hits.iter().map(hit_json).collect::<Vec<_>>() }))
    }

    #[tool(
        description = "One entry in full: its frontmatter and Markdown body. Reading it raises this session's label to the entry's classification."
    )]
    async fn get(&self, p: Parameters<GetParams>) -> Result<CallToolResult, McpError> {
        let e = self.memory.get(&p.0.id).await.map_err(mcp_err)?;
        let f = &e.front;
        ok(json!({
            "id": f.id,
            "domain": f.domain,
            "type": f.entry_type.to_string(),
            "author": f.author,
            "created": rfc3339(f.created),
            "trust_tier": f.trust_tier.to_string(),
            "classification": f.classification.to_string(),
            "source": f.source,
            "supersedes": f.supersedes,
            "review_by": f.review_by.map(|d| d.to_string()),
            "title": e.title(),
            "body": e.body,
        }))
    }

    #[tool(
        description = "Write an entry. It lands at raw trust under this session's author. Write a `handoff` when you stop so the next session continues from it, not from your transcript; write a `decision` with context, options, choice and rationale; write a `fact` with a review_by date."
    )]
    async fn write(&self, p: Parameters<WriteParams>) -> Result<CallToolResult, McpError> {
        let p = p.0;
        let review_by = p
            .review_by
            .map(|s| {
                time::Date::parse(&s, &Iso8601::DATE)
                    .map_err(|e| McpError::invalid_params(format!("review_by: {e}"), None))
            })
            .transpose()?;
        let req = WriteRequest {
            domain: p.domain,
            entry_type: parse("entry_type", p.entry_type)?.unwrap_or_default(),
            title: p.title,
            body: p.body,
            source: p.source,
            confidence: confidence(p.confidence.as_deref())?,
            classification: parse("classification", p.classification)?,
            supersedes: p.supersedes,
            review_by,
            summary: p.summary,
        };
        let w = self.memory.write(req).await.map_err(mcp_err)?;
        ok(json!({
            "id": w.id,
            "domain": w.domain,
            "classification": w.classification.to_string(),
        }))
    }

    #[tool(
        description = "Entries of one kind, newest first: the latest handoffs, the standing decisions. A keyed lookup, not a search."
    )]
    async fn list(&self, p: Parameters<ListParams>) -> Result<CallToolResult, McpError> {
        let p = p.0;
        let kind = parse::<dm_core::EntryType>("kind", p.kind)?;
        let hits = self
            .memory
            .list(p.domain.as_deref(), kind, limit(p.limit))
            .await
            .map_err(mcp_err)?;
        ok(json!({ "entries": hits.iter().map(hit_json).collect::<Vec<_>>() }))
    }

    #[tool(
        description = "T0: the small always-loaded index for a domain. Load this first. It names the latest handoffs, the standing decisions, and where to look; everything else is one `get` away."
    )]
    async fn t0(&self, p: Parameters<DomainParams>) -> Result<CallToolResult, McpError> {
        let text = self
            .memory
            .t0(p.0.domain.as_deref())
            .await
            .map_err(mcp_err)?;
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(
        description = "File a task on the ledger. It is born proposed and becomes claimable once the owner admits it."
    )]
    async fn create_task(
        &self,
        p: Parameters<CreateTaskParams>,
    ) -> Result<CallToolResult, McpError> {
        let p = p.0;
        let spec = dm_ledger::TaskSpec {
            domain: p.domain.unwrap_or_default(),
            title: p.title,
            body: p.body,
            requirements: p.requirements,
            depends_on: p.depends_on,
            origin: None,
            upstream_event: None,
            created_by: String::new(),
            classification: dm_core::Label::Public,
            priority: p.priority.unwrap_or(0),
        };
        let c = self.memory.create_task(spec).await.map_err(mcp_err)?;
        ok(json!({ "task": c.task, "deduplicated": c.deduplicated }))
    }

    #[tool(
        description = "Admit a proposed task so it can be claimed. On a laptop the owner is the triage."
    )]
    async fn admit_task(&self, p: Parameters<TaskIdParams>) -> Result<CallToolResult, McpError> {
        let t = self.memory.admit(&p.0.id).await.map_err(mcp_err)?;
        ok(json!({ "task": t }))
    }

    #[tool(
        description = "Claim the highest-ranked task this session can take, under a lease. Heartbeat before the lease runs out; a lease nobody renews is lost at expiry and the task returns to the pool. Returns no task when nothing is claimable."
    )]
    async fn claim_next(&self, p: Parameters<ClaimNextParams>) -> Result<CallToolResult, McpError> {
        let p = p.0;
        let c = self
            .memory
            .claim_next(p.domain.as_deref(), lease(p.lease_seconds))
            .await
            .map_err(mcp_err)?;
        match c {
            None => ok(json!({ "task": null })),
            Some(c) => ok(json!({ "task": c.task, "lease_until": rfc3339(c.lease_until) })),
        }
    }

    #[tool(description = "Claim one named task, under the same rules as claim_next.")]
    async fn claim(&self, p: Parameters<LeaseParams>) -> Result<CallToolResult, McpError> {
        let p = p.0;
        let c = self
            .memory
            .claim(&p.id, lease(p.lease_seconds))
            .await
            .map_err(mcp_err)?;
        ok(json!({ "task": c.task, "lease_until": rfc3339(c.lease_until) }))
    }

    #[tool(
        description = "Renew your lease on a task. Refused if the lease already expired: stop work, the task is back in the pool. The returned task shows whether it was invalidated meanwhile."
    )]
    async fn heartbeat(&self, p: Parameters<LeaseParams>) -> Result<CallToolResult, McpError> {
        let p = p.0;
        let t = self
            .memory
            .heartbeat(&p.id, lease(p.lease_seconds))
            .await
            .map_err(mcp_err)?;
        ok(json!({ "task": t }))
    }

    #[tool(
        description = "Complete a task with the handoff entry you wrote describing what happened. Write the handoff first with `write`, entry_type handoff."
    )]
    async fn complete(&self, p: Parameters<CompleteParams>) -> Result<CallToolResult, McpError> {
        let p = p.0;
        let t = self
            .memory
            .complete(&p.id, &p.handoff)
            .await
            .map_err(mcp_err)?;
        ok(json!({ "task": t }))
    }

    #[tool(description = "Give a task back to the pool without completing it.")]
    async fn release(&self, p: Parameters<TaskIdParams>) -> Result<CallToolResult, McpError> {
        let t = self.memory.release(&p.0.id).await.map_err(mcp_err)?;
        ok(json!({ "task": t }))
    }

    #[tool(
        description = "Close a task without doing it: obsolete, invalid, or a duplicate of another."
    )]
    async fn invalidate_task(
        &self,
        p: Parameters<InvalidateParams>,
    ) -> Result<CallToolResult, McpError> {
        let p = p.0;
        let t = self
            .memory
            .invalidate(&p.id, &p.reason, p.superseded_by.as_deref())
            .await
            .map_err(mcp_err)?;
        ok(json!({ "task": t }))
    }

    #[tool(description = "One task, with its status, holder, lease, and handoff.")]
    async fn get_task(&self, p: Parameters<TaskIdParams>) -> Result<CallToolResult, McpError> {
        let t = self.memory.get_task(&p.0.id).await.map_err(mcp_err)?;
        ok(json!({ "task": t }))
    }

    #[tool(description = "Tasks in a domain with a status, ranked as the queue ranks them.")]
    async fn list_tasks(&self, p: Parameters<ListTasksParams>) -> Result<CallToolResult, McpError> {
        let p = p.0;
        let status = task_status(p.status)?;
        let tasks = self
            .memory
            .tasks(p.domain.as_deref(), status)
            .await
            .map_err(mcp_err)?;
        ok(json!({ "tasks": tasks }))
    }
}

const INSTRUCTIONS: &str = "This is your memory. Start by loading T0 for your domain (the `t0` tool or the memory://t0/<domain> resource). \
Search before writing a fact that may exist. When you stop, write a `handoff` entry; the next session starts from it, not from your transcript. \
Work comes from the ledger: `claim_next`, heartbeat before the lease runs out, `complete` with your handoff.";

fn t0_uri(domain: &str) -> String {
    format!("memory://t0/{domain}")
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_server_info(Implementation::new("memory", env!("CARGO_PKG_VERSION")))
        .with_instructions(INSTRUCTIONS)
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        let resources = self
            .memory
            .domains()
            .into_iter()
            .map(|d| {
                let mut r = Resource::new(t0_uri(&d), format!("T0 for {d}")).with_description(
                    "The small always-loaded index: latest handoffs, standing decisions, pointers.",
                );
                r.mime_type = Some("text/markdown".into());
                r
            })
            .collect();
        Ok(ListResourcesResult::with_all_items(resources))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, McpError> {
        let Some(domain) = request.uri.strip_prefix("memory://t0/") else {
            return Err(McpError::resource_not_found(
                format!("no resource {}", request.uri),
                None,
            ));
        };
        let text = self.memory.t0(Some(domain)).await.map_err(mcp_err)?;
        Ok(ReadResourceResult::new(vec![ResourceContents::text(text, request.uri)]).into())
    }
}

/// Serve one memory over stdio until the client goes away.
///
/// # Errors
///
/// When the MCP handshake fails or the service task panics.
pub async fn serve_stdio(memory: Arc<Memory>) -> Result<(), String> {
    use rmcp::ServiceExt;
    let service = McpServer::new(memory)
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|e| format!("mcp: {e}"))?;
    service.waiting().await.map_err(|e| format!("mcp: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_has_a_description_and_a_schema() {
        let router = McpServer::tool_router();
        let tools = router.list_all();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        for expected in [
            "search",
            "get",
            "write",
            "list",
            "t0",
            "create_task",
            "admit_task",
            "claim_next",
            "claim",
            "heartbeat",
            "complete",
            "release",
            "invalidate_task",
            "get_task",
            "list_tasks",
        ] {
            assert!(
                names.contains(&expected),
                "missing tool {expected}: {names:?}"
            );
        }
        for t in &tools {
            assert!(
                t.description.as_ref().is_some_and(|d| d.len() > 20),
                "{} needs a description an agent can act on",
                t.name
            );
            assert!(
                t.input_schema.contains_key("type"),
                "{} has no schema",
                t.name
            );
        }
    }

    #[test]
    fn write_schema_names_every_field_and_requires_only_the_body() {
        let router = McpServer::tool_router();
        let write = router
            .list_all()
            .into_iter()
            .find(|t| t.name == "write")
            .unwrap();
        let props = write.input_schema["properties"].as_object().unwrap();
        for f in [
            "body",
            "entry_type",
            "title",
            "domain",
            "classification",
            "review_by",
            "supersedes",
        ] {
            assert!(props.contains_key(f), "write schema lacks {f}");
        }
        let required = write.input_schema["required"].as_array().unwrap();
        assert_eq!(required, &vec![serde_json::Value::from("body")]);
    }

    #[test]
    fn lease_limit_and_enums_are_clamped_or_refused() {
        assert_eq!(lease(None), Duration::seconds(DEFAULT_LEASE_SECONDS));
        assert_eq!(lease(Some(1)), Duration::seconds(10));
        assert_eq!(lease(Some(10_000_000)), Duration::seconds(86_400));
        assert_eq!(limit(None), DEFAULT_LIMIT);
        assert_eq!(limit(Some(0)), 1);
        assert_eq!(limit(Some(10_000)), MAX_LIMIT);
        assert!(task_status(Some("nope".into())).is_err());
        assert_eq!(task_status(None).unwrap(), dm_ledger::Status::Admitted);
        assert!(confidence(Some("sure")).is_err());
        assert_eq!(
            confidence(Some("high")).unwrap(),
            Some(dm_core::Confidence::High)
        );
        assert!(parse::<dm_core::Label>("classification", Some("secret".into())).is_err());
    }

    fn payload(r: &CallToolResult) -> Value {
        let text = r.content[0].as_text().expect("text block").text.clone();
        serde_json::from_str(&text).expect("json payload")
    }

    async fn server() -> (tempfile::TempDir, McpServer) {
        let dir = tempfile::tempdir().unwrap();
        let memory = Memory::init(dir.path(), "human:test:me", "dom-me")
            .unwrap()
            .with_manifest(vec!["tool:fs".into()]);
        (dir, McpServer::new(Arc::new(memory)))
    }

    fn handoff(body: &str) -> Parameters<WriteParams> {
        Parameters(WriteParams {
            body: body.into(),
            entry_type: Some("handoff".into()),
            ..WriteParams::default()
        })
    }

    async fn write_handoff(s: &McpServer, body: &str) -> String {
        let w = payload(&s.write(handoff(body)).await.unwrap());
        w["id"].as_str().unwrap().to_string()
    }

    async fn admitted_task(s: &McpServer, title: &str, priority: Option<i64>) -> String {
        let created = payload(
            &s.create_task(Parameters(CreateTaskParams {
                title: title.into(),
                body: "Do it.".into(),
                priority,
                ..CreateTaskParams::default()
            }))
            .await
            .unwrap(),
        );
        assert_eq!(created["task"]["status"], "proposed");
        let id = created["task"]["id"].as_str().unwrap().to_string();
        s.admit_task(Parameters(TaskIdParams { id: id.clone() }))
            .await
            .unwrap();
        id
    }

    #[tokio::test]
    async fn write_search_list_get_and_t0_round_trip() {
        let (_dir, s) = server().await;
        let w = payload(
            &s.write(Parameters(WriteParams {
                body: "# Stopped at step 3\n\nResume with the digests.\n".into(),
                entry_type: Some("handoff".into()),
                source: vec!["https://example.org/run/1".into()],
                confidence: Some("high".into()),
                classification: Some("public".into()),
                review_by: Some("2027-01-01".into()),
                ..WriteParams::default()
            }))
            .await
            .unwrap(),
        );
        let id = w["id"].as_str().unwrap().to_string();
        assert_eq!(w["domain"], "dom-me");
        assert_eq!(w["classification"], "public");

        let hits = payload(
            &s.search(Parameters(SearchParams {
                query: "digests".into(),
                ..SearchParams::default()
            }))
            .await
            .unwrap(),
        );
        assert_eq!(hits["hits"][0]["id"], id);
        assert_eq!(hits["hits"][0]["type"], "handoff");

        let listed = payload(
            &s.list(Parameters(ListParams {
                domain: Some("dom-me".into()),
                kind: Some("handoff".into()),
                limit: Some(5),
            }))
            .await
            .unwrap(),
        );
        assert_eq!(listed["entries"].as_array().unwrap().len(), 1);

        let got = payload(
            &s.get(Parameters(GetParams { id: id.clone() }))
                .await
                .unwrap(),
        );
        assert_eq!(got["title"], "Stopped at step 3");
        assert_eq!(got["review_by"], "2027-01-01");
        assert_eq!(got["author"], "human:test:me");
        assert!(got["body"].as_str().unwrap().contains("Resume"));

        let t0 = s.t0(Parameters(DomainParams::default())).await.unwrap();
        let t0 = t0.content[0].as_text().unwrap().text.clone();
        assert!(t0.contains(&id), "{t0}");
    }

    #[tokio::test]
    async fn bad_entry_input_is_refused_as_invalid_params() {
        let (_dir, s) = server().await;
        let missing = GetParams {
            id: "mem-me-none".into(),
        };
        assert!(s.get(Parameters(missing)).await.is_err());
        let bad_kind = WriteParams {
            body: "x".into(),
            entry_type: Some("rumour".into()),
            ..WriteParams::default()
        };
        assert!(s.write(Parameters(bad_kind)).await.is_err());
        let bad_date = WriteParams {
            body: "x".into(),
            review_by: Some("yesterday".into()),
            ..WriteParams::default()
        };
        assert!(s.write(Parameters(bad_date)).await.is_err());
        let bad_label = WriteParams {
            body: "x".into(),
            classification: Some("secret".into()),
            ..WriteParams::default()
        };
        assert!(s.write(Parameters(bad_label)).await.is_err());
        let bad_status = ListTasksParams {
            status: Some("done".into()),
            ..ListTasksParams::default()
        };
        assert!(s.list_tasks(Parameters(bad_status)).await.is_err());
    }

    #[tokio::test]
    async fn claim_heartbeat_complete_round_trip() {
        let (_dir, s) = server().await;
        let none = payload(
            &s.claim_next(Parameters(ClaimNextParams::default()))
                .await
                .unwrap(),
        );
        assert!(none["task"].is_null(), "nothing filed yet");
        let id = admitted_task(&s, "Write the import", Some(5)).await;

        let waiting = payload(
            &s.list_tasks(Parameters(ListTasksParams::default()))
                .await
                .unwrap(),
        );
        assert_eq!(waiting["tasks"].as_array().unwrap().len(), 1);

        let claimed = payload(
            &s.claim_next(Parameters(ClaimNextParams {
                lease_seconds: Some(600),
                ..ClaimNextParams::default()
            }))
            .await
            .unwrap(),
        );
        assert_eq!(claimed["task"]["id"], id);
        assert!(claimed["lease_until"].as_str().unwrap().ends_with('Z'));

        s.heartbeat(Parameters(LeaseParams {
            id: id.clone(),
            ..LeaseParams::default()
        }))
        .await
        .unwrap();

        let missing = CompleteParams {
            id: id.clone(),
            handoff: "mem-me-missing".into(),
        };
        assert!(s.complete(Parameters(missing)).await.is_err());
        let h = write_handoff(&s, "Done.").await;
        let done = payload(
            &s.complete(Parameters(CompleteParams {
                id: id.clone(),
                handoff: h.clone(),
            }))
            .await
            .unwrap(),
        );
        assert_eq!(done["task"]["status"], "completed");
        let got = payload(&s.get_task(Parameters(TaskIdParams { id })).await.unwrap());
        assert_eq!(got["task"]["handoff"], h);
    }

    #[tokio::test]
    async fn claim_release_and_invalidate() {
        let (_dir, s) = server().await;
        let id = admitted_task(&s, "Second", None).await;
        let c = payload(
            &s.claim(Parameters(LeaseParams {
                id: id.clone(),
                lease_seconds: Some(60),
            }))
            .await
            .unwrap(),
        );
        assert_eq!(c["task"]["holder"], "human:test:me");
        let r = payload(
            &s.release(Parameters(TaskIdParams { id: id.clone() }))
                .await
                .unwrap(),
        );
        assert_eq!(r["task"]["status"], "admitted");
        let inv = payload(
            &s.invalidate_task(Parameters(InvalidateParams {
                id,
                reason: "obsolete".into(),
                superseded_by: None,
            }))
            .await
            .unwrap(),
        );
        assert_eq!(inv["task"]["status"], "invalidated");
    }

    #[test]
    fn errors_map_to_the_right_mcp_class() {
        let invalid = mcp_err(Error::Denied("no".into()));
        let also = mcp_err(Error::NotFound("x".into()));
        let ledger = mcp_err(Error::Ledger(dm_ledger::Error::NotFound("t".into())));
        let internal = mcp_err(Error::Io {
            path: "p".into(),
            source: std::io::Error::other("disk"),
        });
        assert_eq!(invalid.code, also.code);
        assert_eq!(invalid.code, ledger.code);
        assert_ne!(invalid.code, internal.code);
        assert_eq!(t0_uri("dom-a"), "memory://t0/dom-a");
    }
}
