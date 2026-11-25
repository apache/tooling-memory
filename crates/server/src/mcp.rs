//! The MCP binding: the operations of the paper's Appendix A as tools, and T0 as a
//! resource. Every tool is a thin call into [`Memory`]; nothing lives only here.
//!
//! A harness connects over stdio. It gets `search`, `get`, `write`, `list`, `t0`, the
//! ledger operations, and a `memory://t0/<domain>` resource per granted domain. What it
//! never gets is a path on disk, a credential, or an entry its grants do not admit.

use crate::duties::{CheckOptions, DemoteReason, DEAD_WINDOW_DAYS};
use crate::lineage::Direction;
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

mod timescales;

const DEFAULT_LEASE_SECONDS: i64 = 30 * 60;
const DEFAULT_LIMIT: usize = 10;
const MAX_LIMIT: usize = 100;
/// The entries that `test_together` searches from when the caller names no number.
const DEFAULT_SAMPLE: usize = 8;

fn mcp_err(e: Error) -> McpError {
    match e {
        Error::Denied(_)
        | Error::Stale(_)
        | Error::Outage(_)
        | Error::NotFound(_)
        | Error::Gone(_)
        | Error::Moved { .. }
        | Error::Tool(_) => McpError::invalid_params(e.to_string(), None),
        Error::Ledger(
            dm_ledger::Error::NotFound(_)
            | dm_ledger::Error::WrongState { .. }
            | dm_ledger::Error::NotHolder { .. }
            | dm_ledger::Error::Fenced { .. }
            | dm_ledger::Error::AttemptSpent { .. },
        ) => McpError::invalid_params(e.to_string(), None),
        other => McpError::internal_error(other.to_string(), None),
    }
}

/// The most bytes a reply may take as one line of the transport. The transport drops a
/// longer line and answers nothing, and the caller waits; under this, with room for the
/// envelope, a reply that is too long is refused in words.
pub const MAX_REPLY_BYTES: usize = 12 * 1024 * 1024;

/// A reply as it was, or a refusal that says how long it was. The length is the reply's
/// own serialised form: its text is a JSON string inside JSON, so a quote costs more
/// bytes on the line than in the entry.
fn bounded(reply: Result<CallToolResponse, McpError>) -> Result<CallToolResponse, McpError> {
    let reply = match reply {
        Ok(reply) => reply,
        // a refusal that quotes what the caller sent is as long as what the caller sent
        Err(e) => return Err(short_refusal(e)),
    };
    // only a completed call carries content; the other kinds are a few fields
    let CallToolResponse::Complete(result) = &reply else {
        return Ok(reply);
    };
    let bytes = serde_json::to_vec(result).map_or(usize::MAX, |v| v.len());
    if bytes > MAX_REPLY_BYTES {
        return Err(McpError::invalid_params(
            format!(
                "the reply has {bytes} bytes and a reply holds at most {MAX_REPLY_BYTES}: \
                 ask for less, with a smaller limit or one entry at a time"
            ),
            None,
        ));
    }
    Ok(reply)
}

/// The most bytes a refusal may take: room for a sentence and the names in it.
const MAX_REFUSAL_BYTES: usize = 64 * 1024;

/// A refusal as it was, or its first words when it is longer than a refusal may be.
fn short_refusal(e: McpError) -> McpError {
    let bytes = serde_json::to_vec(&e).map_or(usize::MAX, |v| v.len());
    if bytes <= MAX_REFUSAL_BYTES {
        return e;
    }
    let began: String = e.message.chars().take(200).collect();
    McpError::invalid_params(
        format!(
            "refused, with a reason of {bytes} bytes, which is too long to send; it began: {began}"
        ),
        None,
    )
}

fn ok(value: Value) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![ContentBlock::json(value)?]))
}

/// A granted claim as a tool returns it: the task, and the claim's generation and expiry.
fn claim_json(c: &dm_ledger::Claim) -> Value {
    json!({
        "task": c.task,
        "generation": c.generation,
        "lease_until": rfc3339(c.lease_until),
    })
}

fn rfc3339(t: time::OffsetDateTime) -> String {
    t.format(&Rfc3339).unwrap_or_default()
}

fn hit_json(h: &Hit) -> Value {
    let mut v = json!({
        "id": h.id,
        "domain": h.domain,
        "type": h.entry_type.to_string(),
        "trust_tier": h.trust_tier.to_string(),
        "classification": h.classification.to_string(),
        "title": h.title,
        "summary": h.summary,
        "review_due": h.review_due,
    });
    put_reach(&mut v, h.reach, None);
    v
}

/// Put the reach of an entry into an answer when it is not the fleet's, and the task
/// that the entry names when it names one. An answer about an entry with fleet reach
/// and no task has neither key.
fn put_reach(v: &mut Value, reach: dm_core::Reach, task: Option<&str>) {
    if !reach.is_fleet() {
        v["reach"] = json!(reach.to_string());
    }
    if let Some(task) = task {
        v["task"] = json!(task);
    }
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

/// Parameters of `outside_readers`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct OutsideReadersParams {
    /// The domain; the only granted one when omitted.
    #[serde(default)]
    pub domain: Option<String>,
}

/// Parameters of `lineage`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct LineageParams {
    /// An entry id. With `back`, also `<domain>@<seq>`: a send event of that domain's log.
    pub of: String,
    /// true for what it may have been made from. Default false: what may have been made
    /// from it, and where that went.
    #[serde(default)]
    pub back: Option<bool>,
    /// The most steps to follow. Default: until a step reaches nothing new.
    #[serde(default)]
    pub depth: Option<u32>,
}

/// Parameters of `log`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct LogParams {
    /// A granted domain; the only one when omitted.
    #[serde(default)]
    pub domain: Option<String>,
    /// The first sequence number wanted. Default 0: the whole log.
    #[serde(default)]
    pub from: Option<u64>,
}

/// Parameters of `fetch`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct FetchParams {
    /// An https URL.
    pub url: String,
    /// `true` to have the owner's browser open the page and read it as its scripts
    /// built it, for a page that is empty without them. Only a fetch the browser runs
    /// can; leave it out otherwise.
    #[serde(default)]
    pub render: Option<bool>,
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
    /// One sentence of why: what was checked, and against what. Required: the ledger
    /// keeps it beside the entry.
    pub why: String,
}

/// Parameters of `write`. A parameter this does not have is refused: dropped without a
/// word, a caller's `source` of an older format would lose the entry its citations.
#[derive(Deserialize, schemars::JsonSchema, Default)]
#[serde(deny_unknown_fields)]
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
    pub cites: Vec<String>,
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
    /// The person who answers for the entry, as `human:...`. A convention and a method
    /// name one.
    #[serde(default)]
    pub owner: Option<String>,
}

/// Parameters of `demote`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct DemoteParams {
    /// The entry id.
    pub id: String,
    /// `dead` or `wrong`. An agent gives `dead`.
    pub reason: String,
}

/// Parameters of `dead`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct DeadParams {
    /// A granted domain. Omit for every granted domain.
    #[serde(default)]
    pub domain: Option<String>,
    /// The window in days. Default 90, the window of a demotion by an agent.
    #[serde(default)]
    pub days: Option<i64>,
}

/// Parameters of a survey that can file what it finds: `check_conventions`,
/// `corrections` and `policy_curation`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct SurveyParams {
    /// The domain; the only granted domain when omitted.
    #[serde(default)]
    pub domain: Option<String>,
    /// File one task for each finding. A finding that has a task gets no second task.
    #[serde(default)]
    pub file: Option<bool>,
}

/// Parameters of `record_outcome`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct RecordOutcomeParams {
    /// The task id.
    pub task: String,
    /// accepted, corrected, rejected or reverted.
    pub kind: String,
    /// What was wrong, or what was changed. At most 2000 characters.
    #[serde(default)]
    pub note: Option<String>,
}

/// Parameters of `outcomes`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct OutcomesParams {
    /// The task id.
    pub task: String,
}

/// Parameters of `auditor_mode`: none.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct NoParams {}

/// Parameters of `test_together`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct TogetherParams {
    /// The domain; the only granted domain when omitted.
    #[serde(default)]
    pub domain: Option<String>,
    /// How many of the newest entries to search from. Default 8.
    #[serde(default)]
    pub sample: Option<usize>,
    /// File one task for a person for each pair found.
    #[serde(default)]
    pub file: Option<bool>,
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
    /// The task definition this kind of work runs under, by name. The task takes the
    /// definition's acceptance criteria and requires the tools it declares.
    #[serde(default)]
    pub definition: Option<String>,
    /// Checks that say this task is done, in place of its definition's.
    #[serde(default)]
    pub acceptance: Vec<String>,
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
    /// A key of your own for this attempt. If you did not get the answer, call again
    /// with the same key: you get the same claim, not a second task.
    #[serde(default)]
    pub attempt: Option<String>,
}

/// Parameters of `heartbeat`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct HeartbeatParams {
    /// The task id.
    pub id: String,
    /// The generation of your claim, as `claim_next` or `claim` returned it. Optional in
    /// the session that made the claim, which remembers it.
    #[serde(default)]
    pub generation: Option<u64>,
    /// Lease length in seconds, 10 to 86400. Default 1800.
    #[serde(default)]
    pub lease_seconds: Option<i64>,
}

/// Parameters of `late_handoff`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct LateHandoffParams {
    /// The task id.
    pub id: String,
    /// The generation of the claim you held. Optional in the session that made it.
    #[serde(default)]
    pub generation: Option<u64>,
    /// The id of the handoff entry you wrote with `write`.
    pub handoff: String,
}

/// Parameters of `check_policy`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct CheckPolicyParams {
    /// What you are about to do, in a few words.
    pub action: String,
    /// The domain; the only granted domain when omitted.
    #[serde(default)]
    pub domain: Option<String>,
}

/// Parameters of `file_exception`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct FileExceptionParams {
    /// The policy id of the rule to breach.
    pub breaches: String,
    /// The one task the exception holds for. Optional in a session that runs on a task.
    #[serde(default)]
    pub task: Option<String>,
    /// What the exception covers, in words.
    pub scope: String,
    /// When it lapses, RFC 3339.
    pub expiry: String,
    /// Why it is needed.
    pub justification: String,
}

/// Parameters of `propose_amendment`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct ProposeAmendmentParams {
    /// The policy id of the rule.
    pub policy_id: String,
    /// What happened, and the ids of the entries that show it.
    pub evidence: String,
}

/// Parameters of `claim`.
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
    /// The generation of your claim. Optional in the session that made the claim.
    #[serde(default)]
    pub generation: Option<u64>,
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
    /// The generation of your claim. Optional in the session that made the claim.
    #[serde(default)]
    pub generation: Option<u64>,
    /// The id of the handoff entry you wrote with `write` describing what happened.
    pub handoff: String,
    /// A key of your own for this attempt. If you did not get the answer, call again
    /// with the same key: the task is completed one time.
    #[serde(default)]
    pub attempt: Option<String>,
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
    /// proposed, open, claimed, blocked, done, or invalidated. Default open.
    #[serde(default)]
    pub status: Option<String>,
}

fn task_status(s: Option<String>) -> Result<dm_ledger::Status, McpError> {
    Ok(match s.as_deref().unwrap_or("open") {
        "proposed" => dm_ledger::Status::Proposed,
        "open" => dm_ledger::Status::Open,
        "claimed" => dm_ledger::Status::Claimed,
        "done" => dm_ledger::Status::Done,
        "blocked" => dm_ledger::Status::Blocked,
        "invalidated" => dm_ledger::Status::Invalidated,
        other => {
            return Err(McpError::invalid_params(
                format!(
                    "status {other:?} is not one of proposed, open, claimed, blocked, done, invalidated"
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
    "promotions",
    "task_tree",
    "get_task",
    "list_tasks",
    "lineage",
    "log",
    "outside_readers",
    "dead",
    "outcomes",
    "auditor_mode",
    "curation_case",
    "resolve_identity",
    "proposals",
    "outage_status",
];

/// The tools that reach outside, refused after a re-run diverges.
const OUTSIDE: &[&str] = &["fetch", "web_search", "gofannon"];

/// What a call does: a read, a reach outside, or a change. `registry_check` reads, and
/// with `file` it files tasks, so its arguments decide.
fn kind_of(tool: &str, arguments: &Value) -> tape::Kind {
    let files = arguments.get("file").and_then(Value::as_bool) == Some(true);
    if READS.contains(&tool) || (tool == "registry_check" && !files) {
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
            tool_router: Self::routes(),
            taped: true,
        }
    }

    /// Each tool of the server: the tools of this file, and the tools for the long
    /// timescales.
    fn routes() -> ToolRouter<Self> {
        Self::tool_router() + Self::timescales_router()
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
            "cites": f.cites,
            "supersedes": f.supersedes,
            "review_by": f.review_by.map(|d| d.to_string()),
            "title": e.title(),
            "body": e.body,
        });
        put_reach(&mut v, f.reach, f.task.as_deref());
        if let Some(owner) = &f.owner {
            v["owner"] = json!(owner);
        }
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
        description = "Write an entry. It lands at raw trust under this session's author. Write a `handoff` when you stop so the next session continues from it, not from your transcript; write a `decision` with context, options, choice and rationale; write a `fact` with a review_by date. An answer with `withheld` names something this session read that was erased since: the entry is kept, and no read gives it until the owner releases it. An answer with `reach: tap` means that this session read content from outside the fleet: only the task chain, triage and curators read the entry until it is curated. `task` names the task that this session holds."
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
            cites: p.cites,
            confidence: confidence(p.confidence.as_deref())?,
            classification: parse("classification", p.classification)?,
            supersedes: p.supersedes,
            review_by,
            summary: p.summary,
            owner: p.owner,
        };
        let w = self.memory.write(req).await.map_err(mcp_err)?;
        let mut v = json!({
            "id": w.id,
            "domain": w.domain,
            "classification": w.classification.to_string(),
        });
        // the entry landed withheld: this session read something that is erased now
        if let Some(erased) = w.withheld {
            v["withheld"] = json!(erased);
        }
        put_reach(&mut v, w.reach, w.task.as_deref());
        ok(v)
    }

    #[tool(
        description = "Fetch a web page as text, through the memory server: every request, each redirect included, is checked against the policy, the page is scanned and kept as a source, and an answer that carries instructions is held for the owner, not served. Cite what you use as its `cite_as`, `source:<id>`. What you have read from memory limits where you may fetch: a URL carries data out."
    )]
    async fn fetch(&self, p: Parameters<FetchParams>) -> Result<CallToolResult, McpError> {
        let mut args = json!({ "url": p.0.url });
        if p.0.render == Some(true) {
            args["render"] = json!(true);
        }
        let v = self.memory.use_tool("fetch", args).await.map_err(mcp_err)?;
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
        description = "One turn of a session's transcript, by citation: an entry's `cites` `transcript:<session>#<turn>`. Served only when an entry you may read cites that turn; reading it raises your label as a get does. Recover from handoffs, not transcripts; this is for checking where an entry came from."
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
            .promote(&p.id, to, &p.why)
            .await
            .map_err(mcp_err)?;
        ok(serde_json::to_value(r).unwrap_or_default())
    }

    #[tool(
        description = "Demote an entry behind a tombstone: retrieval no longer serves it, `get` says that it was demoted, and its text stays in the archive. `reason` is `dead` or `wrong`, a code and never a text. As an agent you demote only while you hold a task of a curator or of the auditor, only for `dead`, and only an entry that the `dead` tool lists. A person judges that an entry is wrong, and only a person restores."
    )]
    async fn demote(&self, p: Parameters<DemoteParams>) -> Result<CallToolResult, McpError> {
        let p = p.0;
        let reason = DemoteReason::parse(&p.reason)
            .ok_or_else(|| McpError::invalid_params("reason: dead or wrong", None))?;
        let d = self.memory.demote(&p.id, reason).await.map_err(mcp_err)?;
        ok(json!(d))
    }

    #[tool(
        description = "Restore a demoted entry: retrieval serves it again. Only a person does this."
    )]
    async fn restore(&self, p: Parameters<GetParams>) -> Result<CallToolResult, McpError> {
        self.memory.restore(&p.0.id).await.map_err(mcp_err)?;
        ok(json!({ "restored": p.0.id }))
    }

    #[tool(
        description = "The dead entries that you read: older than the window, not read inside it, named by no active entry, not verified, and not a handoff, a policy entry or a canary. Each is a candidate for `demote` with the reason `dead`. The list is not a read of the entries, but it raises your label to their classification."
    )]
    async fn dead(&self, p: Parameters<DeadParams>) -> Result<CallToolResult, McpError> {
        let p = p.0;
        let days = p.days.unwrap_or(DEAD_WINDOW_DAYS);
        let dead = self
            .memory
            .dead(p.domain.as_deref(), days)
            .await
            .map_err(mcp_err)?;
        ok(json!({ "dead": dead }))
    }

    #[tool(
        description = "The convention checks over the entries of a domain: freshness (a fact has a review date, and no entry is more than 30 days past its review date), ownership (a convention and a method name an owner), cross-link (each entry named is one that the record serves) and superseded (no citation of a replaced entry). Each failure names the entry and the person who answers for it. With `file`, each failure becomes one task: for a person, or for a curator when the entry cites a replaced entry."
    )]
    async fn check_conventions(
        &self,
        p: Parameters<SurveyParams>,
    ) -> Result<CallToolResult, McpError> {
        let p = p.0;
        let m = &self.memory;
        let domain = p.domain.as_deref();
        let today = time::OffsetDateTime::now_utc().date();
        let failures = m
            .check_conventions(domain, today, &CheckOptions::default())
            .await
            .map_err(mcp_err)?;
        let filed = if p.file.unwrap_or(false) {
            m.file_failures(domain, &failures).await.map_err(mcp_err)?
        } else {
            (Vec::new(), 0)
        };
        ok(json!({ "failures": failures, "filed": filed.0, "already_filed": filed.1 }))
    }

    #[tool(
        description = "Record what became of the result of a task: `accepted`, `corrected`, `rejected` or `reverted`, with a note. Only a person records an outcome. A result that was corrected, rejected or reverted is a correction, and the `corrections` tool gives it to a curator."
    )]
    async fn record_outcome(
        &self,
        p: Parameters<RecordOutcomeParams>,
    ) -> Result<CallToolResult, McpError> {
        let p = p.0;
        let kind = dm_ledger::OutcomeKind::parse(&p.kind).ok_or_else(|| {
            McpError::invalid_params("kind: accepted, corrected, rejected or reverted", None)
        })?;
        let said = self
            .memory
            .record_outcome(&p.task, kind, p.note.as_deref().unwrap_or(""))
            .await
            .map_err(mcp_err)?;
        ok(json!({ "outcomes": said }))
    }

    #[tool(
        description = "The outcome events of a task, oldest first: what became of its result, who said so, and the note."
    )]
    async fn outcomes(&self, p: Parameters<OutcomesParams>) -> Result<CallToolResult, McpError> {
        let said = self.memory.outcomes(&p.0.task).await.map_err(mcp_err)?;
        ok(json!({ "outcomes": said }))
    }

    #[tool(
        description = "The corrections of a domain: each task whose result a person corrected, rejected or reverted, with its outcome events. `disputed` is true when one person accepted the result and another person did not. With `file`, each correction becomes one task: for a curator, with the question whether the method or the knowledge was at fault, or for a person when the correction is disputed."
    )]
    async fn corrections(&self, p: Parameters<SurveyParams>) -> Result<CallToolResult, McpError> {
        let p = p.0;
        let m = &self.memory;
        let domain = p.domain.as_deref();
        let found = m.corrections(domain).await.map_err(mcp_err)?;
        let filed = if p.file.unwrap_or(false) {
            m.file_corrections(domain, &found).await.map_err(mcp_err)?
        } else {
            (Vec::new(), 0)
        };
        ok(json!({ "corrections": found, "filed": filed.0, "already_filed": filed.1 }))
    }

    #[tool(
        description = "What a curator finds in the policy and in the done work of a domain: the pairs of rules in force in one layer whose sentences overlap, and the work that was done three times or more with no task definition. With `file`, each pair becomes a proposal to the owners of the two rules to merge them, and each repeated work becomes a task to write a task definition."
    )]
    async fn policy_curation(
        &self,
        p: Parameters<SurveyParams>,
    ) -> Result<CallToolResult, McpError> {
        let p = p.0;
        let m = &self.memory;
        let domain = p.domain.as_deref();
        let found = m.policy_curation(domain).await.map_err(mcp_err)?;
        let filed = if p.file.unwrap_or(false) {
            m.file_policy_curation(domain, &found)
                .await
                .map_err(mcp_err)?
        } else {
            (Vec::new(), 0)
        };
        ok(json!({
            "overlaps": found.overlaps, "repeated": found.repeated,
            "filed": filed.0, "already_filed": filed.1,
        }))
    }

    #[tool(
        description = "How the auditor runs: `other-model` when a model that is not the model of the curators qualified on the planted faults, else `fresh-session`. `qualified` says whether a qualification is on file, and `human_sample` how many verdicts a person reviews in each cycle."
    )]
    async fn auditor_mode(&self, _p: Parameters<NoParams>) -> Result<CallToolResult, McpError> {
        ok(json!(self.memory.auditor_mode().map_err(mcp_err)?))
    }

    #[tool(
        description = "One curated entry as a judge reads it: the entry, the version that it replaced and the entries that it cites, in `text`. The handoff of the curator and the reason of a promotion are never given, so judge the entry on its sources. `unsupported` is true when it cites no entry that you can read. Each entry given is a read of this session."
    )]
    async fn curation_case(&self, p: Parameters<GetParams>) -> Result<CallToolResult, McpError> {
        let case = self.memory.curation_case(&p.0.id).await.map_err(mcp_err)?;
        ok(case.to_json())
    }

    #[tool(
        description = "Test entries together: for each of the newest entries of a domain, the scan of the write gate over each ordered pair of the entries that one search returns with it. A pair is reported when the two bodies together carry a finding that neither carries alone. The report names the ids and the kinds of finding, and never text. With `file`, each pair becomes one task for a person."
    )]
    async fn test_together(
        &self,
        p: Parameters<TogetherParams>,
    ) -> Result<CallToolResult, McpError> {
        let p = p.0;
        let m = &self.memory;
        let domain = p.domain.as_deref();
        let found = m
            .test_together(domain, p.sample.unwrap_or(DEFAULT_SAMPLE))
            .await
            .map_err(mcp_err)?;
        let filed = if p.file.unwrap_or(false) {
            m.file_together(domain, &found).await.map_err(mcp_err)?
        } else {
            Vec::new()
        };
        ok(json!({ "together": found, "filed": filed }))
    }

    #[tool(
        description = "Why each promotion of an entry was made, oldest first: who promoted which version to which tier, the reason they gave, and whether that version reached the record (`landed`). A proposal to verified is listed from when it is made, with `landed` false until a person approves it. As an auditor, read it after you have judged the change yourself, not before, so the reasons do not lead your judgement. Only for an entry you may read, replaced ones included; reading it raises your label as a get does."
    )]
    async fn promotions(&self, p: Parameters<GetParams>) -> Result<CallToolResult, McpError> {
        let r = self.memory.promotions(&p.0.id).await.map_err(mcp_err)?;
        ok(json!({ "promotions": r }))
    }

    #[tool(
        description = "For the owner and the security role; refused to any other author. The lineage of an entry, from the event log of each granted domain. Forward, the default: the entries and the sends that came after a session read it, and the entries that cite it. With `back`: the entries its session had read before it wrote the entry, and the entries it cites; `of` may then be `<domain>@<seq>`, a send event. Each entry comes with its `hops` and whether it was `informed` (a read came before the write), `cited`, or both. `complete` is false when the log could not say. Reading it raises your label as reading the log does."
    )]
    async fn lineage(&self, p: Parameters<LineageParams>) -> Result<CallToolResult, McpError> {
        let p = p.0;
        let direction = if p.back.unwrap_or(false) {
            Direction::Back
        } else {
            Direction::Forward
        };
        let found = self
            .memory
            .lineage(&p.of, direction, p.depth)
            .await
            .map_err(mcp_err)?;
        ok(json!(found))
    }

    #[tool(
        description = "For the owner and the security role; refused to any other author. The sessions that read content from authors outside the fleet, from the event log of a domain: each with its author, the sequence number of its first event after that read (`since`), the entries it wrote after it (`writes`) and the sends it made after it (`sends`). An auditor checks those sends first. Reading it raises your label as reading the log does."
    )]
    async fn outside_readers(
        &self,
        p: Parameters<OutsideReadersParams>,
    ) -> Result<CallToolResult, McpError> {
        let readers = self
            .memory
            .outside_readers(p.0.domain.as_deref())
            .await
            .map_err(mcp_err)?;
        ok(json!({ "readers": readers }))
    }

    #[tool(
        description = "For the owner and the security role; refused to any other author. The event log of a domain from sequence number `from`: each write offered, each version put, each write refused, each erasure, each read, each send and each event of the ledger, by id and digest and never a body, within what you may handle. `freshness` says how far the index is behind the log, against the published bound of 60 seconds. Reading it raises your label to the highest classification among the events served."
    )]
    async fn log(&self, p: Parameters<LogParams>) -> Result<CallToolResult, McpError> {
        let p = p.0;
        let domain = p.domain.as_deref();
        let events = self
            .memory
            .log_synced(domain, p.from.unwrap_or(0))
            .await
            .map_err(mcp_err)?;
        let freshness = self.memory.freshness(domain).map_err(mcp_err)?;
        ok(json!({ "events": events, "freshness": freshness }))
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
            definition: p.definition,
            acceptance: p.acceptance,
            // the library sets it, from what the session read
            outside: false,
        };
        let c = self.memory.create_task(spec).await.map_err(mcp_err)?;
        // what a task that stays proposed lacks goes back to whoever filed it
        let gap = (c.task.status == dm_ledger::Status::Proposed)
            .then(|| self.memory.admission_gap(&c.task.spec))
            .flatten();
        ok(json!({
            "task": c.task,
            "deduplicated": c.deduplicated,
            "not_admitted_because": gap,
        }))
    }

    #[tool(
        description = "Fan a held task out: file one task per item as its subagents' work, and a continuation that resumes the parent when they are done, with their handoffs in its prompt. Filing the same prompt twice is one task: an item asked for before in this chain comes back in `existing`, and `withheld` counts the items held back because the chain holds them in work this session may not read."
    )]
    async fn fan_out(&self, p: Parameters<FanOutParams>) -> Result<CallToolResult, McpError> {
        let p = p.0;
        let items: Vec<(String, String)> = p.items.into_iter().map(|i| (i.title, i.body)).collect();
        let f = self
            .memory
            .fan_out(&p.parent, &items)
            .await
            .map_err(mcp_err)?;
        ok(json!({
            "children": f.children, "existing": f.existing, "withheld": f.withheld,
            "continuation": f.continuation,
        }))
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
        description = "Claim the highest-ranked task this session can take, under a lease. Heartbeat before the lease runs out; a lease nobody renews is lost at expiry and the task returns to the pool. The claim has a generation: this session passes it for you with heartbeat, complete and release, and a session whose claim is over is refused by it. Returns no task when nothing is claimable."
    )]
    async fn claim_next(&self, p: Parameters<ClaimNextParams>) -> Result<CallToolResult, McpError> {
        let p = p.0;
        let c = self
            .memory
            .claim_next_once(
                p.domain.as_deref(),
                lease(p.lease_seconds),
                p.attempt.as_deref(),
            )
            .await
            .map_err(mcp_err)?;
        match c {
            None => ok(json!({ "task": null })),
            Some(c) => ok(claim_json(&c)),
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
        ok(claim_json(&c))
    }

    #[tool(
        description = "Renew your lease on a task. Refused if the lease already expired, or the task was claimed again since: stop work, and leave what you learned with late_handoff. The returned task shows whether it was invalidated meanwhile."
    )]
    async fn heartbeat(&self, p: Parameters<HeartbeatParams>) -> Result<CallToolResult, McpError> {
        let p = p.0;
        let t = self
            .memory
            .heartbeat(&p.id, p.generation, lease(p.lease_seconds))
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
            .complete_once(&p.id, p.generation, &p.handoff, p.attempt.as_deref())
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
            .release(&p.id, p.generation, p.handoff.as_deref())
            .await
            .map_err(mcp_err)?;
        ok(json!({ "task": t }))
    }

    #[tool(
        description = "Leave a handoff on a task after your claim on it is over: the lease ran out, or the task was claimed again. Write the handoff first with `write`. It changes nothing of the task; the next holder reads it with get_task."
    )]
    async fn late_handoff(
        &self,
        p: Parameters<LateHandoffParams>,
    ) -> Result<CallToolResult, McpError> {
        let p = p.0;
        let t = self
            .memory
            .handoff_after(&p.id, p.generation, &p.handoff)
            .await
            .map_err(mcp_err)?;
        ok(json!({ "task": t }))
    }

    #[tool(
        description = "The rules in force for what you are about to do: each with its id, layer, rule, intent and whether it is enforced or advisory. Call it before an action that cannot be taken back. A rule above what you may read is named, and its text is withheld."
    )]
    async fn check_policy(
        &self,
        p: Parameters<CheckPolicyParams>,
    ) -> Result<CallToolResult, McpError> {
        let p = p.0;
        let rules = self
            .memory
            .check_policy(&p.action, p.domain.as_deref())
            .await
            .map_err(mcp_err)?;
        ok(json!({ "rules": rules }))
    }

    #[tool(
        description = "Ask for an exception to a rule that refused you: for one task, until an expiry, with a justification. The scope and the justification are plain words. It lets nothing through until a person approves it. A rule of the L0 core takes no exception."
    )]
    async fn file_exception(
        &self,
        p: Parameters<FileExceptionParams>,
    ) -> Result<CallToolResult, McpError> {
        let p = p.0;
        let filed = self
            .memory
            .file_exception(crate::rules::ExceptionRequest {
                breaches: p.breaches,
                task: p.task,
                scope: p.scope,
                expires: p.expiry,
                justification: p.justification,
            })
            .await
            .map_err(mcp_err)?;
        ok(json!({ "exception": filed }))
    }

    #[tool(
        description = "Propose an amendment to a rule you find wrong in the case before you. It files a task for the rule's owner with your evidence. The rule stays in force until its owner changes it."
    )]
    async fn propose_amendment(
        &self,
        p: Parameters<ProposeAmendmentParams>,
    ) -> Result<CallToolResult, McpError> {
        let p = p.0;
        let created = self
            .memory
            .propose_amendment(&p.policy_id, &p.evidence)
            .await
            .map_err(mcp_err)?;
        ok(json!({ "task": created.task, "deduplicated": created.deduplicated }))
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

    #[tool(
        description = "One task, with its status, holder, lease, generation and handoff, the handoffs that earlier holders left after their claims, and the claims of it that were refused."
    )]
    async fn get_task(&self, p: Parameters<TaskIdParams>) -> Result<CallToolResult, McpError> {
        let t = self.memory.get_task(&p.0.id).await.map_err(mcp_err)?;
        let (late, refused) = self.memory.task_record(&p.0.id).await.map_err(mcp_err)?;
        let refused: Vec<Value> = refused
            .into_iter()
            .map(|(holder, why)| json!({ "holder": holder, "why": why }))
            .collect();
        ok(json!({ "task": t, "late_handoffs": late, "refused_claims": refused }))
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

impl McpServer {
    /// One call through the router; a harness's call is kept on the session's tape, and in
    /// a re-run answered from the tape first.
    async fn routed(
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
                kind_of(&tool, &arguments)
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
            eprintln!("amem: {tool} was not kept on the tape: {e}");
        }
        result
    }
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
        bounded(self.routed(request, context).await)
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
        let router = McpServer::routes();
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
            "promotions",
            "lineage",
            "log",
            "outside_readers",
            "registry_check",
            "resolve_identity",
            "proposals",
            "answer_proposal",
            "outage_status",
            "horizon_schedule",
            "recovery_bundle",
            "recovery_drill",
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
        let router = McpServer::routes();
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
        assert_eq!(task_status(None).unwrap(), dm_ledger::Status::Open);
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

    /// What a task that stays proposed lacks goes back to whoever filed it.
    #[tokio::test]
    async fn create_task_says_why_a_task_was_not_admitted() {
        let dir = tempfile::tempdir().unwrap();
        Memory::init(dir.path(), "human:test:me", "dom-me").unwrap();
        let path = dir.path().join(crate::GRANTS_FILE);
        let mut g = dm_policy::Grants::load(&path).unwrap();
        g.gates.require_definition = true;
        std::fs::write(&path, g.render().unwrap()).unwrap();
        let s = McpServer::new(Arc::new(Memory::open(dir.path()).unwrap()));
        let filed = payload(
            &s.create_task(Parameters(CreateTaskParams {
                title: "Tidy".into(),
                body: "Do it.".into(),
                ..CreateTaskParams::default()
            }))
            .await
            .unwrap(),
        );
        assert_eq!(filed["task"]["status"], "proposed");
        assert_eq!(filed["not_admitted_because"], "it names no task definition");
    }

    /// The three policy tools of the paper's surface answer over MCP.
    #[tokio::test]
    async fn the_policy_tools_give_the_rules_an_exception_and_an_amendment() {
        let (_dir, s) = server().await;
        let rule = "# A rule\n\n```policy\nid = \"pol-L2-060\"\nlayer = 2\nrule = \"A release is tagged by a person.\"\nintent = \"A tag cannot be taken back.\"\nowner = \"human:test:lead\"\nstatus = \"advisory\"\n```\n";
        let written = s
            .memory
            .write(crate::WriteRequest {
                entry_type: dm_core::EntryType::Policy,
                body: rule.into(),
                ..crate::WriteRequest::default()
            })
            .await
            .unwrap();
        let agent = crate::Memory::open(s.memory.root())
            .unwrap()
            .with_author("agent:curator");
        let crate::Promotion::Proposed { task } = agent
            .promote(&written.id, dm_core::TrustTier::Verified, "checked")
            .await
            .unwrap()
        else {
            panic!("proposed")
        };
        s.memory.approve(&task.id).await.unwrap();
        let rules = payload(
            &s.check_policy(Parameters(CheckPolicyParams {
                action: "tag a release".into(),
                domain: None,
            }))
            .await
            .unwrap(),
        );
        assert_eq!(rules["rules"][0]["policy_id"], "pol-L2-060");
        assert_eq!(rules["rules"][0]["status"], "advisory");
        assert_eq!(rules["rules"][0]["intent"], "A tag cannot be taken back.");
        let amend = |evidence: &str| ProposeAmendmentParams {
            policy_id: "pol-L2-060".into(),
            evidence: evidence.into(),
        };
        let task = payload(
            &s.propose_amendment(Parameters(amend("The tag was a test tag.")))
                .await
                .unwrap(),
        );
        assert_eq!(task["task"]["origin"], "amend:pol-L2-060");
        let id = task["task"]["id"].as_str().unwrap().to_string();
        let filed = payload(
            &s.file_exception(Parameters(FileExceptionParams {
                breaches: "pol-L2-060".into(),
                task: Some(id),
                scope: "one test tag".into(),
                expiry: "2099-01-01T00:00:00Z".into(),
                justification: "The tag is on a scratch repository.".into(),
            }))
            .await
            .unwrap(),
        );
        assert_eq!(filed["exception"]["approver"], "human:test:lead");
        assert_eq!(filed["exception"]["task"]["requirements"][0], "human");
        let none = s.propose_amendment(Parameters(amend(""))).await;
        assert!(none.is_err());
    }

    /// The same call made twice under one key, as after an answer that did not arrive.
    #[tokio::test]
    async fn a_claim_and_a_completion_asked_twice_under_a_key_happen_once() {
        let (_dir, s) = server().await;
        for title in ["First", "Second"] {
            let filed = payload(
                &s.create_task(Parameters(CreateTaskParams {
                    title: title.into(),
                    body: "Do it.".into(),
                    ..CreateTaskParams::default()
                }))
                .await
                .unwrap(),
            );
            let id = filed["task"]["id"].as_str().unwrap();
            s.memory.admit(id).await.unwrap();
        }
        let claim = || async {
            payload(
                &s.claim_next(Parameters(ClaimNextParams {
                    attempt: Some("try-1".into()),
                    ..ClaimNextParams::default()
                }))
                .await
                .unwrap(),
            )
        };
        let first = claim().await;
        let again = claim().await;
        assert_eq!(again["task"]["id"], first["task"]["id"]);
        assert_eq!(again["generation"], 1);
        let id = first["task"]["id"].as_str().unwrap().to_string();
        let h = write_handoff(&s, "Done.").await;
        let complete = |id: String, h: String| async {
            s.complete(Parameters(CompleteParams {
                id,
                handoff: h,
                attempt: Some("done-1".into()),
                ..CompleteParams::default()
            }))
            .await
        };
        let done = payload(&complete(id.clone(), h.clone()).await.unwrap());
        let done_again = payload(&complete(id.clone(), h.clone()).await.unwrap());
        assert_eq!(done_again["task"], done["task"]);
        assert_eq!(done["task"]["status"], "done");
        // the key claimed that task, whose claim is over: it claims nothing more
        let spent = s
            .claim_next(Parameters(ClaimNextParams {
                attempt: Some("try-1".into()),
                ..ClaimNextParams::default()
            }))
            .await;
        let why = spent
            .err()
            .map(|e| e.message.to_string())
            .unwrap_or_default();
        assert_eq!(
            why,
            "the attempt key try-1 was used before, and that attempt is over"
        );
    }

    #[tokio::test]
    async fn a_write_says_when_it_landed_withheld() {
        let (dir, s) = server().await;
        let owner = Memory::open(dir.path()).unwrap();
        let note = crate::WriteRequest {
            body: "Written in error.\n".into(),
            ..crate::WriteRequest::default()
        };
        let gone = owner.write(note).await.unwrap().id;
        let read = GetParams { id: gone.clone() };
        s.get(Parameters(read)).await.unwrap();
        let before = payload(&s.write(handoff("Stopped at step 1.\n")).await.unwrap());
        assert!(before.get("withheld").is_none(), "{before}");
        owner
            .erase(&gone, dm_core::erasure::Reason::Mistaken)
            .await
            .unwrap();
        let after = payload(&s.write(handoff("Stopped at step 2.\n")).await.unwrap());
        assert_eq!(after["withheld"], json!(gone), "{after}");
    }

    #[test]
    fn write_refuses_a_parameter_it_does_not_have() {
        let old =
            serde_json::from_value::<WriteParams>(json!({"body": "A fact.", "source": ["a"]}));
        let why = old.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(why.contains("source") && why.contains("cites"), "{why}");
        let new = serde_json::from_value::<WriteParams>(json!({"body": "A fact.", "cites": ["a"]}));
        assert_eq!(new.unwrap().cites, ["a"]);
    }

    #[tokio::test]
    async fn write_search_list_get_and_t0_round_trip() {
        let (_dir, s) = server().await;
        let w = payload(
            &s.write(Parameters(WriteParams {
                body: "# Stopped at step 3\n\nResume with the digests.\n".into(),
                entry_type: Some("handoff".into()),
                cites: vec!["https://example.org/run/1".into()],
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
            status: Some("finished".into()),
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

        s.heartbeat(Parameters(HeartbeatParams {
            id: id.clone(),
            ..HeartbeatParams::default()
        }))
        .await
        .unwrap();

        let missing = CompleteParams {
            id: id.clone(),
            handoff: "mem-me-missing".into(),
            ..CompleteParams::default()
        };
        assert!(s.complete(Parameters(missing)).await.is_err());
        let h = write_handoff(&s, "Done.").await;
        let done = payload(
            &s.complete(Parameters(CompleteParams {
                id: id.clone(),
                handoff: h.clone(),
                ..CompleteParams::default()
            }))
            .await
            .unwrap(),
        );
        assert_eq!(done["task"]["status"], "done");
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
                ..ReleaseParams::default()
            }))
            .await
            .unwrap(),
        );
        assert_eq!(r["task"]["status"], "open");
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
        // a stopped session is told so in words it can act on, as a refusal is
        let stopped = mcp_err(Error::Stale("the grants changed".into()));
        assert_eq!(stopped.code, invalid.code);
        assert!(stopped.message.contains("stopped: the grants changed"));
        // an action that waits in an outage is told so in words too, and is no fault
        let waits = mcp_err(Error::Outage("reads wait".into()));
        assert_eq!(waits.code, invalid.code);
        assert!(waits.message.contains("outage: reads wait"));
        assert_eq!(t0_uri("dom-a"), "memory://t0/dom-a");
    }

    /// A root with one restricted task and one public task, both admitted, and a server
    /// over a session capped at public: the restricted task's id, and the server.
    async fn restricted_and_capped() -> (tempfile::TempDir, String, McpServer) {
        let (dir, _owner) = server().await;
        let owner = Memory::open(dir.path()).unwrap();
        let mut ids = Vec::new();
        for (title, classification) in [
            ("Rotate the signing key", dm_core::Label::Restricted),
            ("Tidy the docs", dm_core::Label::Public),
        ] {
            let t = owner
                .file_own(dm_ledger::TaskSpec {
                    domain: "dom-me".into(),
                    title: title.into(),
                    body: "Do it.".into(),
                    requirements: vec![],
                    depends_on: vec![],
                    origin: None,
                    upstream_event: None,
                    created_by: String::new(),
                    classification,
                    priority: 0,
                    ..dm_ledger::TaskSpec::default()
                })
                .await
                .unwrap()
                .task;
            if t.status == dm_ledger::Status::Proposed {
                owner.admit(&t.id).await.unwrap();
            }
            ids.push(t.id);
        }
        let capped = Memory::open(dir.path())
            .unwrap()
            .with_handling_cap(dm_core::Label::Public);
        (dir, ids.swap_remove(0), McpServer::new(Arc::new(capped)))
    }

    #[tokio::test]
    async fn a_capped_session_neither_lists_nor_gets_a_task_above_its_handling() {
        let (_dir, hidden, s) = restricted_and_capped().await;
        let listed = payload(
            &s.list_tasks(Parameters(ListTasksParams::default()))
                .await
                .unwrap(),
        );
        let text = listed.to_string();
        assert!(text.contains("Tidy the docs"), "{text}");
        assert!(!text.contains("signing key"), "{text}");
        let refused = s
            .get_task(Parameters(TaskIdParams { id: hidden.clone() }))
            .await
            .map(|r| payload(&r))
            .unwrap_err();
        let absent = s
            .get_task(Parameters(TaskIdParams {
                id: "task-me-none".into(),
            }))
            .await
            .unwrap_err();
        assert_eq!(refused.code, absent.code, "the answer a missing task gets");
        assert_eq!(
            refused.message.replace(&hidden, "<id>"),
            absent.message.replace("task-me-none", "<id>")
        );
        assert_eq!(s.memory.label(), dm_core::Label::Public);
    }

    #[tokio::test]
    async fn a_spawn_asked_for_again_over_mcp_comes_back_in_existing() {
        let (_dir, s) = server().await;
        let parent = s
            .memory
            .create_task(dm_ledger::TaskSpec {
                domain: "dom-me".into(),
                title: "Root".into(),
                body: "Do it.".into(),
                requirements: vec![],
                depends_on: vec![],
                origin: None,
                upstream_event: None,
                created_by: String::new(),
                classification: dm_core::Label::Public,
                priority: 0,
                ..dm_ledger::TaskSpec::default()
            })
            .await
            .unwrap()
            .task;
        let ask = || {
            Parameters(FanOutParams {
                parent: parent.id.clone(),
                items: vec![FanOutItem {
                    title: "Rotate".into(),
                    body: "Rotate it.".into(),
                }],
            })
        };
        let first = payload(&s.fan_out(ask()).await.unwrap());
        let child = first["children"][0]["id"].as_str().unwrap().to_string();
        assert_eq!(
            first["existing"].as_array().map(Vec::len),
            Some(0),
            "{first}"
        );
        assert_eq!(first["withheld"], 0, "{first}");
        let again = payload(&s.fan_out(ask()).await.unwrap());
        assert_eq!(
            again["children"].as_array().map(Vec::len),
            Some(0),
            "{again}"
        );
        assert_eq!(
            again["existing"][0]["id"], child,
            "a spawn asked for again comes back as the earlier task: {again}"
        );
        assert!(again["continuation"].is_null(), "{again}");
    }

    #[tokio::test]
    async fn claim_next_over_mcp_skips_a_task_above_the_session_s_handling() {
        let (_dir, _hidden, s) = restricted_and_capped().await;
        let claimed = payload(
            &s.claim_next(Parameters(ClaimNextParams::default()))
                .await
                .unwrap(),
        );
        assert_eq!(claimed["task"]["title"], "Tidy the docs", "{claimed}");
        let none = payload(
            &s.claim_next(Parameters(ClaimNextParams::default()))
                .await
                .unwrap(),
        );
        assert!(none["task"].is_null(), "{none}");
        assert_eq!(s.memory.label(), dm_core::Label::Public);
    }

    /// A server over the record at `dir`, for a session of `author`.
    fn server_of(dir: &std::path::Path, author: &str) -> McpServer {
        let memory = Memory::open(dir).unwrap().with_author(author);
        McpServer::new(Arc::new(memory))
    }

    /// Write `body` through the `write` tool of `s`: the id of the entry.
    async fn write_note(s: &McpServer, body: &str) -> String {
        let params = WriteParams {
            body: body.into(),
            ..WriteParams::default()
        };
        let w = payload(&s.write(Parameters(params)).await.unwrap());
        w["id"].as_str().unwrap().to_string()
    }

    /// Read `id` through the `get` tool of a new session of `author`, and write `body`
    /// in that session: the id of the entry it wrote.
    async fn read_then_write(dir: &std::path::Path, author: &str, id: &str, body: &str) -> String {
        let s = server_of(dir, author);
        s.get(Parameters(GetParams { id: id.into() }))
            .await
            .unwrap();
        write_note(&s, body).await
    }

    /// The ids of the entries in a lineage answer, in its order.
    fn reached(answer: &Value) -> Vec<&str> {
        let entries = answer["entries"].as_array().unwrap();
        entries.iter().map(|e| e["id"].as_str().unwrap()).collect()
    }

    #[tokio::test]
    async fn the_lineage_tool_gives_the_owner_s_server_the_same_answer_as_the_library() {
        let (dir, s) = server().await;
        let x = write_note(&s, "# Port\n\nIt listens on 8080.\n").await;
        let y = read_then_write(dir.path(), "agent:r", &x, "# Proxy\n\nIt forwards.\n").await;
        let z = read_then_write(dir.path(), "agent:q", &y, "# Runbook\n\nRestart it.\n").await;
        let ask = |of: &str, back, depth| {
            let params = LineageParams {
                of: of.into(),
                back,
                depth,
            };
            s.lineage(Parameters(params))
        };
        let library = |l: crate::lineage::Lineage| serde_json::to_value(l).unwrap();
        // forward is the default, and with no depth it runs to the end
        let forward = payload(&ask(&x, None, None).await.unwrap());
        assert_eq!(reached(&forward), [y.as_str(), z.as_str()]);
        assert_eq!(forward["direction"], "forward");
        let same = s.memory.lineage(&x, Direction::Forward, None).await;
        assert_eq!(forward, library(same.unwrap()));
        assert_eq!(forward, payload(&ask(&x, Some(false), None).await.unwrap()));
        // a depth bounds the steps
        let near = payload(&ask(&x, None, Some(1)).await.unwrap());
        assert_eq!(reached(&near), [y.as_str()]);
        // and back gives what the entry was made from
        let back = payload(&ask(&z, Some(true), None).await.unwrap());
        assert_eq!(reached(&back), [y.as_str(), x.as_str()]);
        assert_eq!(back["direction"], "back");
        let same = s.memory.lineage(&z, Direction::Back, None).await;
        assert_eq!(back, library(same.unwrap()));
        assert_eq!(back["entries"][0]["informed"], true);
        assert_eq!(back["entries"][0]["hops"], 1);
    }

    /// The `lineage` tool of `s`, forward from `of`.
    async fn forward_of(s: &McpServer, of: &str) -> Result<CallToolResult, McpError> {
        let params = LineageParams {
            of: of.into(),
            ..LineageParams::default()
        };
        s.lineage(Parameters(params)).await
    }

    #[tokio::test]
    async fn lineage_and_log_are_refused_outside_the_security_role_and_served_inside_it() {
        let (dir, owner) = server().await;
        let grants = dir.path().join(crate::GRANTS_FILE);
        let text = std::fs::read_to_string(&grants).unwrap();
        std::fs::write(&grants, format!("security = [\"agent:security\"]\n{text}")).unwrap();
        // the owner's session started under the grants as they were: a new one
        drop(owner);
        let owner = server_of(dir.path(), "human:test:me");
        let x = write_note(&owner, "# Port\n\nIt listens on 8080.\n").await;
        let worker = server_of(dir.path(), "agent:worker");
        let refused = forward_of(&worker, &x).await.unwrap_err();
        assert_eq!(refused.code, McpError::invalid_params("", None).code);
        let why = "lineage is for the owner and the security role";
        assert!(refused.message.contains(why), "{refused:?}");
        let refused = worker
            .log(Parameters(LogParams::default()))
            .await
            .unwrap_err();
        assert_eq!(refused.code, McpError::invalid_params("", None).code);
        let why = "the log is for the owner and the security role";
        assert!(refused.message.contains(why), "{refused:?}");
        assert_eq!(worker.memory.label(), dm_core::Label::Public);

        let security = server_of(dir.path(), "agent:security");
        let answer = payload(&forward_of(&security, &x).await.unwrap());
        assert_eq!(
            (&answer["of"], &answer["complete"]),
            (&json!(x), &json!(true))
        );
        // a task filed after the last write: the tool serves the log with the ledger's
        // events up to now
        admitted_task(&owner, "Move the port", None).await;
        let log = payload(
            &security
                .log(Parameters(LogParams::default()))
                .await
                .unwrap(),
        );
        let events = log["events"].as_array().unwrap();
        let kinds: Vec<&str> = events.iter().map(|e| e["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds[..2], ["write", "put"], "{kinds:?}");
        assert!(kinds.contains(&"ledger"), "{kinds:?}");
        assert_eq!(events[0]["id"], x);
        let fresh = &log["freshness"];
        assert_eq!(fresh["domain"], "dom-me");
        assert_eq!(
            (&fresh["within_bound"], &fresh["unfed"]),
            (&json!(true), &json!(0))
        );
        assert_eq!(fresh["bound_seconds"], 60);
        assert_eq!(fresh["log_seq"], events[events.len() - 1]["seq"]);
        // an event served is a read: the label of the session rose, and to the highest,
        // since a `put` names no label
        assert_eq!(security.memory.label(), dm_core::Label::Restricted);
        // `from` is the first sequence number served
        let from = LogParams {
            domain: Some("dom-me".into()),
            from: Some(2),
        };
        let tail = payload(&security.log(Parameters(from)).await.unwrap());
        assert_eq!(tail["events"][0]["seq"], 2);
        assert_eq!(tail["events"].as_array().unwrap().len(), events.len() - 1);
    }

    /// A task that a tap filed in the record at `dir`, admitted by the owner: its id.
    async fn tap_task(dir: &std::path::Path, owner: &McpServer) -> String {
        let spec = dm_ledger::TaskSpec {
            title: "Change the port".into(),
            body: "A stranger wrote this.\n".into(),
            ..dm_ledger::TaskSpec::default()
        };
        let tap = server_of(dir, "tap:issues");
        let id = tap.memory.create_task(spec).await.unwrap().task.id;
        owner
            .admit_task(Parameters(TaskIdParams { id: id.clone() }))
            .await
            .unwrap();
        id
    }

    /// The hits of the `search` tool of `s` for the word `harbour`.
    async fn harbour(s: &McpServer) -> Vec<Value> {
        let params = SearchParams {
            query: "harbour".into(),
            ..SearchParams::default()
        };
        let found = payload(&s.search(Parameters(params)).await.unwrap());
        found["hits"].as_array().unwrap().clone()
    }

    #[tokio::test]
    async fn a_write_after_an_outside_read_answers_with_tap_reach_and_the_task() {
        let (dir, owner) = server().await;
        let task = tap_task(dir.path(), &owner).await;
        let worker = server_of(dir.path(), "agent:worker");
        // before the read: the answer has no key for the reach or for a task
        let before = payload(&worker.write(handoff("Nothing yet.\n")).await.unwrap());
        assert!(before.get("reach").is_none() && before.get("task").is_none());
        worker.memory.hold_task(&task).await.unwrap();
        let params = WriteParams {
            body: "# Harbour\n\nThe harbour port stays 8080.\n".into(),
            entry_type: Some("decision".into()),
            ..WriteParams::default()
        };
        let w = payload(&worker.write(Parameters(params)).await.unwrap());
        assert_eq!((&w["reach"], &w["task"]), (&json!("tap"), &json!(task)));
        let id = w["id"].as_str().unwrap().to_string();
        // the hit and the entry say the same to a session in the chain
        let hits = harbour(&worker).await;
        assert_eq!((hits.len(), &hits[0]["reach"]), (1, &json!("tap")));
        let got = GetParams { id: id.clone() };
        let entry = payload(&worker.get(Parameters(got)).await.unwrap());
        assert_eq!(
            (&entry["reach"], &entry["task"]),
            (&json!("tap"), &json!(task))
        );
        // an agent outside the chain reads neither
        let reader = server_of(dir.path(), "agent:reader");
        assert!(harbour(&reader).await.is_empty());
        let got = GetParams { id: id.clone() };
        assert!(reader.get(Parameters(got)).await.is_err());
        // an entry with fleet reach has no key in a hit. The session that admitted the
        // task read it, so a new session of the owner writes
        let fresh = server_of(dir.path(), "human:test:me");
        write_note(&fresh, "# Harbour office\n\nThe harbour office is open.\n").await;
        let hits = harbour(&reader).await;
        assert_eq!(hits.len(), 1);
        assert!(hits[0].get("reach").is_none(), "{hits:?}");
    }

    #[tokio::test]
    async fn outside_readers_is_refused_to_an_agent_and_gives_the_owner_the_library_s_answer() {
        let (dir, owner) = server().await;
        let task = tap_task(dir.path(), &owner).await;
        let none = OutsideReadersParams::default();
        let empty = payload(&owner.outside_readers(Parameters(none)).await.unwrap());
        assert_eq!(empty, json!({ "readers": [] }));
        let worker = server_of(dir.path(), "agent:worker");
        worker.memory.hold_task(&task).await.unwrap();
        let id = write_note(&worker, "# Port\n\nIt listens on 8080.\n").await;
        let none = OutsideReadersParams::default();
        let refused = worker.outside_readers(Parameters(none)).await.unwrap_err();
        assert_eq!(refused.code, McpError::invalid_params("", None).code);
        let why = "the log is for the owner and the security role";
        assert!(refused.message.contains(why), "{refused:?}");
        let named = OutsideReadersParams {
            domain: Some("dom-me".into()),
        };
        let answer = payload(&owner.outside_readers(Parameters(named)).await.unwrap());
        let readers = answer["readers"].as_array().unwrap();
        assert_eq!(readers.len(), 1, "{answer}");
        assert_eq!(readers[0]["by"], "agent:worker");
        assert_eq!(readers[0]["writes"], json!([id]));
        let library = owner.memory.outside_readers(None).await.unwrap();
        assert_eq!(answer["readers"], serde_json::to_value(library).unwrap());
        // the domain asked for is the domain read: one that is not granted is refused
        let other = OutsideReadersParams {
            domain: Some("dom-other".into()),
        };
        assert!(owner.outside_readers(Parameters(other)).await.is_err());
    }

    fn id_params(id: &str) -> Parameters<GetParams> {
        Parameters(GetParams { id: id.into() })
    }

    fn demote_params(id: &str, reason: &str) -> Parameters<DemoteParams> {
        Parameters(DemoteParams {
            id: id.into(),
            reason: reason.into(),
        })
    }

    #[tokio::test]
    async fn demote_is_refused_to_an_agent_with_no_curator_s_task_in_the_words_of_the_library() {
        let (dir, owner) = server().await;
        let id = write_note(&owner, "# Port\n\nIt listens on 8080.\n").await;
        let worker = server_of(dir.path(), "agent:worker");
        let refused = worker.demote(demote_params(&id, "dead")).await.unwrap_err();
        assert_eq!(refused.code, McpError::invalid_params("", None).code);
        let library = worker.memory.demote(&id, DemoteReason::Dead).await;
        assert_eq!(refused.message, library.unwrap_err().to_string());
        let why = "only on a task of a curator or of the auditor, and agent:worker holds none";
        assert!(refused.message.contains(why), "{refused:?}");
        let bad = owner.demote(demote_params(&id, "stale")).await.unwrap_err();
        assert_eq!(bad.message, "reason: dead or wrong");

        // the survey of the dead: the window of the caller, and 90 days with none
        let now = DeadParams {
            days: Some(0),
            ..DeadParams::default()
        };
        let dead = payload(&owner.dead(Parameters(now)).await.unwrap());
        assert_eq!(dead["dead"][0]["id"], id, "{dead}");
        let default = payload(&owner.dead(Parameters(DeadParams::default())).await.unwrap());
        assert_eq!(default, json!({ "dead": [] }));

        let done = payload(&owner.demote(demote_params(&id, "wrong")).await.unwrap());
        assert_eq!(done["id"], id);
        assert_eq!(done["reason"], "wrong");
        assert_eq!(done["by"], "human:test:me");
        let gone = owner.get(id_params(&id)).await.unwrap_err();
        assert!(gone.message.contains("demoted"), "{gone:?}");
        let refused = worker.restore(id_params(&id)).await.unwrap_err();
        let why = "only a person restores a demoted entry";
        assert!(refused.message.contains(why), "{refused:?}");
        let back = payload(&owner.restore(id_params(&id)).await.unwrap());
        assert_eq!(back, json!({ "restored": id }));
        owner.get(id_params(&id)).await.unwrap();
    }

    fn outcome_params(task: &str, kind: &str) -> Parameters<RecordOutcomeParams> {
        Parameters(RecordOutcomeParams {
            task: task.into(),
            kind: kind.into(),
            note: Some("The port is 8443.".into()),
        })
    }

    /// A survey that files what it finds, or one that names no `file`.
    fn filing(file: bool) -> Parameters<SurveyParams> {
        Parameters(SurveyParams {
            domain: None,
            file: file.then_some(true),
        })
    }

    #[tokio::test]
    async fn record_outcome_is_refused_to_an_agent_and_a_correction_is_filed_once() {
        let (dir, owner) = server().await;
        let task = admitted_task(&owner, "Find the port", None).await;
        let worker = server_of(dir.path(), "agent:worker");
        let refused = worker.record_outcome(outcome_params(&task, "corrected"));
        let refused = refused.await.unwrap_err();
        let why = "only a person records the outcome of a task";
        assert!(refused.message.contains(why), "{refused:?}");
        let bad = owner.record_outcome(outcome_params(&task, "wrong"));
        let bad = bad.await.unwrap_err().message;
        assert_eq!(bad, "kind: accepted, corrected, rejected or reverted");
        let empty = payload(&owner.corrections(filing(true)).await.unwrap());
        let none = json!({ "corrections": [], "filed": [], "already_filed": 0 });
        assert_eq!(empty, none);

        let said = owner.record_outcome(outcome_params(&task, "corrected"));
        let said = payload(&said.await.unwrap());
        assert_eq!(said["outcomes"][0]["outcome"], "corrected");
        assert_eq!(said["outcomes"][0]["by"], "human:test:me");
        assert_eq!(said["outcomes"][0]["note"], "The port is 8443.");
        let params = OutcomesParams { task: task.clone() };
        let read = payload(&owner.outcomes(Parameters(params)).await.unwrap());
        assert_eq!(read, said);
        let library = owner.memory.outcomes(&task).await.unwrap();
        assert_eq!(read["outcomes"], serde_json::to_value(library).unwrap());

        let only = payload(&owner.corrections(filing(false)).await.unwrap());
        assert_eq!(only["corrections"][0]["task"], task);
        assert_eq!(only["filed"], json!([]), "not filed unless asked: {only}");
        let found = payload(&owner.corrections(filing(true)).await.unwrap());
        let filed = found["filed"][0].as_str().expect("a task").to_string();
        let t = owner.memory.get_task(&filed).await.unwrap();
        assert_eq!(t.spec.origin.as_deref(), Some("curator:correction"));
        let again = payload(&owner.corrections(filing(true)).await.unwrap());
        assert_eq!(again["filed"], json!([]));
        assert_eq!(again["already_filed"], 1);
    }

    #[tokio::test]
    async fn auditor_mode_gives_the_answer_of_the_library() {
        let (_dir, owner) = server().await;
        let mode = || async {
            let none = Parameters(NoParams::default());
            payload(&owner.auditor_mode(none).await.unwrap())
        };
        let fresh = json!({
            "mode": "fresh-session", "qualified": false,
            "auditor_model": null, "human_sample": 3,
        });
        assert_eq!(mode().await, fresh);
        let score = |model: &str, caught| crate::audit::Score {
            model: model.into(),
            caught,
            missed: 6 - caught,
            false_alarms: 0,
        };
        let at = time::OffsetDateTime::UNIX_EPOCH;
        let q = crate::audit::qualify(score("m-a", 4), Some(score("m-b", 5)), at);
        owner.memory.set_qualification(&q).unwrap();
        let other = mode().await;
        assert_eq!(other["mode"], "other-model");
        assert_eq!(other["auditor_model"], "m-b");
        assert_eq!(other["human_sample"], 1);
        let library = owner.memory.auditor_mode().unwrap();
        assert_eq!(other, serde_json::to_value(library).unwrap());
    }

    fn convention(owner: Option<&str>) -> Parameters<WriteParams> {
        Parameters(WriteParams {
            body: "# Titles\n\nA title says what an entry claims.\n".into(),
            entry_type: Some("convention".into()),
            owner: owner.map(str::to_string),
            ..WriteParams::default()
        })
    }

    #[tokio::test]
    async fn write_takes_an_owner_and_check_conventions_files_an_entry_that_names_none() {
        let (_dir, s) = server().await;
        let refused = s.write(convention(Some("agent:worker"))).await;
        assert!(refused.is_err(), "an owner is a person");
        let owned = payload(&s.write(convention(Some("human:test:lead"))).await.unwrap());
        let got = payload(
            &s.get(id_params(owned["id"].as_str().unwrap()))
                .await
                .unwrap(),
        );
        assert_eq!(got["owner"], "human:test:lead", "{got}");
        let clean = payload(&s.check_conventions(filing(true)).await.unwrap());
        let none = json!({ "failures": [], "filed": [], "already_filed": 0 });
        assert_eq!(clean, none);

        let bare = payload(&s.write(convention(None)).await.unwrap());
        let found = payload(&s.check_conventions(filing(false)).await.unwrap());
        assert_eq!(found["failures"][0]["check"], "ownership", "{found}");
        assert_eq!(found["failures"][0]["entry"], bare["id"]);
        assert_eq!(found["failures"][0]["owner"], "human:test:me");
        assert_eq!(found["filed"], json!([]), "not filed unless asked: {found}");
        let filed = payload(&s.check_conventions(filing(true)).await.unwrap());
        let task = filed["filed"][0].as_str().expect("a task").to_string();
        let t = s.memory.get_task(&task).await.unwrap();
        assert_eq!(t.spec.origin.as_deref(), Some("convention:ownership"));
        let again = payload(&s.check_conventions(filing(true)).await.unwrap());
        assert_eq!(again["filed"], json!([]));
        assert_eq!(again["already_filed"], 1);
    }

    /// File a task through `s`, claim it and complete it with a handoff.
    async fn done_task(s: &McpServer, title: &str) -> String {
        let id = admitted_task(s, title, None).await;
        let claim = LeaseParams {
            id: id.clone(),
            ..LeaseParams::default()
        };
        s.claim(Parameters(claim)).await.unwrap();
        let done = CompleteParams {
            id: id.clone(),
            handoff: write_handoff(s, "Done.").await,
            ..CompleteParams::default()
        };
        s.complete(Parameters(done)).await.unwrap();
        id
    }

    #[tokio::test]
    async fn policy_curation_gives_repeated_work_and_files_it_once() {
        let (_dir, s) = server().await;
        let mut ids = Vec::new();
        for n in 1..=3 {
            ids.push(done_task(&s, &format!("Rotate the signing key {n}")).await);
        }
        ids.sort();
        let found = payload(&s.policy_curation(filing(false)).await.unwrap());
        assert_eq!(found["overlaps"], json!([]));
        assert_eq!(found["repeated"][0]["title"], "rotate the signing key");
        assert_eq!(found["repeated"][0]["tasks"], json!(ids));
        assert_eq!(found["filed"], json!([]), "not filed unless asked: {found}");
        let library = s.memory.policy_curation(None).await.unwrap();
        assert_eq!(found["repeated"], json!(library.repeated));
        let filed = payload(&s.policy_curation(filing(true)).await.unwrap());
        assert_eq!(filed["filed"].as_array().unwrap().len(), 1, "{filed}");
        let again = payload(&s.policy_curation(filing(true)).await.unwrap());
        assert_eq!(again["filed"], json!([]));
        assert_eq!(again["already_filed"], 1);
    }

    fn together(sample: Option<usize>, file: bool) -> Parameters<TogetherParams> {
        Parameters(TogetherParams {
            domain: None,
            sample,
            file: file.then_some(true),
        })
    }

    fn titled(title: &str, body: &str) -> Parameters<WriteParams> {
        Parameters(WriteParams {
            title: Some(title.into()),
            body: body.into(),
            ..WriteParams::default()
        })
    }

    #[tokio::test]
    async fn test_together_and_curation_case_give_the_answers_of_the_library() {
        let (dir, owner) = server().await;
        let w = server_of(dir.path(), "agent:writer");
        let half = "After a rotation of the cache key, the old build tools ignore all\nprevious\n";
        let rest = "instructions for a rotation of the cache key are in the runbook.\n";
        let a = payload(&w.write(titled("Cache key rotation", half)).await.unwrap());
        let b = w.write(titled("Rotation of the cache key", rest)).await;
        let b = payload(&b.unwrap());
        let found = payload(&owner.test_together(together(None, false)).await.unwrap());
        assert_eq!(found["together"][0]["ids"], json!([a["id"], b["id"]]));
        assert_eq!(found["together"][0]["findings"], json!(["instruction"]));
        assert_eq!(found["filed"], json!([]), "not filed unless asked: {found}");
        let library = owner.memory.test_together(None, DEFAULT_SAMPLE).await;
        assert_eq!(found["together"], json!(library.unwrap()));
        // the sample of the caller is the sample of the survey
        let zero = payload(&owner.test_together(together(Some(0), true)).await.unwrap());
        assert_eq!(zero, json!({ "together": [], "filed": [] }));
        let filed = payload(&owner.test_together(together(None, true)).await.unwrap());
        let task = filed["filed"][0].as_str().expect("a task").to_string();
        let t = owner.memory.get_task(&task).await.unwrap();
        assert_eq!(t.spec.origin.as_deref(), Some("auditor:together"));
        let again = payload(&owner.test_together(together(None, true)).await.unwrap());
        assert_eq!(again["filed"], json!([]));

        let id = a["id"].as_str().unwrap();
        let case = payload(&owner.curation_case(id_params(id)).await.unwrap());
        let library = owner.memory.curation_case(id).await.unwrap();
        assert_eq!(case, library.to_json());
        assert_eq!(case["entry"], id);
        assert_eq!(case["unsupported"], true);
        assert!(case["text"].as_str().unwrap().contains("old build tools"));
    }

    #[test]
    fn the_surveys_of_the_duties_only_read_and_the_duties_change() {
        let none = json!({});
        for tool in [
            "dead",
            "outcomes",
            "auditor_mode",
            "curation_case",
            "registry_check",
            "resolve_identity",
            "proposals",
            "outage_status",
        ] {
            assert_eq!(kind_of(tool, &none), tape::Kind::Read, "{tool}");
        }
        for tool in ["demote", "restore", "record_outcome", "check_conventions"] {
            assert_eq!(kind_of(tool, &none), tape::Kind::Mutation, "{tool}");
        }
    }

    #[test]
    fn lineage_and_log_only_read() {
        let none = json!({});
        assert_eq!(kind_of("lineage", &none), tape::Kind::Read);
        assert_eq!(kind_of("log", &none), tape::Kind::Read);
        assert_eq!(kind_of("outside_readers", &none), tape::Kind::Read);
        assert_eq!(kind_of("write", &none), tape::Kind::Mutation);
    }
}
