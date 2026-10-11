//! The MCP binding: the operations of the paper's Appendix A as tools, and T0 as a
//! resource. Every tool is a thin call into [`Memory`]; nothing lives only here.
//!
//! A harness connects over stdio. It gets `search`, `get`, `write`, `list`, `t0`, the
//! ledger operations, and a `memory://t0/<domain>` resource per granted domain. What it
//! never gets is a path on disk, a credential, or an entry its grants do not admit.

use crate::recall::{self, Budget, Depth};
use crate::{tape, Error, Memory, WriteRequest};
use dm_core::TrustTier;
use dm_index::Hit;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    ListResourcesResult, PaginatedRequestParams, ReadResourceRequestParams, ReadResourceResponse,
    ReadResourceResult, Resource, ResourceContents, ServerCapabilities, ServerConfig,
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
        Error::Denied(_)
        | Error::NotFound(_)
        | Error::Gone(_)
        | Error::Moved { .. }
        | Error::Tool(_) => McpError::invalid_params(e.to_string(), None),
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
        "review_due": h.review_due,
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
    /// summaries (default) or sections: with sections, the bodies of the best hits come
    /// back too, within a token budget, so one call replaces a search and several gets.
    #[serde(default)]
    pub depth: Option<String>,
}

/// Parameters of `recall`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct RecallParams {
    /// The question, in natural language.
    pub question: String,
    /// A granted domain. Omit for every granted domain.
    #[serde(default)]
    pub domain: Option<String>,
    /// summaries or sections: how much of each entry comes back when the synthesis is
    /// owed to you. Default: sections, or summaries once this session has fetched
    /// entries in parallel.
    #[serde(default)]
    pub depth: Option<String>,
    /// Tokens of entry bodies the pack may carry, 500 to 50000. Default 6000.
    #[serde(default)]
    pub budget_tokens: Option<usize>,
    /// Entries in the pack, 1 to 100. Default 8.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Parameters of `get`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct GetParams {
    /// The entry id, as a hit or T0 names it.
    pub id: String,
}

/// Parameters of `fetch`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct FetchParams {
    /// An https URL.
    pub url: String,
}

/// Parameters of `gofannon`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct GofannonParams {
    /// `agents` to list what is deployed, `run` to run one and wait a while for it,
    /// `start` to start one and come back later, `status` to read a run, `stop` to stop
    /// one.
    pub op: String,
    /// The deployed agent's name, for `run` and `start`.
    #[serde(default)]
    pub agent: Option<String>,
    /// The agent's input, a JSON object in its input schema, for `run` and `start`. It
    /// leaves this machine, so it carries nothing you read from memory above public.
    #[serde(default)]
    pub input: Option<Value>,
    /// The run, for `status` and `stop`.
    #[serde(default)]
    pub run_id: Option<String>,
}

/// Parameters of `web_search`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct SearchWebParams {
    /// What to search the web for. It leaves this machine, so it carries nothing you
    /// read from memory above public.
    pub query: String,
    /// How many results, at most 20.
    #[serde(default)]
    pub count: Option<u32>,
}

/// Parameters of `transcript_turn`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct TurnParams {
    /// The session, as a citation `transcript:<session>#<turn>` names it.
    pub session: String,
    /// The turn, from one.
    pub turn: usize,
}

/// Parameters of `promote`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct PromoteParams {
    /// The entry id.
    pub id: String,
    /// `curated` or `verified`.
    pub to: String,
    /// One sentence of why: what was checked, and against what.
    #[serde(default)]
    pub why: Option<String>,
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

/// One subagent task to file in a fan-out.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct FanOutItem {
    /// One line.
    pub title: String,
    /// The prompt the subagent would have been given, in Markdown.
    pub body: String,
}

/// Parameters of `fan_out`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct FanOutParams {
    /// The task the run asking for subagents holds.
    pub parent: String,
    /// The subagent tasks, one per prompt.
    pub items: Vec<FanOutItem>,
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

/// Parameters of `release`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct ReleaseParams {
    /// The task id.
    pub id: String,
    /// A handoff entry you wrote about what you did before stopping; the next claimant
    /// starts from it. Optional.
    #[serde(default)]
    pub handoff: Option<String>,
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

/// A recall's budget from its parameters: the defaults, clamped where given.
fn recall_budget(limit: Option<usize>, tokens: Option<usize>) -> Budget {
    let default = Budget::default();
    Budget {
        hits: limit.map_or(default.hits, |n| n.clamp(1, MAX_LIMIT)),
        tokens: tokens.map_or(default.tokens, |t| t.clamp(500, 50_000)),
        ..default
    }
}

/// The MCP server over one [`Memory`].
#[derive(Clone)]
pub struct McpServer {
    memory: Arc<Memory>,
    tool_router: ToolRouter<Self>,
    /// Whether the calls are the harness's, kept on the session's tape and, in a re-run,
    /// answered from the tape; the launcher's own in-process calls are neither.
    taped: bool,
}

/// The tools that only read the record, served as it is now after a re-run diverges.
const READS: &[&str] = &[
    "search",
    "get",
    "list",
    "t0",
    "recall",
    "transcript_turn",
    "task_tree",
    "get_task",
    "list_tasks",
];

/// The tools that reach outside, refused after a re-run diverges.
const OUTSIDE: &[&str] = &["fetch", "web_search", "gofannon"];

fn kind_of(tool: &str) -> tape::Kind {
    if READS.contains(&tool) {
        tape::Kind::Read
    } else if OUTSIDE.contains(&tool) {
        tape::Kind::Outside
    } else {
        tape::Kind::Mutation
    }
}

/// A recorded result, given back as it was: a result, or the refusal it was.
fn recorded(result: Value, is_error: bool) -> Result<CallToolResponse, McpError> {
    if is_error {
        if let Some(e) = result.get("error") {
            if let Ok(e) = serde_json::from_value::<McpError>(e.clone()) {
                return Err(e);
            }
        }
    }
    serde_json::from_value::<CallToolResult>(result)
        .map(CallToolResponse::from)
        .map_err(|e| McpError::internal_error(format!("the tape's result: {e}"), None))
}

/// A result as the tape keeps it, and whether it was an error.
fn to_tape(result: &Result<CallToolResponse, McpError>) -> (Value, bool) {
    match result {
        Ok(CallToolResponse::Complete(r)) => (
            serde_json::to_value(r).unwrap_or(Value::Null),
            r.is_error.unwrap_or(false),
        ),
        // input asked of the client, or a task made of the call: nothing a re-run
        // could be answered with
        Ok(_) => (json!({ "error": "not a complete result" }), true),
        Err(e) => (
            json!({ "error": serde_json::to_value(e).unwrap_or(Value::Null) }),
            true,
        ),
    }
}

#[tool_router]
impl McpServer {
    /// A server over an opened memory, for a harness: its calls are kept on the
    /// session's tape, or answered from the tape the session re-enacts.
    #[must_use]
    pub fn new(memory: Arc<Memory>) -> Self {
        Self {
            memory,
            tool_router: Self::tool_router(),
            taped: true,
        }
    }

    /// A server for the launcher's own calls in the same process, which are not the
    /// harness's and stay off its tape.
    #[must_use]
    pub fn for_launcher(memory: Arc<Memory>) -> Self {
        Self {
            taped: false,
            ..Self::new(memory)
        }
    }

    #[tool(
        description = "Ranked search over memory, summaries by default; open a hit with `get` if its summary warrants it. With depth `sections` the best hits come with their bodies, within a budget, so one call does the work of a search and several gets. Search before writing a fact that may already exist."
    )]
    async fn search(&self, p: Parameters<SearchParams>) -> Result<CallToolResult, McpError> {
        let p = p.0;
        if parse::<Depth>("depth", p.depth)? == Some(Depth::Sections) {
            let pack = self
                .memory
                .search_sections(&p.query, p.domain.as_deref(), limit(p.limit))
                .await
                .map_err(mcp_err)?;
            return ok(json!({
                "hits": pack.pieces,
                "truncated": pack.truncated,
                "tokens": pack.tokens,
            }));
        }
        let hits = self
            .memory
            .search(&p.query, p.domain.as_deref(), limit(p.limit))
            .await
            .map_err(mcp_err)?;
        ok(json!({ "hits": hits.iter().map(hit_json).collect::<Vec<_>>() }))
    }

    #[tool(
        description = "One entry in full: its frontmatter and Markdown body. Reading it raises this session's label to the entry's classification. `read_with`, when present, gives the summaries of entries usually read alongside it."
    )]
    async fn get(&self, p: Parameters<GetParams>) -> Result<CallToolResult, McpError> {
        let (e, from) = self.memory.get_following(&p.0.id).await.map_err(mcp_err)?;
        let with = self
            .memory
            .read_with(&e.front.id, 2)
            .await
            .unwrap_or_default();
        let f = &e.front;
        let mut v = json!({
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
        });
        if !from.is_empty() {
            // as a client shows the URL it was redirected to: cite this id, not the one asked
            v["redirected_from"] = json!(from);
        }
        if !with.is_empty() {
            v["read_with"] = json!(with
                .iter()
                .map(|h| json!({ "id": h.id, "title": h.title, "summary": h.summary }))
                .collect::<Vec<_>>());
        }
        ok(v)
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
        description = "Fetch a web page as text, through the memory server: every request, each redirect included, is checked against the policy, the page is scanned and kept as a source, and an answer that carries instructions is held for the owner, not served. Cite what you use as its `cite_as`, `source:<id>`. What you have read from memory limits where you may fetch: a URL carries data out."
    )]
    async fn fetch(&self, p: Parameters<FetchParams>) -> Result<CallToolResult, McpError> {
        let v = self
            .memory
            .use_tool("fetch", json!({ "url": p.0.url }))
            .await
            .map_err(mcp_err)?;
        ok(v)
    }

    #[tool(
        description = "Gofannon's deployed agents, through the memory server: list them with their input schemas (`agents`), run one on an input and wait up to half a minute for its result (`run`), or `start` one and read it later with `status`, or `stop` it. Every request is checked against the policy at your session's label, since the input leaves this machine; what comes back is scanned and kept as a source. Cite what you use as its `cite_as`."
    )]
    async fn gofannon(&self, p: Parameters<GofannonParams>) -> Result<CallToolResult, McpError> {
        let p = p.0;
        let mut args = json!({ "op": p.op });
        if let Some(a) = p.agent {
            args["agent"] = json!(a);
        }
        if let Some(i) = p.input {
            args["input"] = i;
        }
        if let Some(r) = p.run_id {
            args["run_id"] = json!(r);
        }
        let v = self
            .memory
            .use_tool("gofannon", args)
            .await
            .map_err(mcp_err)?;
        ok(v)
    }

    #[tool(
        description = "Search the web, through the memory server, as the fetch tool does: the query is checked against the policy before it leaves, the results are scanned and kept as a source. Cite what you use as its `cite_as`. Fetch a result's URL to read it."
    )]
    async fn web_search(&self, p: Parameters<SearchWebParams>) -> Result<CallToolResult, McpError> {
        let mut args = json!({ "query": p.0.query });
        if let Some(c) = p.0.count {
            args["count"] = json!(c);
        }
        let v = self
            .memory
            .use_tool("search", args)
            .await
            .map_err(mcp_err)?;
        ok(v)
    }

    #[tool(
        description = "One turn of a session's transcript, by citation: an entry's source `transcript:<session>#<turn>`. Served only when an entry you may read cites that turn; reading it raises your label as a get does. Recover from handoffs, not transcripts; this is for checking where an entry came from."
    )]
    async fn transcript_turn(&self, p: Parameters<TurnParams>) -> Result<CallToolResult, McpError> {
        let r = self
            .memory
            .transcript_turn(&p.0.session, p.0.turn)
            .await
            .map_err(mcp_err)?;
        ok(serde_json::to_value(r).unwrap_or_default())
    }

    #[tool(
        description = "Promote an entry's trust tier. `curated`: you checked, summarised or merged someone else's entry; never your own. `verified`: files a proposal a person approves; you never verify an entry yourself. Say why in one sentence."
    )]
    async fn promote(&self, p: Parameters<PromoteParams>) -> Result<CallToolResult, McpError> {
        let p = p.0;
        let to: TrustTier = parse("to", Some(p.to))?
            .ok_or_else(|| McpError::invalid_params("to: curated or verified", None))?;
        let r = self
            .memory
            .promote(&p.id, to, p.why.as_deref().unwrap_or(""))
            .await
            .map_err(mcp_err)?;
        ok(serde_json::to_value(r).unwrap_or_default())
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
        description = "Delegated recall, one turn either way: a question in; a cited brief out when this server has a model behind it, else the pack the brief would be written from, with the synthesis owed to you in this turn. `synthesis.by` says which. Cheaper than a search and several gets when the question spans entries."
    )]
    async fn recall(&self, p: Parameters<RecallParams>) -> Result<CallToolResult, McpError> {
        let p = p.0;
        let depth = parse::<Depth>("depth", p.depth)?;
        let budget = recall_budget(p.limit, p.budget_tokens);
        let r = self
            .memory
            .recall(&p.question, p.domain.as_deref(), depth, budget)
            .await
            .map_err(mcp_err)?;
        ok(recall::to_json(&r))
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
        description = "Fan a held task out: file one task per item as its subagents' work, and a continuation that resumes the parent when they are done, with their handoffs in its prompt. Filing the same prompt twice is one task."
    )]
    async fn fan_out(&self, p: Parameters<FanOutParams>) -> Result<CallToolResult, McpError> {
        let p = p.0;
        let items: Vec<(String, String)> = p.items.into_iter().map(|i| (i.title, i.body)).collect();
        let f = self
            .memory
            .fan_out(&p.parent, &items)
            .await
            .map_err(mcp_err)?;
        ok(json!({ "children": f.children, "continuation": f.continuation }))
    }

    #[tool(
        description = "A task with everything filed from it, recursively: its subagent tasks, continuations and retries."
    )]
    async fn task_tree(&self, p: Parameters<TaskIdParams>) -> Result<CallToolResult, McpError> {
        let t = self.memory.task_tree(&p.0.id).await.map_err(mcp_err)?;
        ok(json!(t))
    }

    #[tool(
        description = "File a task again from one that dead-ended, with the handoffs of its chain attached as lessons."
    )]
    async fn retry_task(&self, p: Parameters<TaskIdParams>) -> Result<CallToolResult, McpError> {
        let t = self.memory.retry_task(&p.0.id).await.map_err(mcp_err)?;
        ok(json!({ "task": t }))
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

    #[tool(
        description = "Give a task back to the pool without completing it. Pass the id of a handoff you wrote so the next claimant starts where you stopped."
    )]
    async fn release(&self, p: Parameters<ReleaseParams>) -> Result<CallToolResult, McpError> {
        let p = p.0;
        let t = self
            .memory
            .release(&p.id, p.handoff.as_deref())
            .await
            .map_err(mcp_err)?;
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
    /// Every call goes through the router, as the macro would route it; a harness's call
    /// is kept on the session's tape, and in a re-run answered from the tape first.
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        if !self.taped {
            let tcc = ToolCallContext::new(self, request, context);
            return self.tool_router.call(tcc).await;
        }
        let tool = request.name.to_string();
        let arguments = Value::Object(request.arguments.clone().unwrap_or_default());
        if let Some(play) = self.memory.playback() {
            // a tool this server does not have is the router's to refuse, as it was
            let kind = if self.tool_router.get(&tool).is_some() {
                kind_of(&tool)
            } else {
                tape::Kind::Read
            };
            let memory = &self.memory;
            match play.answer_within(&tool, &arguments, kind, &|l| memory.handles(l)) {
                tape::Answer::Recorded {
                    result,
                    is_error,
                    label,
                } => {
                    self.memory.observe_recorded(label);
                    return recorded(result, is_error);
                }
                tape::Answer::Live => {}
                tape::Answer::Captured => {
                    return Ok(CallToolResult::success(vec![ContentBlock::json(json!({
                        "reenactment": "captured, not applied: this call is not on the tape",
                    }))?])
                    .into());
                }
                tape::Answer::Refused(why) => return Err(McpError::invalid_params(why, None)),
            }
            let tcc = ToolCallContext::new(self, request, context);
            return self.tool_router.call(tcc).await;
        }
        let tcc = ToolCallContext::new(self, request, context);
        let result = self.tool_router.call(tcc).await;
        let (value, is_error) = to_tape(&result);
        if let Err(e) = self
            .memory
            .tape_call(&tool, &arguments, &value, is_error)
            .await
        {
            eprintln!("memory: {tool} was not kept on the tape: {e}");
        }
        result
    }

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
    serve_pipe(memory, tokio::io::stdin(), tokio::io::stdout()).await
}

/// Serve over any byte pair until the peer closes: standard input and output, or one end
/// of an in-process pipe for a client in the same process.
///
/// # Errors
///
/// When the MCP handshake fails or the service task panics.
pub async fn serve_pipe<R, W>(memory: Arc<Memory>, reader: R, writer: W) -> Result<(), String>
where
    R: tokio::io::AsyncRead + Send + Unpin + 'static,
    W: tokio::io::AsyncWrite + Send + Unpin + 'static,
{
    serve_with(McpServer::new(memory), reader, writer).await
}

/// Serve the launcher's own client over a byte pair: its calls stay off the harness's
/// tape.
///
/// # Errors
///
/// When the MCP handshake fails or the service task panics.
pub async fn serve_launcher_pipe<R, W>(
    memory: Arc<Memory>,
    reader: R,
    writer: W,
) -> Result<(), String>
where
    R: tokio::io::AsyncRead + Send + Unpin + 'static,
    W: tokio::io::AsyncWrite + Send + Unpin + 'static,
{
    serve_with(McpServer::for_launcher(memory), reader, writer).await
}

async fn serve_with<R, W>(server: McpServer, reader: R, writer: W) -> Result<(), String>
where
    R: tokio::io::AsyncRead + Send + Unpin + 'static,
    W: tokio::io::AsyncWrite + Send + Unpin + 'static,
{
    use rmcp::ServiceExt;
    let service = server
        .serve((reader, writer))
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
            "recall",
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
            "fan_out",
            "task_tree",
            "retry_task",
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

    #[tokio::test]
    async fn search_at_section_depth_carries_bodies_and_depth_is_checked() {
        let (_dir, s) = server().await;
        s.write(handoff(
            "# Digest handoff\n\nThe manifest parses. Next: digests.\n",
        ))
        .await
        .unwrap();
        let deep = payload(
            &s.search(Parameters(SearchParams {
                query: "digests".into(),
                depth: Some("sections".into()),
                ..SearchParams::default()
            }))
            .await
            .unwrap(),
        );
        assert!(deep["hits"][0]["body"]
            .as_str()
            .is_some_and(|b| b.contains("Next: digests")));
        assert_eq!(deep["truncated"], false);
        let flat = payload(
            &s.search(Parameters(SearchParams {
                query: "digests".into(),
                depth: Some("summaries".into()),
                ..SearchParams::default()
            }))
            .await
            .unwrap(),
        );
        assert!(flat["hits"][0].get("body").is_none());
        assert!(s
            .search(Parameters(SearchParams {
                query: "digests".into(),
                depth: Some("deep".into()),
                ..SearchParams::default()
            }))
            .await
            .is_err());
        let b = recall_budget(Some(10_000), Some(10_000_000));
        assert_eq!(b.hits, MAX_LIMIT);
        assert_eq!(b.tokens, 50_000);
        assert_eq!(recall_budget(None, Some(1)).tokens, 500);
        assert_eq!(recall_budget(None, None), Budget::default());
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

        // an id from a list a moment ago, replaced since: the reader is redirected
        let w2 = payload(
            &s.write(Parameters(WriteParams {
                body: "# Stopped at step 4\n\nResume with the manifest.\n".into(),
                entry_type: Some("handoff".into()),
                supersedes: Some(id.clone()),
                ..WriteParams::default()
            }))
            .await
            .unwrap(),
        );
        let moved = payload(
            &s.get(Parameters(GetParams { id: id.clone() }))
                .await
                .unwrap(),
        );
        assert_eq!(moved["id"], w2["id"], "the successor is read");
        assert_eq!(moved["redirected_from"], json!([id]));
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
            &s.release(Parameters(ReleaseParams {
                id: id.clone(),
                handoff: None,
            }))
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
