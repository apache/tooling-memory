//! The MCP binding: the operations of the paper's Appendix A as tools, and T0 as a
//! resource. Every tool is a thin call into [`Memory`]; nothing lives only here.
//!
//! A harness connects over stdio. It gets `search`, `get`, `write`, `list`, `t0`, the
//! ledger operations, and a `memory://t0/<domain>` resource per granted domain. What it
//! never gets is a path on disk, a credential, or an entry its grants do not admit.

use crate::brief::{AnswerKind, Ask};
use crate::duties::{CheckOptions, DemoteReason, DEAD_WINDOW_DAYS};
use crate::lineage::Direction;
use crate::recall::{self, Budget, Depth};
use crate::{fetch, tape, Error, Memory, Narrow, WriteRequest};
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
            | dm_ledger::Error::AttemptSpent { .. }
            | dm_ledger::Error::BadPin(_)
            | dm_ledger::Error::Pinned { .. },
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

/// One hit as a binding returns it: the summary of an entry, with the snippet that the
/// index cut around the words of the query when it cut one. One shape for the command
/// line and the memory tools.
#[must_use]
pub fn hit_json(h: &Hit) -> Value {
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
    if let Some(snippet) = &h.snippet {
        v["snippet"] = json!(snippet);
    }
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

/// An RFC 3339 time, or the refusal that names the parameter.
fn parse_time(what: &str, s: Option<String>) -> Result<Option<time::OffsetDateTime>, McpError> {
    s.map(|v| {
        time::OffsetDateTime::parse(&v, &Rfc3339).map_err(|e| {
            McpError::invalid_params(format!("{what}: {e}: give an RFC 3339 time"), None)
        })
    })
    .transpose()
}

/// The filters of `search`. Each one narrows the search within what this session may
/// read.
#[derive(Deserialize, schemars::JsonSchema, Default)]
#[serde(deny_unknown_fields)]
pub struct FiltersParams {
    /// Only entries of this kind: decision, fact, convention, method, handoff, policy,
    /// or position.
    #[serde(default, rename = "type")]
    pub entry_type: Option<String>,
    /// Only entries at this trust tier or above: raw, curated, or verified.
    #[serde(default)]
    pub min_tier: Option<String>,
    /// Only entries of this author, as the principal: `agent:a`.
    #[serde(default)]
    pub author: Option<String>,
    /// Only entries created at this time or after it, RFC 3339.
    #[serde(default)]
    pub since: Option<String>,
}

/// The filters of a search as the index takes them. No filters narrow nothing.
fn narrow(filters: Option<FiltersParams>) -> Result<Narrow, McpError> {
    let Some(f) = filters else {
        return Ok(Narrow::default());
    };
    Ok(Narrow {
        entry_type: parse("filters.type", f.entry_type)?,
        min_tier: parse("filters.min_tier", f.min_tier)?,
        author: f.author,
        since: parse_time("filters.since", f.since)?,
    })
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
    /// Filters that narrow the search: by type, by lowest trust tier, by author, by
    /// time of creation.
    #[serde(default)]
    pub filters: Option<FiltersParams>,
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
    /// The tokens that your context holds now, approximately. The answer is smaller
    /// when your context is full.
    #[serde(default)]
    pub context_tokens: Option<usize>,
    /// The kind of answer that you need: fact, list, or overview. It sets the length of
    /// the answer.
    #[serde(default)]
    pub answer_kind: Option<String>,
}

/// Parameters of `get`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct GetParams {
    /// The address of what to read: the entry id, as a hit or T0 names it; or
    /// `<id>#<anchor>` for one section; or `<id>@<version>` for one version.
    pub id: String,
}

/// Parameters that name an entry.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct EntryParams {
    /// The entry id, as a hit or T0 names it.
    pub id: String,
}

/// Parameters of `approve`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct ApproveParams {
    /// The id of the task that asks for the approval.
    pub task: String,
    /// When the approval of a sign-off task expires, RFC 3339. 24 hours from now when
    /// omitted.
    #[serde(default)]
    pub expires: Option<String>,
}

/// Parameters of `approvals`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct ApprovalsParams {
    /// One entry; every entry of the domain when omitted.
    #[serde(default)]
    pub id: Option<String>,
    /// A granted domain; the only one when omitted.
    #[serde(default)]
    pub domain: Option<String>,
}

/// Parameters of `export`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct ExportParams {
    /// A granted domain; the only one when omitted.
    #[serde(default)]
    pub domain: Option<String>,
    /// The highest label that the bundle may hold: public, internal, or restricted.
    pub max_label: String,
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
    /// decision, fact, convention, method, handoff, policy, or position. Default fact.
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
    /// decision, fact, convention, method, handoff, policy, or position. Omit for every
    /// kind.
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
    /// The task this one is split from, when it is not the task you hold.
    #[serde(default)]
    pub parent: Option<String>,
    /// The decisions this task carries out, as entry ids.
    #[serde(default)]
    pub implements: Vec<String>,
    /// Whether what the task does can be undone: `reversible` or `irreversible`.
    #[serde(default)]
    pub reversibility: Option<String>,
    /// The owned paths the task changes, as globs.
    #[serde(default)]
    pub artifacts_touched: Vec<String>,
    /// The position this task is pinned to, `pos-<name>`: only a session that fills
    /// the position claims the task. Give `pin_reason` with it.
    #[serde(default)]
    pub position: Option<String>,
    /// Why the task is pinned: continuity, relationship, or separation.
    #[serde(default)]
    pub pin_reason: Option<String>,
}

/// Parameters of `wait_for`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct WaitParams {
    /// The task id.
    pub id: String,
    /// The generation of your claim. Optional in the session that made the claim.
    #[serde(default)]
    pub generation: Option<u64>,
    /// The name of the outside event the task waits on, one token, as the tap that
    /// reports it will name it.
    pub on: String,
    /// A handoff entry you wrote about what you did before waiting; the next claimant
    /// starts from it. Optional.
    #[serde(default)]
    pub handoff: Option<String>,
}

/// Parameters of `effect_begun` and `effect_settled`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct EffectParams {
    /// The task id.
    pub id: String,
    /// The generation of your claim. Optional in the session that made the claim.
    #[serde(default)]
    pub generation: Option<u64>,
    /// A stable id for the external action, one token of at most 64 characters. A retry
    /// of the action names the same id.
    pub action: String,
    /// For `effect_settled`: `applied` when the destination holds the effect,
    /// `not_applied` when it does not.
    #[serde(default)]
    pub outcome: Option<String>,
}

/// Parameters of `position`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct PositionParams {
    /// The position's name, `pos-<name>`.
    pub name: String,
    /// A granted domain; the one that holds the position when omitted.
    #[serde(default)]
    pub domain: Option<String>,
}

/// Parameters of `positions`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct PositionsParams {
    /// A granted domain. Omit for every granted domain.
    #[serde(default)]
    pub domain: Option<String>,
}

/// Parameters of `retire_position`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct RetirePositionParams {
    /// The position's name, `pos-<name>`.
    pub name: String,
    /// Why it is retired: words on one line. The retiring entry keeps them.
    pub why: String,
    /// A granted domain; the one that holds the position when omitted.
    #[serde(default)]
    pub domain: Option<String>,
}

/// Parameters of `scorecards`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct ScorecardsParams {
    /// The most scorecards to return, newest first. Ten when omitted.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// One outcome of `score_evaluation`: what the runner says of one run.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct OutcomeParams {
    /// The session of the run. Its tape gives the task, the seed and the arm.
    pub session: String,
    /// Whether the judge accepted the answer.
    pub success: bool,
    /// What the run cost, in tokens.
    pub cost: u64,
    /// The name of the judge that decided the success.
    pub judge: String,
}

/// Parameters of `score_evaluation`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct ScoreEvaluationParams {
    /// The plan: the TOML text of the evaluation set file, with its `[plan]` table.
    pub plan: String,
    /// One outcome for each run.
    pub outcomes: Vec<OutcomeParams>,
    /// Also keep the scorecard as an entry in the platform's domain.
    #[serde(default)]
    pub keep: bool,
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

/// Parameters of `erase`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct EraseParams {
    /// The entry id.
    pub id: String,
    /// Why, as a class: `requested`, `mistaken`, `sensitive` or `legal`. The receipt
    /// keeps it.
    pub reason: String,
}

/// Parameters of `declassify`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct DeclassifyParams {
    /// The entry id.
    pub id: String,
    /// The label of the copy, lower than the label of the entry: `public` or
    /// `internal`.
    pub target_label: String,
    /// Why the text may be read at the lower label, in words on one line. The guard
    /// reads it.
    pub justification: String,
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

/// Parameters of `verdict`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct VerdictParams {
    /// The id of the task whose work waits for a verdict.
    pub id: String,
    /// True to accept the work, false to reject it.
    pub accept: bool,
}

/// Parameters that name a task definition.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct DefinitionParams {
    /// The name of the task definition.
    pub definition: String,
}

/// One answer in a graded run.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct GradeAnswer {
    /// The name of a scenario of the held-out suite.
    pub scenario: String,
    /// The id of the handoff entry that answers the scenario.
    pub handoff: String,
}

/// Parameters of `grade`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct GradeParams {
    /// The name of the task definition.
    pub definition: String,
    /// The level that the run is for: `person`, `review` or `checks`.
    pub level: String,
    /// One answer for each scenario that was worked. A scenario without an answer
    /// fails.
    #[serde(default)]
    pub answers: Vec<GradeAnswer>,
}

/// Parameters of `metrics`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct MetricsParams {
    /// A granted domain; the only one when omitted.
    #[serde(default)]
    pub domain: Option<String>,
    /// The length of the window, in days. Default 7.
    #[serde(default)]
    pub days: Option<u32>,
}

/// Parameters of `task_times`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct TaskTimesParams {
    /// The task id.
    pub task: String,
}

/// Parameters that name a landing queue.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct QueueParams {
    /// The name of the landing queue, as the grants name it.
    pub queue: String,
}

/// Parameters of `queue_landing`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct QueueLandingParams {
    /// The id of the task that you hold.
    pub task: String,
    /// The name of the landing queue, as the grants name it.
    pub queue: String,
    /// The id of the handoff entry you wrote with `write` describing the change.
    pub handoff: String,
    /// The generation of your claim. Optional in the session that made the claim.
    #[serde(default)]
    pub generation: Option<u64>,
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
    "position",
    "positions",
    "settings",
    "scorecards",
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
    "autonomy",
    "candidate_gates",
    "metrics",
    "limits",
    "task_times",
    "landing",
    "approvals",
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
        // what a harness asks through this server, an agent may have asked
        memory.mark_served();
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

    /// A server for a session of a person, with no harness behind it: a person started
    /// it and types its calls. The session is not marked as served, so it approves as
    /// the command line does, and its calls are on no tape. Nobody authenticates here:
    /// the caller is trusted as the caller of the command line is.
    #[must_use]
    pub fn for_person(memory: Arc<Memory>) -> Self {
        Self {
            memory,
            tool_router: Self::routes(),
            taped: false,
        }
    }

    #[tool(
        description = "Ranked search over memory, summaries by default; open a hit with `get` if its summary warrants it. A hit of an entry with a body has a `snippet`: the words of the body around the match. `filters` narrows the search by `type`, `min_tier`, `author` and `since`. With depth `sections` the best hits come with their bodies, within a budget, so one call does the work of a search and several gets. Search before writing a fact that may already exist."
    )]
    async fn search(&self, p: Parameters<SearchParams>) -> Result<CallToolResult, McpError> {
        let p = p.0;
        let narrow = narrow(p.filters)?;
        if parse::<Depth>("depth", p.depth)? == Some(Depth::Sections) {
            let pack = self
                .memory
                .search_sections_with(&p.query, p.domain.as_deref(), &narrow, limit(p.limit))
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
            .search_with(&p.query, p.domain.as_deref(), &narrow, limit(p.limit))
            .await
            .map_err(mcp_err)?;
        ok(json!({ "hits": hits.iter().map(hit_json).collect::<Vec<_>>() }))
    }

    #[tool(
        description = "One entry: its frontmatter and Markdown body. `id` is an address. `<id>` gives the current version in full. `<id>#<anchor>` gives one section; an anchor is the heading in lower case with `-` between its words, and the refusal of an unknown anchor lists the anchors of the entry. `<id>@<version>` gives one version, with `current: false` when the entry has changed since; `version` in every answer is the value to cite. Reading raises this session's label to the entry's classification. `related`, when present, gives the summaries of the entries that searches return together with this one, each with its weight `p`."
    )]
    async fn get(&self, p: Parameters<GetParams>) -> Result<CallToolResult, McpError> {
        let fetched = self.memory.fetch(&p.0.id).await.map_err(mcp_err)?;
        ok(fetch::to_json(&fetched))
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
        description = "The dead entries that you read: older than the window, not read inside it, named by no active entry, not verified, and not a handoff, a policy entry, a position or a canary. Each is a candidate for `demote` with the reason `dead`. The list is not a read of the entries, but it raises your label to their classification."
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
            .record_outcome_event(&p.task, kind, p.note.as_deref().unwrap_or(""))
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
    async fn promotions(&self, p: Parameters<EntryParams>) -> Result<CallToolResult, McpError> {
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
        description = "Delegated recall, one turn either way: a question in; a cited brief out when this server has a model behind it, else the pack the brief would be written from, with the synthesis owed to you in this turn. `synthesis.by` says which. Cheaper than a search and several gets when the question spans entries. Give `answer_kind` (fact, list or overview) and `context_tokens` to get an answer of the size that you need; `plan` then says the size that was chosen."
    )]
    async fn recall(&self, p: Parameters<RecallParams>) -> Result<CallToolResult, McpError> {
        let p = p.0;
        let depth = parse::<Depth>("depth", p.depth)?;
        let budget = recall_budget(p.limit, p.budget_tokens);
        let ask = Ask {
            context_tokens: p.context_tokens,
            answer_kind: parse::<AnswerKind>("answer_kind", p.answer_kind)?,
        };
        let r = self
            .memory
            .recall_for(&p.question, p.domain.as_deref(), depth, budget, ask)
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
        description = "File a task on the ledger. It is born proposed and becomes claimable once the owner admits it. With `position` and `pin_reason` the task is pinned: only a session that fills that position claims it, and it stays proposed until a person or a session that fills the position under its charter's agent definition admits it."
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
            parent: p.parent,
            implements: p.implements,
            reversibility: parse("reversibility", p.reversibility)?,
            artifacts_touched: p.artifacts_touched,
            position: p.position,
            pin_reason: parse("pin_reason", p.pin_reason)?,
            ..dm_ledger::TaskSpec::default()
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
        description = "Admit a proposed task so it can be claimed. On a laptop the owner is the triage. A task pinned to a position is admitted by a person, or by a session that fills the position under the agent definition its charter names."
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
        description = "Complete a task with the handoff entry you wrote describing what happened. Write the handoff first with `write`, entry_type handoff. A task under a task definition can come back `blocked`, waiting on `verdict:<task>`: your work is handed in, and the task is done when a person or a reviewer accepts it. Do not complete it again."
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
        description = "Give a held task up until an outside event comes: a check that runs, a reply, a change that has not landed. Do not wait in the session. The task is blocked, and nobody may claim it, until a tap or a person reports the event; then it is offered again. Pass the id of a handoff you wrote so the next claimant starts where you stopped. To queue a change for landing, call `queue_landing`: this tool refuses an event named `landing:<queue>`."
    )]
    async fn wait_for(&self, p: Parameters<WaitParams>) -> Result<CallToolResult, McpError> {
        let p = p.0;
        let t = self
            .memory
            .wait_for(&p.id, p.generation, &p.on, p.handoff.as_deref())
            .await
            .map_err(mcp_err)?;
        ok(json!({ "task": t }))
    }

    #[tool(
        description = "Record that you are about to make an external action under your claim, before you make it. Until the action is settled, the task goes to nobody else, even when your lease runs out. A retry of the action names the same action id."
    )]
    async fn effect_begun(&self, p: Parameters<EffectParams>) -> Result<CallToolResult, McpError> {
        let p = p.0;
        let t = self
            .memory
            .effect_begun(&p.id, p.generation, &p.action)
            .await
            .map_err(mcp_err)?;
        ok(json!({ "task": t }))
    }

    #[tool(
        description = "Record what became of an external action you began: `applied` when the destination holds the effect, `not_applied` when it does not. Only while your claim is in force. After that a person reconciles it: you cannot tell a lost answer from an action that was not made."
    )]
    async fn effect_settled(
        &self,
        p: Parameters<EffectParams>,
    ) -> Result<CallToolResult, McpError> {
        let p = p.0;
        let outcome = parse::<dm_ledger::Outcome>("outcome", p.outcome)?
            .ok_or_else(|| McpError::invalid_params("outcome: applied or not_applied", None))?;
        let t = self
            .memory
            .effect_settled(&p.id, p.generation, &p.action, outcome)
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
        description = "Erase an entry. It cannot be taken back: the text leaves every version and the index, and a stub and a receipt remain. It runs only under an approval of the owner for this version of the entry. Without one, the call is refused, and the refusal names the task that asks the owner for the approval. Call again after the approval. If the entry changed since, the approval is of another version, and a new task asks again."
    )]
    async fn erase(&self, p: Parameters<EraseParams>) -> Result<CallToolResult, McpError> {
        let p = p.0;
        let reason: dm_core::erasure::Reason = parse("reason", Some(p.reason))?
            .ok_or_else(|| McpError::invalid_params("reason: a class of reason", None))?;
        let receipt = self.memory.erase(&p.id, reason).await.map_err(mcp_err)?;
        ok(serde_json::to_value(receipt).unwrap_or_default())
    }

    #[tool(
        description = "Ask the guard of the domain for a copy of an entry at a lower label, with your reason in words on one line. `proposed` means that the guard has a task and that nothing exists at the lower label. Call again after the approval to land the copy: `declassified`, with the id of the copy. The copy cites the entry and the version it was made from. If the entry changed since, the approval is of another version, and a new task asks again."
    )]
    async fn declassify(
        &self,
        p: Parameters<DeclassifyParams>,
    ) -> Result<CallToolResult, McpError> {
        let p = p.0;
        let to: dm_core::Label = parse("target_label", Some(p.target_label))?
            .ok_or_else(|| McpError::invalid_params("target_label: a label", None))?;
        let answer = self
            .memory
            .declassify(&p.id, to, &p.justification)
            .await
            .map_err(mcp_err)?;
        ok(serde_json::to_value(answer).unwrap_or_default())
    }

    #[tool(
        description = "For a person, in a session that a person started with `amem serve --person`; refused in every session that a harness reaches. Approve what a task asks a person for: a proposal to verify an entry, or a sign-off task for an erasure or a copy at a lower label. The approval is of the action and the version that the task names, for the principal that asked, until `expires` or for 24 hours. The action does not run here: the requester runs it again."
    )]
    async fn approve(&self, p: Parameters<ApproveParams>) -> Result<CallToolResult, McpError> {
        let p = p.0;
        let expires = parse_time("expires", p.expires)?;
        let signed = self
            .memory
            .sign_off(&p.task, expires)
            .await
            .map_err(mcp_err)?;
        ok(serde_json::to_value(signed).unwrap_or_default())
    }

    #[tool(
        description = "The approvals on the ledger of a domain, or of one entry, oldest first: who approved which action on which version of which entry, for which principal, until when, and whether the approval is still in force (`in_force`). Only for entries that you may read. An approval holds ids and a digest and no text of an entry."
    )]
    async fn approvals(&self, p: Parameters<ApprovalsParams>) -> Result<CallToolResult, McpError> {
        let p = p.0;
        let found = self
            .memory
            .approvals(p.domain.as_deref(), p.id.as_deref())
            .await
            .map_err(mcp_err)?;
        ok(json!({ "approvals": found }))
    }

    #[tool(
        description = "Export a domain as a bundle, at or below `max_label`. The memory server keeps the bundle under its own state; you name no path and get none that you can open. The answer holds where the bundle is kept (`kept_at`), the number of entries, the highest label inside and the digest of the manifest, and no id or text of an entry. Your label rises to the highest label inside, as after a search that returned those entries. A `max_label` above what this session may handle is refused. A second export of the same domain at the same label replaces the first."
    )]
    async fn export(&self, p: Parameters<ExportParams>) -> Result<CallToolResult, McpError> {
        let p = p.0;
        let max_label: dm_core::Label = parse("max_label", Some(p.max_label))?
            .ok_or_else(|| McpError::invalid_params("max_label: a label", None))?;
        let kept = self
            .memory
            .export_kept(p.domain.as_deref(), max_label)
            .await
            .map_err(mcp_err)?;
        ok(serde_json::to_value(kept).unwrap_or_default())
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

    #[tool(
        description = "Give the verdict on the work of a task that is `blocked` and waits on `verdict:<task>`. Read its handoff first. To accept closes the task. To reject puts the task back on offer and lowers the autonomy level of its task definition one step. You cannot give the verdict on work that you handed in. While the definition is at the level `person`, only a person gives the verdict, at the command line."
    )]
    async fn verdict(&self, p: Parameters<VerdictParams>) -> Result<CallToolResult, McpError> {
        let p = p.0;
        let v = self
            .memory
            .verdict(&p.id, p.accept)
            .await
            .map_err(mcp_err)?;
        ok(serde_json::to_value(v).unwrap_or_default())
    }

    #[tool(
        description = "The autonomy level of a task definition: `level` is what a completion under the version in force needs before it counts (`person`, `review` or `checks`), `ceiling` is the highest level that the text of the definition lets it earn, and `records` are its graded runs, raises and work outcomes."
    )]
    async fn autonomy(&self, p: Parameters<DefinitionParams>) -> Result<CallToolResult, McpError> {
        let view = self
            .memory
            .autonomy(&p.0.definition)
            .await
            .map_err(mcp_err)?;
        ok(serde_json::to_value(view).unwrap_or_default())
    }

    #[tool(
        description = "Grade handoffs against the held-out suite of a task definition, for one level. Only the evaluator grades. Each answer names a scenario of the suite and the handoff that answers it. You cannot grade a handoff that you wrote. The run is recorded on the definition, passed or not."
    )]
    async fn grade(&self, p: Parameters<GradeParams>) -> Result<CallToolResult, McpError> {
        let p = p.0;
        let level: dm_ledger::Autonomy = parse("level", Some(p.level))?
            .ok_or_else(|| McpError::invalid_params("level: a level", None))?;
        let answers: Vec<crate::held_out::Answer> = p
            .answers
            .into_iter()
            .map(|a| crate::held_out::Answer {
                scenario: a.scenario,
                handoff: a.handoff,
            })
            .collect();
        let graded = self
            .memory
            .grade(&p.definition, level, &answers)
            .await
            .map_err(mcp_err)?;
        ok(serde_json::to_value(graded).unwrap_or_default())
    }

    #[tool(
        description = "Raise the autonomy level of a task definition from `person` to `review`, on its newest graded run for `review`. The run must have passed on the suite in force, and no work outcome may be newer. Only the evaluator calls this. The step to `checks` is the sign-off of a person at the command line, and this tool refuses it."
    )]
    async fn raise_autonomy(
        &self,
        p: Parameters<DefinitionParams>,
    ) -> Result<CallToolResult, McpError> {
        let record = self.memory.raise(&p.0.definition).await.map_err(mcp_err)?;
        ok(serde_json::to_value(record).unwrap_or_default())
    }

    #[tool(
        description = "The gates that an entry of the platform needs before a person can verify it: `performance`, `safety` and `review`, each with `needed`, `met` and `why`, and `reach`, which says what the entry would change. Call it before you propose a candidate for verified."
    )]
    async fn candidate_gates(
        &self,
        p: Parameters<EntryParams>,
    ) -> Result<CallToolResult, McpError> {
        let gates = self
            .memory
            .candidate_gates(&p.0.id)
            .await
            .map_err(mcp_err)?;
        ok(serde_json::to_value(gates).unwrap_or_default())
    }

    #[tool(
        description = "The metric table of the paper for a domain, over the last `days` days: 21 rows. A row has `measured`, a `value` and the log it comes `from`, or `why_not` says why no log answers it. Read `Share of throughput spent on upkeep` and `Induced work` before you file work of your own. The cost of a completed task is not measured here: `amem cost` gives it at the command line. The four rows from the read log have no value here: only a person reads them, at the command line."
    )]
    async fn metrics(&self, p: Parameters<MetricsParams>) -> Result<CallToolResult, McpError> {
        let p = p.0;
        let table = self
            .memory
            .metrics(p.domain.as_deref(), p.days.unwrap_or(7), None)
            .await
            .map_err(mcp_err)?;
        ok(serde_json::to_value(table).unwrap_or_default())
    }

    #[tool(
        description = "The five limits of a domain as they stand, each in its own unit: spend, your share of throughput, the outbound rate, the enforced rules, and the share of upkeep. A row has the count `now`, the `cap` in force, and `raised_by` when an approval raised it. A refusal with the action `limit` names one of them. Do not work around a limit: only a person raises one, at the command line."
    )]
    async fn limits(&self, p: Parameters<DomainParams>) -> Result<CallToolResult, McpError> {
        let rows = self
            .memory
            .limits(p.0.domain.as_deref())
            .await
            .map_err(mcp_err)?;
        ok(serde_json::to_value(rows).unwrap_or_default())
    }

    #[tool(
        description = "The times of one task from the ledger: its first claim, its completion, when it was queued for landing, and the seconds from the claim to the completion and from the claim to the landing."
    )]
    async fn task_times(&self, p: Parameters<TaskTimesParams>) -> Result<CallToolResult, McpError> {
        let times = self.memory.task_times(&p.0.task).await.map_err(mcp_err)?;
        ok(serde_json::to_value(times).unwrap_or_default())
    }

    #[tool(
        description = "A landing queue as it stands: the changes that wait, in landing order with the changes of people first, the share of the fleet in 24 hours, and how many changes of agents landed in the last 24 hours. Read it before you queue a change, to see how long the queue is."
    )]
    async fn landing(&self, p: Parameters<QueueParams>) -> Result<CallToolResult, McpError> {
        let view = self.memory.landing(&p.0.queue).await.map_err(mcp_err)?;
        ok(serde_json::to_value(view).unwrap_or_default())
    }

    #[tool(
        description = "Hand the change of a task that you hold to a landing queue, in place of `complete`. Write the handoff first with `write`, entry_type handoff. The task comes back `blocked`, waiting on `landing:<queue>`: your claim is over, and the task is done when a person lands the change. Do not complete it and do not queue it again."
    )]
    async fn queue_landing(
        &self,
        p: Parameters<QueueLandingParams>,
    ) -> Result<CallToolResult, McpError> {
        let p = p.0;
        let t = self
            .memory
            .queue_landing(&p.task, p.generation, &p.queue, &p.handoff)
            .await
            .map_err(mcp_err)?;
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

    #[tool(
        description = "One position: its charter and authority envelope, its open commitments (the open tasks pinned to it), its handoff cache, and the sessions that hold a task from it. `seat` is present when this session fills the position, and says whether it is seconded."
    )]
    async fn position(&self, p: Parameters<PositionParams>) -> Result<CallToolResult, McpError> {
        let p = p.0;
        let view = self
            .memory
            .position(&p.name, p.domain.as_deref())
            .await
            .map_err(mcp_err)?;
        ok(json!(view))
    }

    #[tool(
        description = "The positions of a domain, newest first: for each, whether it is active or retired, how many open tasks are pinned to it, which of them wait, the sessions live in it, and whether it needs a session started. An entry that is not in force says why."
    )]
    async fn positions(&self, p: Parameters<PositionsParams>) -> Result<CallToolResult, McpError> {
        let listed = self
            .memory
            .positions(p.0.domain.as_deref())
            .await
            .map_err(mcp_err)?;
        ok(json!({ "positions": listed }))
    }

    #[tool(
        description = "The settings of this session, which narrow what it reads: memory access, the disclosure depth, prefetch, the T0 contents, the highest trust tier, the snapshot, and transcript replay. `arm` names the evaluation arm with these settings, or is null. A launcher sets them once; no tool changes them."
    )]
    async fn settings(&self) -> Result<CallToolResult, McpError> {
        let settings = self.memory.settings();
        ok(json!({ "arm": crate::arms::Arm::of(&settings), "settings": settings }))
    }

    #[tool(
        description = "The scorecards of evaluations that the record keeps, newest first. A scorecard compares the evaluation arms with a baseline arm: for each arm the paired differences in success and in cost, the verdict, the replay traps, and the drift window that the runs spanned. Served under this session's read filter."
    )]
    async fn scorecards(
        &self,
        p: Parameters<ScorecardsParams>,
    ) -> Result<CallToolResult, McpError> {
        let limit = p.0.limit.unwrap_or(10).clamp(1, MAX_LIMIT);
        let kept = self.memory.scorecards(limit).await.map_err(mcp_err)?;
        ok(json!({ "scorecards": kept }))
    }

    #[tool(
        description = "Score an evaluation, as the owner of the record. `plan` is the TOML text of the plan that the runs started under. Each outcome names the session of one run and says whether the run succeeded, what it cost in tokens, and which judge decided. The task, the seed, the arm, the harness and the model come from the tape of the session. With `keep`, the scorecard is also written as a decision entry in the platform's domain."
    )]
    async fn score_evaluation(
        &self,
        p: Parameters<ScoreEvaluationParams>,
    ) -> Result<CallToolResult, McpError> {
        let p = p.0;
        let plan = crate::evaluation::Plan::parse(&p.plan).map_err(mcp_err)?;
        let outcomes: Vec<crate::evaluation::Outcome> = p
            .outcomes
            .into_iter()
            .map(|o| crate::evaluation::Outcome {
                session: o.session,
                success: o.success,
                cost: o.cost,
                judge: o.judge,
            })
            .collect();
        let scorecard = self.memory.score(&plan, &outcomes).await.map_err(mcp_err)?;
        let entry = if p.keep {
            let kept = self.memory.keep_scorecard(&scorecard).await;
            Some(kept.map_err(mcp_err)?.id)
        } else {
            None
        };
        ok(json!({ "scorecard": scorecard, "entry": entry }))
    }

    #[tool(
        description = "Retire a position, as the owner of the record: its tasks lose their pin and return to matching, and its history stays under its name. `why` is words on one line."
    )]
    async fn retire_position(
        &self,
        p: Parameters<RetirePositionParams>,
    ) -> Result<CallToolResult, McpError> {
        let p = p.0;
        let retired = self
            .memory
            .retire_position(&p.name, &p.why, p.domain.as_deref())
            .await
            .map_err(mcp_err)?;
        ok(json!(retired))
    }
}

const INSTRUCTIONS: &str = "This is your memory. Start by loading T0 for your domain (the `t0` tool or the memory://t0/<domain> resource). \
Search before writing a fact that may exist. `get` takes an address: an id, `<id>#<anchor>` for one section, `<id>@<version>` for one version; cite a version when the exact text matters. When you stop, write a `handoff` entry; the next session starts from it, not from your transcript. \
Work comes from the ledger: `claim_next`, heartbeat before the lease runs out, `complete` with your handoff.";

fn t0_uri(domain: &str) -> String {
    format!("memory://t0/{domain}")
}

impl McpServer {
    /// One call through the router; a harness's call is kept on the session's tape, and in
    /// a re-run answered from the tape first. A harness's call is asked of the session's
    /// settings before it is served; the launcher's own calls are not, and get the empty
    /// answers the read filter gives.
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
            return self.admitted(&tool, request, context).await;
        }
        let result = self.admitted(&tool, request, context).await;
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

impl McpServer {
    /// A harness's call, served when the session's settings admit the tool. A refusal
    /// names the setting, and is the call's result like any other error.
    async fn admitted(
        &self,
        tool: &str,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        self.memory.admits_tool(tool).map_err(mcp_err)?;
        let tcc = ToolCallContext::new(self, request, context);
        self.tool_router.call(tcc).await
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

/// Serve one memory over stdio as the session of a person, with no harness behind it
/// ([`McpServer::for_person`]), until the client goes away.
///
/// # Errors
///
/// When the MCP handshake fails or the service task panics.
pub async fn serve_stdio_for_person(memory: Arc<Memory>) -> Result<(), String> {
    let server = McpServer::for_person(memory);
    serve_with(server, tokio::io::stdin(), tokio::io::stdout()).await
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
            "erase",
            "declassify",
            "position",
            "positions",
            "retire_position",
            "settings",
            "scorecards",
            "score_evaluation",
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
            "verdict",
            "autonomy",
            "grade",
            "raise_autonomy",
            "candidate_gates",
            "metrics",
            "limits",
            "task_times",
            "landing",
            "queue_landing",
            "approve",
            "approvals",
            "export",
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

    /// The two gated actions and the approval are tools, and a re-run captures each as
    /// it captures a write. `approvals` only reads. Who may approve is held by the tests
    /// of the module `surface`: a session that a harness reaches approves nothing.
    #[test]
    fn the_gated_actions_and_the_approval_are_tools_that_a_re_run_captures() {
        let router = McpServer::tool_router();
        let tools = router.list_all();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        for gated in ["erase", "declassify", "approve", "export"] {
            assert!(names.contains(&gated), "missing tool {gated}: {names:?}");
            assert_eq!(kind_of(gated, &json!({})), tape::Kind::Mutation, "{gated}");
        }
        assert!(names.contains(&"approvals"), "{names:?}");
        assert_eq!(kind_of("approvals", &json!({})), tape::Kind::Read);
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
        // a served session approves nothing: the owner approves in a session of its own
        let owner = crate::Memory::open(s.memory.root()).unwrap();
        owner.approve(&task.id).await.unwrap();
        s.memory.reload_policy().await.unwrap();
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

    const AGENT: &str = "agent:x";

    /// A server over the session of an agent, and the owner in person on the same root.
    fn agent_and_owner() -> (tempfile::TempDir, McpServer, Memory) {
        let dir = tempfile::tempdir().unwrap();
        let owner = Memory::init(dir.path(), "human:test:me", "dom-me").unwrap();
        let agent = Memory::open(dir.path()).unwrap().with_author(AGENT);
        (dir, McpServer::new(Arc::new(agent)), owner)
    }

    /// An entry of the owner at a label.
    async fn entry_at(owner: &Memory, label: dm_core::Label, body: &str) -> String {
        let req = WriteRequest {
            title: Some("Door code".into()),
            body: body.into(),
            classification: Some(label),
            ..WriteRequest::default()
        };
        owner.write(req).await.unwrap().id
    }

    /// A refusal of a tool: its message, which is in the class of a bad request.
    fn refusal_text(answer: Result<CallToolResult, McpError>) -> String {
        let invalid = McpError::invalid_params("", None).code;
        match answer {
            Err(e) if e.code == invalid => e.message.to_string(),
            other => panic!("expected a refusal: {other:?}"),
        }
    }

    /// An erasure that an agent asks for over the memory server is refused, and the
    /// refusal names the task that asks the owner. After the owner signed that task off
    /// in person, the same call erases.
    #[tokio::test]
    async fn erase_over_mcp_waits_for_the_owner_s_approval_and_then_gives_the_receipt() {
        let (_dir, s, owner) = agent_and_owner();
        let id = entry_at(&owner, dm_core::Label::Internal, "The door code is 4417.").await;
        let erase = |reason: &str| {
            s.erase(Parameters(EraseParams {
                id: id.clone(),
                reason: reason.into(),
            }))
        };
        let bad = refusal_text(erase("because").await);
        assert!(bad.contains("reason: reason \"because\" is not"), "{bad}");
        let why = refusal_text(erase("mistaken").await);
        let named = why
            .split_once("Task ")
            .and_then(|(_, rest)| rest.split_once(" asks human:test:me for it."));
        let Some((task, _)) = named else {
            panic!("the refusal names no task: {why}")
        };
        // the reason that did not parse asked nobody: this is the one request
        let asked = owner.tasks(None, dm_ledger::Status::Open).await.unwrap();
        let asked: Vec<&str> = asked.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(asked, [task]);
        assert!(
            owner.get(&id).await.is_ok(),
            "a refused erasure erases nothing"
        );
        owner.sign_off(task, None).await.unwrap();
        let receipt = payload(&erase("mistaken").await.unwrap());
        assert_eq!(receipt["id"], id.as_str());
        assert_eq!(receipt["erased_by"], AGENT);
        assert_eq!(receipt["reason"], "mistaken");
        assert!(matches!(owner.get(&id).await, Err(Error::Gone(_))));
    }

    fn copy_at(id: &str, label: &str) -> Parameters<DeclassifyParams> {
        Parameters(DeclassifyParams {
            id: id.into(),
            target_label: label.into(),
            justification: "Every visitor gets the code at the door.".into(),
        })
    }

    /// A declassification that an agent asks for over the memory server is `proposed`,
    /// with the task for the guard. After the guard signed that task off in person, the
    /// same call lands the copy at the label that the call named.
    #[tokio::test]
    async fn declassify_over_mcp_is_proposed_and_lands_after_the_guard_s_approval() {
        let (_dir, s, owner) = agent_and_owner();
        let id = entry_at(&owner, dm_core::Label::Restricted, "The door code is 4417.").await;
        let bad = refusal_text(s.declassify(copy_at(&id, "secret")).await);
        assert!(bad.contains("target_label: "), "{bad}");
        let asked = payload(&s.declassify(copy_at(&id, "internal")).await.unwrap());
        assert_eq!(asked["outcome"], "proposed", "{asked}");
        let title = format!("Approve: copy of {id} at internal");
        assert_eq!(asked["task"]["title"], title.as_str());
        assert_eq!(asked["task"]["created_by"], AGENT);
        let task = asked["task"]["id"].as_str().unwrap();
        owner.sign_off(task, None).await.unwrap();
        let landed = payload(&s.declassify(copy_at(&id, "internal")).await.unwrap());
        assert_eq!(landed["outcome"], "declassified", "{landed}");
        assert_eq!(landed["classification"], "internal");
        let cites = landed["cites"].as_str().unwrap();
        assert!(cites.starts_with(&format!("{id}@")), "{cites}");
        let copy = owner.get(landed["id"].as_str().unwrap()).await.unwrap();
        assert_eq!(copy.front.classification, dm_core::Label::Internal);
        assert_eq!(copy.front.cites, [cites]);
        // the approval was of a copy at internal: a copy at public is another request
        let lower = payload(&s.declassify(copy_at(&id, "public")).await.unwrap());
        assert_eq!(lower["outcome"], "proposed", "{lower}");
        let title = format!("Approve: copy of {id} at public");
        assert_eq!(lower["task"]["title"], title.as_str());
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
        // a pin the caller got wrong, or does not fill, is the caller's to act on
        let pinned = mcp_err(Error::Ledger(dm_ledger::Error::Pinned {
            id: "t".into(),
            position: "pos-a-qa".into(),
        }));
        assert_eq!(pinned.code, invalid.code);
        assert!(pinned.message.contains("pinned to position pos-a-qa"));
        let bad_pin = mcp_err(Error::Ledger(dm_ledger::Error::BadPin("no reason".into())));
        assert_eq!(bad_pin.code, invalid.code);
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

    const RELEASE: &str = "pos-release";

    /// Write the position `pos-release`, which `codex` fills, through the `write` tool,
    /// and leave it as a person other than its proposer would: verified.
    async fn release_position(s: &McpServer) -> String {
        let block = "```position pos-release\n[charter]\norders = \"Each release is cut from main.\"\ncovers = []\nagent = \"codex\"\n\n[envelope]\ntools = []\nhandling = \"restricted\"\n```\n";
        let written = payload(
            &s.write(Parameters(WriteParams {
                body: format!("# The release coordinator\n\n{block}"),
                entry_type: Some("position".into()),
                ..WriteParams::default()
            }))
            .await
            .unwrap(),
        );
        let id = written["id"].as_str().unwrap().to_string();
        let mut e = s.memory.store.get("dom-me", &id).await.unwrap().unwrap();
        e.front.trust_tier = dm_core::TrustTier::Verified;
        s.memory.store.put(&e).await.unwrap();
        id
    }

    fn pinned(title: &str, position: Option<&str>, reason: Option<&str>) -> CreateTaskParams {
        CreateTaskParams {
            title: title.into(),
            body: "Do it.".into(),
            position: position.map(str::to_string),
            pin_reason: reason.map(str::to_string),
            ..CreateTaskParams::default()
        }
    }

    /// File a task pinned to `pos-release` for continuity, and admit it.
    async fn admitted_pinned(s: &McpServer, title: &str) -> String {
        let filed = s
            .create_task(Parameters(pinned(title, Some(RELEASE), Some("continuity"))))
            .await
            .unwrap();
        let filed = payload(&filed);
        assert_eq!(filed["task"]["position"], RELEASE, "{filed}");
        assert_eq!(filed["task"]["pin_reason"], "continuity", "{filed}");
        let id = filed["task"]["id"].as_str().unwrap().to_string();
        s.admit_task(Parameters(TaskIdParams { id: id.clone() }))
            .await
            .unwrap();
        id
    }

    fn refusal<T>(r: Result<T, McpError>) -> McpError {
        match r {
            Err(e) => e,
            Ok(_) => panic!("it was allowed"),
        }
    }

    #[tokio::test]
    async fn create_task_refuses_half_a_pin_and_an_unknown_reason_as_invalid_params() {
        let (_dir, s) = server().await;
        release_position(&s).await;
        let invalid = McpError::invalid_params("x", None).code;
        let no_reason = refusal(
            s.create_task(Parameters(pinned("Announce", Some(RELEASE), None)))
                .await,
        );
        assert_eq!(no_reason.code, invalid);
        assert!(
            no_reason
                .message
                .contains("a pin names a position and gives a reason"),
            "{}",
            no_reason.message
        );
        let unknown = refusal(
            s.create_task(Parameters(pinned("Announce", Some(RELEASE), Some("habit"))))
                .await,
        );
        assert_eq!(unknown.code, invalid);
        assert!(
            unknown.message.starts_with("pin_reason:") && unknown.message.contains("\"habit\""),
            "{}",
            unknown.message
        );
        let none = payload(
            &s.list_tasks(Parameters(ListTasksParams {
                status: Some("proposed".into()),
                ..ListTasksParams::default()
            }))
            .await
            .unwrap(),
        );
        assert_eq!(none["tasks"], json!([]), "a refused task is not filed");
    }

    #[tokio::test]
    async fn a_pinned_task_is_refused_to_a_claim_from_outside_its_position() {
        let (_dir, s) = server().await;
        release_position(&s).await;
        let id = admitted_pinned(&s, "Announce release 1.4").await;
        let refused = refusal(
            s.claim(Parameters(LeaseParams {
                id: id.clone(),
                lease_seconds: Some(60),
            }))
            .await,
        );
        assert_eq!(refused.code, McpError::invalid_params("x", None).code);
        assert!(
            refused.message.contains("pinned to position pos-release"),
            "{}",
            refused.message
        );
        let next = payload(
            &s.claim_next(Parameters(ClaimNextParams::default()))
                .await
                .unwrap(),
        );
        assert!(next["task"].is_null(), "{next}");
    }

    #[tokio::test]
    async fn positions_lists_a_verified_position_with_what_waits_on_it() {
        let (_dir, s) = server().await;
        let entry = release_position(&s).await;
        let id = admitted_pinned(&s, "Announce release 1.4").await;
        let listed = payload(
            &s.positions(Parameters(PositionsParams::default()))
                .await
                .unwrap(),
        );
        let row = &listed["positions"][0];
        assert_eq!(listed["positions"].as_array().map(Vec::len), Some(1));
        assert_eq!(row["name"], RELEASE, "{listed}");
        assert_eq!(row["entry"], entry, "{listed}");
        assert_eq!(row["state"], "active", "{listed}");
        assert_eq!(row["agent"], "codex", "{listed}");
        assert_eq!(row["pinned"], 1, "{listed}");
        assert_eq!(row["waiting"], json!([id]), "{listed}");
        assert_eq!(row["wake"], true, "{listed}");
        // both tools only read the record: a re-run that diverged still serves them
        assert_eq!(kind_of("position", &json!({})), tape::Kind::Read);
        assert_eq!(kind_of("positions", &json!({})), tape::Kind::Read);
        assert_eq!(kind_of("retire_position", &json!({})), tape::Kind::Mutation);
    }

    #[tokio::test]
    async fn position_shows_a_verified_position_with_its_commitments() {
        let (_dir, s) = server().await;
        let entry = release_position(&s).await;
        let id = admitted_pinned(&s, "Announce release 1.4").await;
        let view = payload(
            &s.position(Parameters(PositionParams {
                name: RELEASE.into(),
                domain: Some("dom-me".into()),
            }))
            .await
            .unwrap(),
        );
        assert_eq!(view["name"], RELEASE, "{view}");
        assert_eq!(view["entry"], entry, "{view}");
        assert_eq!(view["charter"]["agent"], "codex", "{view}");
        assert_eq!(view["envelope"]["handling"], "restricted", "{view}");
        assert_eq!(view["commitments"][0]["id"], id, "{view}");
        assert_eq!(view["handoffs"], json!([]), "{view}");
        assert!(view.get("seat").is_none(), "{view}");
        let missing = refusal(
            s.position(Parameters(PositionParams {
                name: "pos-none".into(),
                domain: None,
            }))
            .await,
        );
        assert!(missing.message.contains("pos-none"), "{}", missing.message);
    }

    #[tokio::test]
    async fn retire_position_is_the_owner_s_and_unpins_the_tasks() {
        let (dir, s) = server().await;
        release_position(&s).await;
        let id = admitted_pinned(&s, "Announce release 1.4").await;
        let agent = Memory::open(dir.path())
            .unwrap()
            .with_author("agent:codex (model: test-1)");
        let agent = McpServer::new(Arc::new(agent));
        let retire = || {
            Parameters(RetirePositionParams {
                name: RELEASE.into(),
                why: "the release train ended".into(),
                domain: None,
            })
        };
        let refused = refusal(agent.retire_position(retire()).await);
        assert_eq!(refused.code, McpError::invalid_params("x", None).code);
        assert!(
            refused.message.contains("only the owner"),
            "{}",
            refused.message
        );
        let retired = payload(&s.retire_position(retire()).await.unwrap());
        assert_eq!(retired["position"], RELEASE, "{retired}");
        assert_eq!(retired["unpinned"], json!([id]), "{retired}");
        let listed = payload(
            &s.positions(Parameters(PositionsParams::default()))
                .await
                .unwrap(),
        );
        assert_eq!(listed["positions"][0]["state"], "retired", "{listed}");
        assert_eq!(listed["positions"][0]["entry"], retired["retired_by"]);
        // the task returned to matching: a session in no position claims it
        let claimed = payload(
            &agent
                .claim(Parameters(LeaseParams {
                    id: id.clone(),
                    lease_seconds: Some(60),
                }))
                .await
                .unwrap(),
        );
        assert_eq!(claimed["task"]["id"], id, "{claimed}");
        assert!(claimed["task"].get("position").is_none(), "{claimed}");
    }

    const PLAN: &str = "preamble = \"Read the record. \"\n[plan]\nname = \"demo\"\ndomain = \"dom-me\"\nbaseline = \"no-memory\"\narms = [\"no-memory\", \"full\"]\nseeds = 2\nmargin = 0.1\njudge = \"regex\"\n[[task]]\nid = \"a\"\nprompt = \"What is A?\"\nexpect = \"A\"\n";

    /// One run of the plan that keeps the head of its tape, as an outcome.
    async fn evaluation_run(
        dir: &std::path::Path,
        seed: u32,
        arm: crate::arms::Arm,
    ) -> OutcomeParams {
        let plan = crate::evaluation::Plan::parse(PLAN).unwrap();
        let m = Memory::open(dir).unwrap().with_author("agent:claude");
        let launched = crate::evaluation::Launched {
            harness: "claude",
            model: None,
        };
        let as_of = time::macros::datetime!(2020-01-01 00:00:00 UTC);
        m.start_evaluation(&plan, ("a", seed, arm), as_of, launched)
            .unwrap();
        m.tape_start(tape::TapeStart {
            harness: "claude".into(),
            prompt: plan.prompt_of("a"),
            ..tape::TapeStart::default()
        })
        .await
        .unwrap();
        OutcomeParams {
            session: m.session_id().to_string(),
            success: true,
            cost: if arm == crate::arms::Arm::Full {
                600
            } else {
                1000
            },
            judge: "regex".into(),
        }
    }

    #[tokio::test]
    async fn score_evaluation_scores_and_keeps_and_scorecards_reads_it_back() {
        let (dir, _) = server().await;
        let path = dir.path().join(crate::GRANTS_FILE);
        let mut g = dm_policy::Grants::load(&path).unwrap();
        g.platform = Some("dom-me".into());
        std::fs::write(&path, g.render().unwrap()).unwrap();
        let s = McpServer::new(Arc::new(Memory::open(dir.path()).unwrap()));
        let none = payload(
            &s.scorecards(Parameters(ScorecardsParams::default()))
                .await
                .unwrap(),
        );
        assert_eq!(none, json!({ "scorecards": [] }));
        let mut outcomes = Vec::new();
        for seed in [0, 1] {
            outcomes.push(evaluation_run(dir.path(), seed, crate::arms::Arm::NoMemory).await);
            outcomes.push(evaluation_run(dir.path(), seed, crate::arms::Arm::Full).await);
        }
        let params = |plan: &str, outcomes: Vec<OutcomeParams>, keep: bool| {
            Parameters(ScoreEvaluationParams {
                plan: plan.into(),
                outcomes,
                keep,
            })
        };
        // a plan that is refused, and an evaluation that is refused, say why
        let no_plan = refusal(
            s.score_evaluation(params("preamble = \"x\"", Vec::new(), false))
                .await,
        );
        assert!(
            no_plan.message.contains("[plan] table"),
            "{}",
            no_plan.message
        );
        let no_outcome = refusal(s.score_evaluation(params(PLAN, Vec::new(), false)).await);
        assert!(
            no_outcome.message.contains("one outcome or more"),
            "{}",
            no_outcome.message
        );
        let scored = payload(
            &s.score_evaluation(params(PLAN, outcomes, true))
                .await
                .unwrap(),
        );
        assert_eq!(scored["scorecard"]["plan"], "demo", "{scored}");
        assert_eq!(scored["scorecard"]["arms"][1]["pairs"], 2, "{scored}");
        assert_eq!(
            scored["scorecard"]["arms"][1]["finding"], "pays",
            "{scored}"
        );
        // one task, with no task that failed and no frozen task: two traps are open
        assert_eq!(
            scored["scorecard"]["arms"][1]["verdict"]["inconclusive"]["traps"],
            json!(["survivorship", "distribution-shift"]),
            "{scored}"
        );
        let entry = scored["entry"].as_str().expect("kept").to_string();
        let kept = payload(
            &s.scorecards(Parameters(ScorecardsParams { limit: Some(5) }))
                .await
                .unwrap(),
        );
        assert_eq!(kept["scorecards"][0]["entry"], entry, "{kept}");
        assert_eq!(kept["scorecards"][0]["scorecard"], scored["scorecard"]);
        // scoring is the owner's; a re-run that diverged still reads the scorecards
        let agent = Memory::open(dir.path())
            .unwrap()
            .with_author("agent:claude");
        let agent = McpServer::new(Arc::new(agent));
        let refused = refusal(
            agent
                .score_evaluation(params(PLAN, Vec::new(), false))
                .await,
        );
        assert!(
            refused.message.contains("the owner's"),
            "{}",
            refused.message
        );
        let none = json!({});
        assert_eq!(kind_of("scorecards", &none), tape::Kind::Read);
        assert_eq!(kind_of("score_evaluation", &none), tape::Kind::Mutation);
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

    /// The autonomy level, the grade and the gates over the memory server.
    mod levels {
        use super::*;
        use crate::testing::{as_author, held_out_root, written, written_in, EVALUATOR};
        use dm_ledger::{Autonomy, DefinitionChange, Status};
        use std::path::Path;

        const OWNER: &str = "human:test:me";

        /// A definition whose text lets it earn the highest level.
        const TIDY: &str = "# Tidy a module\n\n```platform task tidy\npurpose = \"Tidy one module.\"\nacceptance = [\"The handoff names each file that changed.\"]\noutputs = [\"handoff\"]\nautonomy = \"checks\"\n```\n";

        /// One scenario, and a pass mark of one for `review` and for `checks`.
        const SUITE: &str = "# The suite for tidy\n\n```platform suite tidy\n[pass]\nreview = 1\nchecks = 1\n\n[[scenario]]\nname = \"files question\"\nask = \"Which files changed?\"\nexpect = [\"src/importer\"]\n```\n";

        /// A root with a held-out domain, and the definition `tidy` and its suite, both
        /// verified: the owner in person, and the entry of the definition.
        async fn tidy_root(dir: &Path) -> (Memory, String) {
            let owner = held_out_root(dir);
            let entry = written(&owner, TIDY, true).await;
            written_in(&owner, "dom-held", SUITE, true).await;
            (owner, entry)
        }

        /// A server over a new session of `author`.
        fn server_of(dir: &Path, author: &str) -> McpServer {
            McpServer::new(Arc::new(as_author(dir, author)))
        }

        /// A handoff in `dom-me` that passes the scenario of the suite.
        async fn passing_handoff(s: &McpServer) -> String {
            let h = WriteParams {
                domain: Some("dom-me".into()),
                ..handoff("Changed src/importer.rs.\n").0
            };
            let w = payload(&s.write(Parameters(h)).await.unwrap());
            w["id"].as_str().unwrap().to_string()
        }

        /// A task under `tidy` that `worker` claimed and completed over the memory
        /// server: the task and its handoff. The completion parks the task.
        async fn handed_in(owner: &Memory, dir: &Path, worker: &str) -> (String, String) {
            let spec = dm_ledger::TaskSpec {
                domain: "dom-me".into(),
                title: "Tidy the importer".into(),
                body: "Do it.\n".into(),
                definition: Some("tidy".into()),
                ..dm_ledger::TaskSpec::default()
            };
            let task = owner.create_task(spec).await.unwrap().task;
            if task.status == Status::Proposed {
                owner.admit(&task.id).await.unwrap();
            }
            let s = server_of(dir, worker);
            let lease = LeaseParams {
                id: task.id.clone(),
                lease_seconds: None,
            };
            s.claim(Parameters(lease)).await.unwrap();
            let h = passing_handoff(&s).await;
            let done = CompleteParams {
                id: task.id.clone(),
                handoff: h.clone(),
                ..CompleteParams::default()
            };
            let parked = payload(&s.complete(Parameters(done)).await.unwrap());
            assert_eq!(parked["task"]["status"], "blocked", "{parked}");
            let event = format!("verdict:{}", task.id);
            assert_eq!(parked["task"]["waiting_on"], event.as_str(), "{parked}");
            (task.id, h)
        }

        fn accept(task: &str) -> Parameters<VerdictParams> {
            Parameters(VerdictParams {
                id: task.into(),
                accept: true,
            })
        }

        fn tidy() -> Parameters<DefinitionParams> {
            Parameters(DefinitionParams {
                definition: "tidy".into(),
            })
        }

        fn run(level: &str, handoff: &str) -> Parameters<GradeParams> {
            Parameters(GradeParams {
                definition: "tidy".into(),
                level: level.into(),
                answers: vec![GradeAnswer {
                    scenario: "files question".into(),
                    handoff: handoff.into(),
                }],
            })
        }

        /// At the level `person`, the verdict is a person's in person. The memory
        /// server serves a harness, so it refuses the verdict whatever the name of the
        /// session is. The same person at the command line gives it.
        #[tokio::test]
        async fn a_verdict_at_person_is_refused_over_the_memory_server_whatever_the_name() {
            let dir = tempfile::tempdir().unwrap();
            let (owner, _) = tidy_root(dir.path()).await;
            let (task, _) = handed_in(&owner, dir.path(), "agent:w").await;
            for name in [OWNER, "agent:r"] {
                let why = refusal_text(server_of(dir.path(), name).verdict(accept(&task)).await);
                let said = "only a person gives the verdict on work at the level person";
                assert!(why.contains(said), "{name}: {why}");
            }
            let waits = owner.get_task(&task).await.unwrap();
            assert_eq!(waits.status, Status::Blocked);
            let given = owner.verdict(&task, true).await.unwrap();
            assert_eq!(given.task.status, Status::Done);
        }

        /// At the level `review`, the worker's own verdict is refused, and the verdict
        /// of another agent over the memory server closes the task.
        #[tokio::test]
        async fn a_verdict_at_review_from_another_agent_closes_the_task() {
            let dir = tempfile::tempdir().unwrap();
            let (owner, _) = tidy_root(dir.path()).await;
            let reference = owner.autonomy("tidy").await.unwrap().definition;
            let raised = DefinitionChange::Raised {
                to: Autonomy::Review,
                run: "def-plat-test".into(),
                signed_by: None,
            };
            {
                let mut l = owner.ledger.lock().await;
                l.record_definition("dom-plat", &reference, raised)
                    .await
                    .unwrap();
            }
            let (task, h) = handed_in(&owner, dir.path(), "agent:w").await;
            let own = refusal_text(
                server_of(dir.path(), "agent:w")
                    .verdict(accept(&task))
                    .await,
            );
            assert!(
                own.contains("nobody gives the verdict on its own work"),
                "{own}"
            );
            let reviewer = server_of(dir.path(), "agent:r");
            let given = payload(&reviewer.verdict(accept(&task)).await.unwrap());
            assert_eq!(given["accepted"], true, "{given}");
            assert_eq!(given["level"], "review", "{given}");
            assert_eq!(given["task"]["status"], "done", "{given}");
            assert_eq!(given["task"]["handoff"], h.as_str(), "{given}");
            let done = owner.get_task(&task).await.unwrap();
            assert_eq!(done.status, Status::Done);
        }

        /// Only the evaluator grades over the memory server. The level is one of the
        /// three names. The run that the tool gives is the record on the ledger.
        #[tokio::test]
        async fn grade_over_the_memory_server_is_the_evaluator_s_and_records_the_run() {
            let dir = tempfile::tempdir().unwrap();
            let (owner, _) = tidy_root(dir.path()).await;
            let h = passing_handoff(&server_of(dir.path(), "agent:w")).await;
            let other = refusal_text(
                server_of(dir.path(), "agent:x")
                    .grade(run("review", &h))
                    .await,
            );
            assert!(
                other.contains("only the evaluator grades a task definition"),
                "{other}"
            );
            let evaluator = server_of(dir.path(), EVALUATOR);
            let bad = refusal_text(evaluator.grade(run("expert", &h)).await);
            assert!(
                bad.contains("level: \"expert\" is not person, review or checks"),
                "{bad}"
            );
            assert_eq!(owner.autonomy("tidy").await.unwrap().records, []);
            let graded = payload(&evaluator.grade(run("review", &h)).await.unwrap());
            assert_eq!(graded["pass"], true, "{graded}");
            assert_eq!(graded["failed"], json!([]), "{graded}");
            assert_eq!(graded["record"]["change"], "graded", "{graded}");
            assert_eq!(graded["record"]["level"], "review", "{graded}");
            assert_eq!(graded["record"]["by"], EVALUATOR, "{graded}");
            let records = owner.autonomy("tidy").await.unwrap().records;
            assert_eq!(
                serde_json::to_value(&records).unwrap(),
                json!([graded["record"]])
            );
        }

        /// The evaluator takes the step to `review` over the memory server. The step
        /// to `checks` is refused there, from the evaluator and from a session with
        /// the name of the owner: the owner takes it in person.
        #[tokio::test]
        async fn raise_autonomy_takes_the_step_to_review_and_refuses_the_step_to_checks() {
            let dir = tempfile::tempdir().unwrap();
            let (owner, _) = tidy_root(dir.path()).await;
            let reference = owner.autonomy("tidy").await.unwrap().definition;
            let h = passing_handoff(&server_of(dir.path(), "agent:w")).await;
            let evaluator = server_of(dir.path(), EVALUATOR);
            evaluator.grade(run("review", &h)).await.unwrap();
            let worker = refusal_text(
                server_of(dir.path(), "agent:w")
                    .raise_autonomy(tidy())
                    .await,
            );
            assert!(
                worker.contains("or the evaluator raises a task definition to the level review"),
                "{worker}"
            );
            let raised = payload(&evaluator.raise_autonomy(tidy()).await.unwrap());
            assert_eq!(raised["change"], "raised", "{raised}");
            assert_eq!(raised["to"], "review", "{raised}");
            assert_eq!(raised["definition"], reference.as_str(), "{raised}");
            evaluator.grade(run("checks", &h)).await.unwrap();
            for name in [EVALUATOR, OWNER] {
                let why = refusal_text(server_of(dir.path(), name).raise_autonomy(tidy()).await);
                let said = "only a person signs a task definition off for the level checks";
                assert!(why.contains(said), "{name}: {why}");
            }
            let level = owner.level_of(&reference).await.unwrap();
            assert_eq!(level, Autonomy::Review);
            let signed = owner.raise("tidy").await.unwrap();
            assert!(
                matches!(&signed.change, DefinitionChange::Raised { to: Autonomy::Checks, signed_by: Some(by), .. } if by == OWNER),
                "{signed:?}"
            );
        }

        /// The two reads give what the library gives to the same principal.
        #[tokio::test]
        async fn autonomy_and_candidate_gates_give_what_the_library_gives() {
            let dir = tempfile::tempdir().unwrap();
            let (_owner, entry) = tidy_root(dir.path()).await;
            let s = server_of(dir.path(), "agent:x");
            let library = as_author(dir.path(), "agent:x");
            let view = payload(&s.autonomy(tidy()).await.unwrap());
            let same = library.autonomy("tidy").await.unwrap();
            assert_eq!(view, serde_json::to_value(&same).unwrap());
            assert_eq!(view["definition"], format!("tidy@{entry}").as_str());
            assert_eq!(view["level"], "person", "{view}");
            assert_eq!(view["ceiling"], "checks", "{view}");
            let id = Parameters(EntryParams { id: entry.clone() });
            let gates = payload(&s.candidate_gates(id).await.unwrap());
            let same = library.candidate_gates(&entry).await.unwrap();
            assert_eq!(gates, serde_json::to_value(&same).unwrap());
            assert_eq!(gates["entry"], entry.as_str(), "{gates}");
            assert_eq!(gates["review"]["needed"], true, "{gates}");
            assert_eq!(gates["review"]["met"], true, "{gates}");
        }

        /// A re-run serves the two reads from the record as it is, and captures the
        /// other three as it captures a write. A work outcome is a person's at the
        /// command line, and a list of who is flagged would tell an agent where the
        /// threshold is: no tool does either. The tools `record_outcome` and `outcomes`
        /// are for the outcome event of a task, which is on no definition and moves no
        /// level.
        #[test]
        fn the_reads_are_reads_and_no_tool_records_a_work_outcome_or_lists_probing() {
            for read in ["autonomy", "candidate_gates"] {
                assert_eq!(kind_of(read, &json!({})), tape::Kind::Read, "{read}");
            }
            for change in ["verdict", "grade", "raise_autonomy"] {
                assert_eq!(
                    kind_of(change, &json!({})),
                    tape::Kind::Mutation,
                    "{change}"
                );
            }
            let router = McpServer::tool_router();
            let tools = router.list_all();
            let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
            let of_an_outcome_event = ["record_outcome", "outcomes"];
            let neither: Vec<&&str> = names
                .iter()
                .filter(|n| n.contains("outcome") || n.contains("probing"))
                .filter(|n| !of_an_outcome_event.contains(n))
                .collect();
            assert!(neither.is_empty(), "a tool that should not be: {neither:?}");
        }
    }

    /// The metrics, the limits and the landing queue over the memory server.
    mod measurement {
        use super::*;
        use crate::testing::as_author;
        use dm_ledger::Status;
        use std::path::Path;
        use time::OffsetDateTime;

        /// A root with the domains `dom-me` and `dom-plat`, the landing queue `main` on
        /// `dom-me`, and a share of throughput and of upkeep there.
        fn site(dir: &Path) {
            crate::testing::root(dir);
            let path = dir.join(crate::GRANTS_FILE);
            let mut g = dm_policy::Grants::load(&path).unwrap();
            let queue = dm_policy::LandingQueue {
                domain: "dom-me".into(),
                per_day: Some(2),
                count_after_hours: None,
            };
            g.landing.insert("main".into(), queue);
            let limits = dm_policy::DomainLimits {
                claims_per_hour: Some(5),
                upkeep_percent: Some(50),
                ..dm_policy::DomainLimits::default()
            };
            g.limits.insert("dom-me".into(), limits);
            std::fs::write(&path, g.render().unwrap()).unwrap();
        }

        /// A new session of `author` whose ledger reads the time `now`, so that two
        /// sessions give the same numbers.
        async fn session(dir: &Path, author: &str, now: OffsetDateTime) -> Memory {
            let m = as_author(dir, author);
            m.ledger.lock().await.set_clock(move || now);
            m
        }

        async fn server_at(dir: &Path, author: &str, now: OffsetDateTime) -> McpServer {
            McpServer::new(Arc::new(session(dir, author, now).await))
        }

        /// A task of `dom-me` that the owner filed and `worker` claimed over the memory
        /// server, and a handoff of the worker: the task, the generation of the claim,
        /// and the handoff.
        async fn claimed(dir: &Path, worker: &str, now: OffsetDateTime) -> (String, u64, String) {
            let owner = session(dir, "human:test:me", now).await;
            let spec = dm_ledger::TaskSpec {
                domain: "dom-me".into(),
                title: "Change the importer".into(),
                body: "Do it.\n".into(),
                ..dm_ledger::TaskSpec::default()
            };
            let task = owner.create_task(spec).await.unwrap().task;
            if task.status == Status::Proposed {
                owner.admit(&task.id).await.unwrap();
            }
            let s = server_at(dir, worker, now).await;
            let lease = LeaseParams {
                id: task.id.clone(),
                lease_seconds: None,
            };
            let held = payload(&s.claim(Parameters(lease)).await.unwrap());
            let generation = held["task"]["generation"].as_u64().unwrap();
            let h = WriteParams {
                domain: Some("dom-me".into()),
                ..handoff("Changed src/importer.rs.\n").0
            };
            let w = payload(&s.write(Parameters(h)).await.unwrap());
            (task.id, generation, w["id"].as_str().unwrap().to_string())
        }

        fn json_of<T: serde::Serialize>(v: &T) -> Value {
            serde_json::to_value(v).unwrap()
        }

        fn queue_main(
            task: &str,
            generation: u64,
            handoff: &str,
        ) -> Parameters<QueueLandingParams> {
            Parameters(QueueLandingParams {
                task: task.into(),
                queue: "main".into(),
                handoff: handoff.into(),
                generation: Some(generation),
            })
        }

        /// Each of the five tools gives what the library gives to the same principal.
        /// The change is queued from a second session of the worker, under the
        /// generation of the claim.
        #[tokio::test]
        async fn the_five_tools_of_measurement_give_what_the_library_gives() {
            let dir = tempfile::tempdir().unwrap();
            site(dir.path());
            let now = OffsetDateTime::now_utc();
            let (task, generation, h) = claimed(dir.path(), "agent:x", now).await;
            let s = server_at(dir.path(), "agent:x", now).await;
            let library = session(dir.path(), "agent:x", now).await;

            let queued = payload(
                &s.queue_landing(queue_main(&task, generation, &h))
                    .await
                    .unwrap(),
            );
            let same = library.get_task(&task).await.unwrap();
            assert_eq!(queued["task"], json_of(&same));
            assert_eq!(queued["task"]["status"], "blocked", "{queued}");
            assert_eq!(queued["task"]["waiting_on"], "landing:main", "{queued}");
            assert_eq!(queued["task"]["handoff"], h.as_str(), "{queued}");

            let queue = Parameters(QueueParams {
                queue: "main".into(),
            });
            let view = payload(&s.landing(queue).await.unwrap());
            assert_eq!(view, json_of(&library.landing("main").await.unwrap()));
            assert_eq!(view["waiting"][0]["task"], task.as_str(), "{view}");
            assert_eq!(view["waiting"][0]["by"], "agent:x", "{view}");

            let of = Parameters(TaskTimesParams { task: task.clone() });
            let times = payload(&s.task_times(of).await.unwrap());
            assert_eq!(times, json_of(&library.task_times(&task).await.unwrap()));
            assert_eq!(times["task"], task.as_str(), "{times}");
            assert_eq!(times["queue"], "main", "{times}");

            let me = || Some("dom-me".to_string());
            let rows = payload(
                &s.limits(Parameters(DomainParams { domain: me() }))
                    .await
                    .unwrap(),
            );
            let same = library.limits(Some("dom-me")).await.unwrap();
            assert_eq!(rows, json_of(&same));
            let claims = rows
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["limit"] == "throughput");
            let claims = claims.unwrap();
            assert_eq!(claims["scope"], "agent:x", "{claims}");
            assert_eq!((&claims["now"], &claims["cap"]), (&json!(1.0), &json!(5.0)));

            let week = MetricsParams {
                domain: me(),
                days: None,
            };
            let table = payload(&s.metrics(Parameters(week)).await.unwrap());
            let same = library.metrics(Some("dom-me"), 7, None).await.unwrap();
            assert_eq!(table, json_of(&same));
            assert_eq!(
                table["window_days"], 7,
                "the window is 7 days when none is named"
            );
            assert_eq!(table["rows"].as_array().unwrap().len(), 21);
            assert_eq!(
                table["rows"][0]["measured"], false,
                "no tool reads the meter"
            );
            let three = MetricsParams {
                domain: me(),
                days: Some(3),
            };
            let table = payload(&s.metrics(Parameters(three)).await.unwrap());
            assert_eq!(table["window_days"], 3, "{table}");
        }

        /// A re-run serves the four reads from the record as it is, and captures a
        /// queued change as it captures a write. Only a person in person lands a change
        /// or raises a limit: no tool does either.
        #[test]
        fn the_reads_of_measurement_are_reads_and_no_tool_lands_or_raises() {
            for read in ["metrics", "limits", "task_times", "landing"] {
                assert_eq!(kind_of(read, &json!({})), tape::Kind::Read, "{read}");
            }
            assert_eq!(kind_of("queue_landing", &json!({})), tape::Kind::Mutation);
            let router = McpServer::tool_router();
            let tools = router.list_all();
            let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
            let neither: Vec<&&str> = names
                .iter()
                .filter(|n| {
                    **n == "land" || n.contains("raise_limit") || n.contains("limits_raise")
                })
                .collect();
            assert!(neither.is_empty(), "a tool that should not be: {neither:?}");
        }

        /// A session that does not hold the task cannot queue its change, though it
        /// names the generation of the claim. The answer is a refusal, and the task
        /// stays with its holder.
        #[tokio::test]
        async fn queue_landing_by_a_session_that_does_not_hold_the_task_is_a_refusal() {
            let dir = tempfile::tempdir().unwrap();
            site(dir.path());
            let now = OffsetDateTime::now_utc();
            let (task, generation, h) = claimed(dir.path(), "agent:x", now).await;
            let other = server_at(dir.path(), "agent:y", now).await;
            let why = refusal(other.queue_landing(queue_main(&task, generation, &h)).await).message;
            assert!(
                why.contains(&format!("task {task} is not held by agent:y")),
                "{why}"
            );
            let unnamed = Parameters(QueueLandingParams {
                generation: None,
                ..queue_main(&task, generation, &h).0
            });
            let why = refusal(other.queue_landing(unnamed).await).message;
            assert!(
                why.contains("in a session that made no claim on it"),
                "{why}"
            );
            let still = other.memory.get_task(&task).await.unwrap();
            assert_eq!(still.status, Status::Claimed);
            assert_eq!(still.holder.as_deref(), Some("agent:x"));
            assert_eq!(still.waiting_on, None);
        }
    }

    /// The surface of the paper's Appendix A: an address, filters, the size of a recall,
    /// and `approve` and `export` as tools.
    mod surface {
        use super::*;

        const DECISION: &str = "The import runs on the staging host.\n\n## Context\n\nThe import job needs a host with the loader installed.\n\n## Decision\n\nThe import job runs on the staging host every night.\n";

        fn get_at(address: &str) -> Parameters<GetParams> {
            Parameters(GetParams { id: address.into() })
        }

        async fn written(s: &McpServer, kind: &str, title: &str, body: &str) -> String {
            let p = WriteParams {
                body: body.into(),
                title: Some(title.into()),
                entry_type: Some(kind.into()),
                ..WriteParams::default()
            };
            let w = payload(&s.write(Parameters(p)).await.unwrap());
            w["id"].as_str().unwrap().to_string()
        }

        fn approve_task(task: &str) -> Parameters<ApproveParams> {
            Parameters(ApproveParams {
                task: task.into(),
                expires: None,
            })
        }

        async fn approvals_of(s: &McpServer, id: &str) -> Vec<Value> {
            let p = ApprovalsParams {
                id: Some(id.into()),
                domain: None,
            };
            let listed = payload(&s.approvals(Parameters(p)).await.unwrap());
            listed["approvals"].as_array().unwrap().clone()
        }

        /// The agent asks to erase `id`: the id of the sign-off task that the refusal
        /// names.
        async fn asked_to_erase(agent: &McpServer, id: &str) -> String {
            let p = EraseParams {
                id: id.into(),
                reason: "mistaken".into(),
            };
            let why = refusal_text(agent.erase(Parameters(p)).await);
            let named = why.split_once("Task ").and_then(|(_, r)| r.split_once(' '));
            named.map(|(task, _)| task.to_string()).expect("a task")
        }

        fn served_as(dir: &std::path::Path, author: Option<&str>) -> McpServer {
            let m = Memory::open(dir).unwrap();
            let m = match author {
                Some(a) => m.with_author(a),
                None => m,
            };
            McpServer::new(Arc::new(m))
        }

        fn person_as(dir: &std::path::Path, author: Option<&str>) -> McpServer {
            let m = Memory::open(dir).unwrap();
            let m = match author {
                Some(a) => m.with_author(a),
                None => m,
            };
            McpServer::for_person(Arc::new(m))
        }

        /// A session that a harness reaches approves nothing, the owner's author
        /// included: the task stays open and the ledger holds no approval.
        #[tokio::test]
        async fn approve_is_refused_in_every_session_that_a_harness_reaches() {
            let (dir, agent, owner) = agent_and_owner();
            let id = entry_at(&owner, dm_core::Label::Internal, "The door code is 4417.").await;
            let task = asked_to_erase(&agent, &id).await;
            let as_owner = served_as(dir.path(), None);
            for s in [&agent, &as_owner] {
                let why = refusal_text(s.approve(approve_task(&task)).await);
                assert!(
                    why.contains("a session of the memory server approves nothing"),
                    "{why}"
                );
            }
            let open = owner.tasks(None, dm_ledger::Status::Open).await.unwrap();
            let open: Vec<&str> = open.iter().map(|t| t.id.as_str()).collect();
            assert_eq!(open, [task.as_str()]);
            assert_eq!(approvals_of(&as_owner, &id).await, Vec::<Value>::new());
        }

        /// In a session of a person the owner approves, `approvals` lists the approval,
        /// and the agent's erasure then runs. An agent's author in such a session is
        /// refused as the approver by the policy.
        #[tokio::test]
        async fn approve_in_a_session_of_a_person_records_the_approval_of_the_owner_alone() {
            let (dir, agent, owner) = agent_and_owner();
            let id = entry_at(&owner, dm_core::Label::Internal, "The door code is 4417.").await;
            let task = asked_to_erase(&agent, &id).await;
            let other = person_as(dir.path(), Some("agent:y"));
            let why = refusal_text(other.approve(approve_task(&task)).await);
            assert!(!why.contains("a session of the memory server"), "{why}");
            assert!(why.contains("agent:y"), "{why}");
            assert_eq!(approvals_of(&other, &id).await, Vec::<Value>::new());
            let person = person_as(dir.path(), None);
            let signed = payload(&person.approve(approve_task(&task)).await.unwrap());
            assert_eq!(signed["outcome"], "approved", "{signed}");
            assert_eq!(signed["approval"]["principal"], "human:test:me");
            assert_eq!(signed["approval"]["scope"], AGENT);
            let listed = approvals_of(&person, &id).await;
            assert_eq!(listed.len(), 1, "{listed:?}");
            assert_eq!(listed[0]["target"], id.as_str());
            assert_eq!(listed[0]["in_force"], true);
            let bad = ApproveParams {
                task: task.clone(),
                expires: Some("tomorrow".into()),
            };
            let bad = refusal_text(person.approve(Parameters(bad)).await);
            assert!(bad.contains("expires: "), "{bad}");
            let erase = EraseParams {
                id: id.clone(),
                reason: "mistaken".into(),
            };
            let receipt = payload(&agent.erase(Parameters(erase)).await.unwrap());
            assert_eq!(receipt["erased_by"], AGENT);
        }

        /// `get` takes an address. An anchor gives the section alone. The `version` of
        /// an answer is what a later `get` by version takes, and that `get` says when
        /// the version is no longer the current one.
        #[tokio::test]
        async fn get_serves_a_section_and_an_earlier_version_by_address() {
            let (dir, s) = server().await;
            let id = written(&s, "decision", "Where the import runs", DECISION).await;
            let whole = payload(&s.get(get_at(&id)).await.unwrap());
            assert_eq!(whole["body"], DECISION);
            assert!(whole.get("anchor").is_none() && whole.get("current").is_none());
            let first = whole["version"].as_str().unwrap().to_string();
            assert_eq!(first.len(), 64, "{first}");
            let part = payload(&s.get(get_at(&format!("{id}#decision"))).await.unwrap());
            assert_eq!(
                part["body"],
                "## Decision\n\nThe import job runs on the staging host every night.\n"
            );
            assert_eq!(part["anchor"], "decision");
            assert_eq!(part["version"], first.as_str());
            let unknown = refusal_text(s.get(get_at(&format!("{id}#options"))).await);
            assert!(unknown.contains("context, decision"), "{unknown}");
            let curator = Memory::open(dir.path()).unwrap().with_author("agent:c");
            let why = "checked against the host list";
            curator.promote(&id, TrustTier::Curated, why).await.unwrap();
            let now = payload(&s.get(get_at(&id)).await.unwrap());
            assert_eq!(now["trust_tier"], "curated");
            assert_ne!(now["version"], first.as_str());
            let old = payload(&s.get(get_at(&format!("{id}@{first}"))).await.unwrap());
            assert_eq!(old["current"], false);
            assert_eq!(old["trust_tier"], "raw");
            assert_eq!(old["version"], first.as_str());
            assert_eq!(old["body"], DECISION);
            let second = now["version"].as_str().unwrap();
            let same = payload(&s.get(get_at(&format!("{id}@{second}"))).await.unwrap());
            assert!(same.get("current").is_none(), "{same}");
            assert_eq!(same["trust_tier"], "curated");
        }

        /// `get` carries the related entries, each with its weight, under the key
        /// `related`. Three of four searches that returned the first entry returned
        /// the second too.
        #[tokio::test]
        async fn get_carries_the_related_entries_with_their_weight() {
            let (dir, s) = server().await;
            let a = written(
                &s,
                "fact",
                "Retry backoff",
                "Operation R retries with backoff.\n",
            )
            .await;
            let b = written(
                &s,
                "fact",
                "Backoff jitter",
                "Backoff adds jitter to spread load.\n",
            )
            .await;
            for query in ["backoff", "backoff", "backoff", "operation retries"] {
                let reader = Memory::open(dir.path()).unwrap();
                reader.search(query, None, 5).await.unwrap();
            }
            let got = payload(&served_as(dir.path(), None).get(get_at(&a)).await.unwrap());
            let related = json!([{
                "id": b,
                "title": "Backoff jitter",
                "summary": "Backoff adds jitter to spread load.",
                "p": 0.75,
            }]);
            assert_eq!(got["related"], related, "{got}");
            assert!(got.get("read_with").is_none(), "{got}");
        }

        fn search_for(query: &str, filters: Option<FiltersParams>) -> Parameters<SearchParams> {
            Parameters(SearchParams {
                query: query.into(),
                filters,
                ..SearchParams::default()
            })
        }

        /// `search` narrows by its filters, gives each hit a snippet, and refuses a
        /// filter value that it does not know.
        #[tokio::test]
        async fn search_narrows_by_its_filters_and_gives_each_hit_a_snippet() {
            let (_dir, s) = server().await;
            let decision = written(&s, "decision", "Where the import runs", DECISION).await;
            let note = "The import took four hours on the staging host last night.\n";
            let fact = written(&s, "fact", "Import duration", note).await;
            let all = payload(&s.search(search_for("import staging", None)).await.unwrap());
            let mut ids: Vec<&str> = all["hits"]
                .as_array()
                .unwrap()
                .iter()
                .map(|h| h["id"].as_str().unwrap())
                .collect();
            ids.sort_unstable();
            let mut both = [decision.as_str(), fact.as_str()];
            both.sort_unstable();
            assert_eq!(ids, both);
            for h in all["hits"].as_array().unwrap() {
                let snippet = h["snippet"].as_str().unwrap_or_default();
                assert!(snippet.contains("staging"), "{h}");
            }
            let only = FiltersParams {
                entry_type: Some("decision".into()),
                ..FiltersParams::default()
            };
            let narrowed = payload(
                &s.search(search_for("import staging", Some(only)))
                    .await
                    .unwrap(),
            );
            let hits = narrowed["hits"].as_array().unwrap();
            assert_eq!(hits.len(), 1, "{narrowed}");
            assert_eq!(hits[0]["id"], decision.as_str());
            assert_eq!(hits[0]["type"], "decision");
            let deep = SearchParams {
                depth: Some("sections".into()),
                filters: Some(FiltersParams {
                    entry_type: Some("fact".into()),
                    ..FiltersParams::default()
                }),
                ..search_for("import staging", None).0
            };
            let deep = payload(&s.search(Parameters(deep)).await.unwrap());
            let pieces = deep["hits"].as_array().unwrap();
            assert_eq!(pieces.len(), 1, "{deep}");
            assert_eq!(pieces[0]["id"], fact.as_str());
            for (field, bad) in [
                (
                    "filters.min_tier",
                    FiltersParams {
                        min_tier: Some("gold".into()),
                        ..FiltersParams::default()
                    },
                ),
                (
                    "filters.type",
                    FiltersParams {
                        entry_type: Some("rumour".into()),
                        ..FiltersParams::default()
                    },
                ),
                (
                    "filters.since",
                    FiltersParams {
                        since: Some("yesterday".into()),
                        ..FiltersParams::default()
                    },
                ),
            ] {
                let why = refusal_text(s.search(search_for("import", Some(bad))).await);
                assert!(why.contains(field), "{field}: {why}");
            }
        }

        /// The other filters reach the index too: the lowest tier, the author, and the
        /// time of creation.
        #[tokio::test]
        async fn search_narrows_by_tier_author_and_time() {
            let (dir, s) = server().await;
            let id = written(&s, "decision", "Where the import runs", DECISION).await;
            let found = |f: FiltersParams| async {
                let hits = payload(&s.search(search_for("import", Some(f))).await.unwrap());
                hits["hits"].as_array().unwrap().len()
            };
            let tier = |t: &str| FiltersParams {
                min_tier: Some(t.into()),
                ..FiltersParams::default()
            };
            let author = |a: &str| FiltersParams {
                author: Some(a.into()),
                ..FiltersParams::default()
            };
            let since = |t: &str| FiltersParams {
                since: Some(t.into()),
                ..FiltersParams::default()
            };
            assert_eq!(found(tier("curated")).await, 0);
            assert_eq!(found(tier("raw")).await, 1);
            assert_eq!(found(author("agent:nobody")).await, 0);
            assert_eq!(found(author("human:test:me")).await, 1);
            assert_eq!(found(since("2999-01-01T00:00:00Z")).await, 0);
            assert_eq!(found(since("2000-01-01T00:00:00Z")).await, 1);
            let curator = Memory::open(dir.path()).unwrap().with_author("agent:c");
            curator
                .promote(&id, TrustTier::Curated, "checked")
                .await
                .unwrap();
            assert_eq!(found(tier("curated")).await, 1);
        }

        fn recall_of(kind: Option<&str>, context: Option<usize>) -> Parameters<RecallParams> {
            Parameters(RecallParams {
                question: "where does the import run".into(),
                answer_kind: kind.map(Into::into),
                context_tokens: context,
                ..RecallParams::default()
            })
        }

        /// `recall` takes the answer kind and the context of the caller, and `plan`
        /// says what was chosen. A recall with neither has no plan.
        #[tokio::test]
        async fn recall_plans_its_size_from_the_answer_kind_and_the_context() {
            let (_dir, s) = server().await;
            written(&s, "decision", "Where the import runs", DECISION).await;
            let plain = payload(&s.recall(recall_of(None, None)).await.unwrap());
            assert!(plain.get("plan").is_none(), "{plain}");
            let fact = payload(&s.recall(recall_of(Some("fact"), None)).await.unwrap());
            assert_eq!(fact["plan"]["answer_kind"], "fact");
            assert!(fact["plan"]["context_tokens"].is_null(), "{fact}");
            let full = payload(&s.recall(recall_of(None, Some(120_000))).await.unwrap());
            assert_eq!(full["plan"]["answer_kind"], "overview");
            assert_eq!(full["plan"]["context_tokens"], 120_000);
            let overview = payload(&s.recall(recall_of(Some("overview"), None)).await.unwrap());
            assert!(
                full["plan"]["pack_tokens"].as_u64() < overview["plan"]["pack_tokens"].as_u64(),
                "{full} {overview}"
            );
            let why = refusal_text(s.recall(recall_of(Some("essay"), None)).await);
            assert!(why.contains("answer_kind: "), "{why}");
        }

        fn export_at(label: &str) -> Parameters<ExportParams> {
            Parameters(ExportParams {
                domain: None,
                max_label: label.into(),
            })
        }

        /// `export` keeps the bundle under the root and answers with counts, labels and
        /// a digest: no id of an entry. A label above the handling of the session is
        /// refused.
        #[tokio::test]
        async fn export_keeps_a_bundle_and_names_no_entry() {
            let (dir, agent, owner) = agent_and_owner();
            let id = entry_at(&owner, dm_core::Label::Public, "The door opens at nine.").await;
            let kept = payload(&agent.export(export_at("public")).await.unwrap());
            let at = kept["kept_at"].as_str().unwrap();
            assert!(at.starts_with("state/exports/"), "{at}");
            assert!(at.ends_with("/dom-me-public"), "{at}");
            assert!(dir.path().join(at).join("manifest.yaml").is_file());
            assert_eq!(kept["entries"], 1);
            assert_eq!(kept["highest_label"], "public");
            assert_eq!(kept["manifest_digest"].as_str().map(str::len), Some(64));
            assert!(!kept.to_string().contains(&id), "{kept}");
            let bad = refusal_text(agent.export(export_at("secret")).await);
            assert!(bad.contains("max_label: "), "{bad}");
            let capped = Memory::open(dir.path())
                .unwrap()
                .with_author(AGENT)
                .with_handling_cap(dm_core::Label::Public);
            let capped = McpServer::new(Arc::new(capped));
            let why = refusal_text(capped.export(export_at("internal")).await);
            assert!(why.contains("exceeds handling public"), "{why}");
        }
    }
}
