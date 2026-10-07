//! Hooks the launcher installs in the harness. Two leave handoffs the harness did not
//! write: when context is about to be compacted and when the session ends, a note made
//! from the transcript's tail lands in an outbox in the private home, and the launcher
//! files the notes as handoffs when the run ends. Files, not a client, because the only
//! thing those need to reach is a directory. The third fires before a tool runs: a read
//! of the projection, the memory as files, becomes a `get` on the memory server, so the
//! filter, the label and the funnel apply to a file read as to a tool call; the server
//! is reached over the relay, the one route out of the sandbox.

use serde::Deserialize;
use serde_json::Value;
use std::path::{Path, PathBuf};

type Fallible<T> = Result<T, Box<dyn std::error::Error>>;

/// Where notes wait in the private home.
pub const OUTBOX: &str = ".memory-outbox";

/// What Claude Code passes on standard input. Other harnesses' hooks map onto this.
#[derive(Debug, Deserialize)]
pub struct HookInput {
    /// Which event. Antigravity CLI names it in camelCase, as it does every key.
    #[serde(alias = "hookEventName", alias = "event")]
    pub hook_event_name: String,
    /// The harness's own session id; Antigravity's conversation id.
    #[serde(default, alias = "conversationId")]
    pub session_id: String,
    /// The transcript, a JSON Lines file.
    #[serde(default, alias = "transcriptPath")]
    pub transcript_path: Option<PathBuf>,
    /// PreCompact: manual or auto.
    #[serde(default)]
    pub trigger: Option<String>,
    /// SessionEnd: why.
    #[serde(default)]
    pub reason: Option<String>,
    /// PreToolUse: the tool about to run.
    #[serde(default)]
    pub tool_name: Option<String>,
    /// PreToolUse: its input, in the tool's own shape.
    #[serde(default)]
    pub tool_input: Value,
    /// PreToolUse as Antigravity CLI sends it: the tool's name and arguments together.
    #[serde(default, alias = "toolCall")]
    pub tool_call: Option<Value>,
}

impl HookInput {
    /// Read one event. Grok Build writes its camelCase keys and the snake_case
    /// aliases side by side, with Claude's PascalCase event name under the snake key
    /// only; a key present in both spellings is read from the snake one, since a
    /// field and its alias together are a duplicate to the deserializer.
    ///
    /// # Errors
    ///
    /// When the text is not a hook event.
    pub fn parse(text: &str) -> Result<HookInput, serde_json::Error> {
        let mut v: Value = serde_json::from_str(text)?;
        if let Some(map) = v.as_object_mut() {
            for (camel, snake) in [
                ("hookEventName", "hook_event_name"),
                ("sessionId", "session_id"),
                ("conversationId", "session_id"),
                ("transcriptPath", "transcript_path"),
                ("toolName", "tool_name"),
                ("toolInput", "tool_input"),
            ] {
                if map.contains_key(snake) {
                    map.remove(camel);
                }
            }
        }
        serde_json::from_value(v)
    }

    /// The tool about to run and its input, from whichever shape the harness sent.
    fn tool(&self) -> (String, Value) {
        if let Some(call) = &self.tool_call {
            let name = call
                .get("name")
                .or_else(|| call.get("toolName"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let input = call
                .get("args")
                .or_else(|| call.get("arguments"))
                .or_else(|| call.get("input"))
                .cloned()
                .unwrap_or(Value::Null);
            return (name, input);
        }
        (
            self.tool_name.clone().unwrap_or_default(),
            self.tool_input.clone(),
        )
    }
}

/// What the hook tells the harness about a tool call. Nothing said is allowed; a denial
/// carries its reason, which the harness shows the model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Not the hook's business, or a read the memory served.
    Allow,
    /// Refused, and why.
    Deny(String),
}

impl Decision {
    /// The hook's standard output in the harness's shape. Claude Code, Codex and Qwen
    /// Code read a `hookSpecificOutput` and take silence as allowed; Gemini CLI reads a
    /// top-level `decision` and takes silence as allowed; Antigravity CLI and Grok
    /// Build want a `decision` either way, since an empty answer is refused or logged
    /// as a broken hook.
    #[must_use]
    pub fn output(&self, harness: &str) -> Option<String> {
        if matches!(harness, "antigravity" | "gemini" | "grok") {
            return match self {
                Decision::Allow if harness == "gemini" => None,
                Decision::Allow => Some(serde_json::json!({ "decision": "allow" }).to_string()),
                Decision::Deny(reason) => {
                    Some(serde_json::json!({ "decision": "deny", "reason": reason }).to_string())
                }
            };
        }
        match self {
            Decision::Allow => None,
            Decision::Deny(reason) => Some(
                serde_json::json!({
                    "hookSpecificOutput": {
                        "hookEventName": "PreToolUse",
                        "permissionDecision": "deny",
                        "permissionDecisionReason": reason,
                    }
                })
                .to_string(),
            ),
        }
    }
}

/// The name of the event before a tool runs: `PreToolUse` in Claude Code, Codex, Qwen
/// Code and Antigravity CLI, `BeforeTool` in Gemini CLI.
#[must_use]
pub fn pre_tool_event(name: &str) -> bool {
    matches!(name, "PreToolUse" | "BeforeTool")
}

/// The path a tool call is about, for the tools that touch files: `file_path` for a
/// read or an edit, `notebook_path` for a notebook, `path` for a search over a tree,
/// and for a search without one, its pattern when the pattern is a path.
fn path_of(input: &Value) -> Option<PathBuf> {
    // Claude Code's names, Gemini CLI's and Qwen Code's (`absolute_path` is what
    // `read_file` took before `file_path`), Grok Build's `target_file` and
    // `target_directory`, then Antigravity CLI's, which are PascalCase
    [
        "file_path",
        "absolute_path",
        "notebook_path",
        "target_file",
        "path",
        "dir_path",
        "target_directory",
        "pattern",
        "AbsolutePath",
        "SearchPath",
        "DirectoryPath",
        "SearchDirectory",
        "Pattern",
    ]
    .iter()
    .find_map(|k| {
        input
            .get(*k)
            .and_then(Value::as_str)
            .filter(|p| !p.is_empty())
    })
    .map(|p| normalise(Path::new(p)))
}

/// A path with `.` and `..` folded away lexically, so `/memory/../memory/x` is
/// `/memory/x` and `/memory/..` is `/`; a path through a process's root in `/proc`,
/// which is the same tree under another name, is read from the root.
fn normalise(path: &Path) -> PathBuf {
    use std::path::Component;
    let text = path.to_string_lossy();
    let via_proc = text
        .strip_prefix("/proc/")
        .and_then(|rest| rest.split_once("/root"))
        .filter(|(pid, _)| {
            *pid == "self" || *pid == "thread-self" || pid.bytes().all(|b| b.is_ascii_digit())
        })
        .map(|(_, rest)| format!("/{}", rest.trim_start_matches('/')));
    let path = via_proc.as_deref().map_or(path, Path::new);
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// A tool that reads one file, under the names the harnesses give it.
fn reads_one(tool: &str) -> bool {
    matches!(
        tool,
        "Read" | "read" | "read_file" | "view" | "view_file" | "NotebookRead" | "read_resource"
    )
}

/// A tool that lists files by glob.
fn globs(tool: &str) -> bool {
    matches!(tool, "Glob" | "glob" | "find_by_name")
}

/// Whether a glob pattern, or a plain path, can reach into `projection`: it climbs with
/// a `..` component, or its literal prefix, up to the first wildcard, is under the
/// projection or names it or an ancestor of it. Paths are compared by component, so a
/// sibling whose name merely starts with the projection's is not the projection; a
/// prefix cut by a wildcard is compared as text, since `/mem*` does match `/memory`.
fn glob_reaches(pattern: &str, projection: &Path) -> bool {
    use std::path::Component;
    if Path::new(pattern)
        .components()
        .any(|c| c == Component::ParentDir)
    {
        return true;
    }
    let wild = pattern.find(['*', '?', '[']);
    let literal = &pattern[..wild.unwrap_or(pattern.len())];
    if !literal.starts_with('/') {
        return false;
    }
    let under = Path::new(literal).starts_with(projection);
    let above = match wild {
        Some(_) => projection.to_string_lossy().starts_with(literal),
        None => projection.starts_with(Path::new(literal)),
    };
    under || above
}

/// A tool that searches a tree, by path or by glob.
fn searches(tool: &str) -> bool {
    matches!(
        tool,
        "Grep"
            | "Glob"
            | "grep"
            | "glob"
            | "search_files"
            | "search_file_content"
            | "list_directory"
            | "grep_search"
            | "grep_files"
            | "find_by_name"
            | "list_dir"
    )
}

/// Whether a read of many files at once, Gemini CLI's and Qwen Code's
/// `read_many_files`, reaches into `projection`: any of its paths or globs is under
/// the projection, holds it, or climbs. Such a read is a search over bodies, whatever
/// else it lists.
fn reads_many_into(tool: &str, input: &Value, projection: &Path) -> bool {
    tool == "read_many_files"
        && ["paths", "include"].iter().any(|k| {
            input.get(*k).and_then(Value::as_array).is_some_and(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .any(|p| glob_reaches(p, projection))
            })
        })
}

/// Why a tool is refused over the projection as a whole.
fn whole_of(projection: &Path) -> String {
    format!(
        "{} is the memory's projection: its bodies are read one file at a time, each \
         read a `get` the memory serves and counts. Use the memory's `search` tool, or \
         open one entry file.",
        projection.display()
    )
}

/// Decide a tool call against the projection at `projection`. A read of an entry file
/// is served through `get`, which returns the server's refusal in words when it has
/// one; a read of the summaries page is free, as a list is; any other name under the
/// projection is refused, whatever put it there; a search over the projection, or over
/// a directory that holds it, is refused, since a `grep -r` over bodies is a
/// full-corpus read the memory would not have served; so is a write, which the mount
/// refuses anyway. Anything outside the projection is not the hook's.
pub fn decide<F>(tool: &str, input: &Value, projection: &Path, session: &str, get: F) -> Decision
where
    F: FnOnce(&str) -> Result<(), String>,
{
    let projection = normalise(projection);
    let Some(path) = path_of(input) else {
        if reads_many_into(tool, input, &projection) {
            return Decision::Deny(whole_of(&projection));
        }
        return Decision::Allow;
    };
    // a search over an absolute ancestor of the projection covers it; a relative path
    // is the working tree's, which does not hold the projection
    let covers = searches(tool) && path.is_absolute() && projection.starts_with(&path);
    // a glob reaches wherever its pattern says, whatever its path says; a grep's
    // pattern is a regex over text, not a place
    let by_pattern = globs(tool)
        && input
            .get("pattern")
            .or_else(|| input.get("Pattern"))
            .and_then(Value::as_str)
            .is_some_and(|pat| glob_reaches(pat, &projection));
    if !path.starts_with(&projection) && !covers && !by_pattern {
        return Decision::Allow;
    }
    let whole = whole_of(&projection);
    if !reads_one(tool) {
        return Decision::Deny(whole);
    }
    if path
        .file_name()
        .is_some_and(|n| n == dm_server::projection::SUMMARIES)
    {
        return Decision::Allow;
    }
    // this session's recall page, `recall/<session>/<stamp>.md` directly under the
    // projection: what it holds was served to this session when the recall ran;
    // another session's pages are not this one's to read
    let recall_page = !session.is_empty()
        && path
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|s| s == session)
        && path
            .parent()
            .and_then(Path::parent)
            .is_some_and(|d| d == projection.join(dm_server::projection::RECALLS))
        && path.extension().is_some_and(|e| e == "md");
    if recall_page {
        return Decision::Allow;
    }
    match dm_server::projection::id_of(&path) {
        Some(id) => match get(id) {
            Ok(()) => Decision::Allow,
            Err(why) => Decision::Deny(format!(
                "the memory refused {id}: {why}. Its summary is in the README beside it; \
                 the memory's `search` tool finds what you may read."
            )),
        },
        None => Decision::Deny(whole),
    }
}

/// The answer to input the hook could not read. Grok Build truncates a payload over its
/// size cap to one JSON string and lets a tool through when the hook says nothing, so
/// there the answer is a refusal, which it honours on any event that takes one; the
/// other harnesses take silence as nothing decided, which refuses nothing but also
/// lets no truncated call through, since they do not truncate.
#[must_use]
pub fn unreadable(harness: &str) -> Option<String> {
    (harness == "grok").then(|| {
        serde_json::json!({
            "decision": "deny",
            "reason": "the memory's hook could not read this tool call, so it is refused; \
                       a smaller call, or the memory's `search` and `get` tools, will do",
        })
        .to_string()
    })
}

/// The transcript to read for a hook event: the file the harness named, unless it is
/// Grok Build's `updates.jsonl`, the ACP log, with the chat history beside it, which is
/// the shape the tail reader knows.
#[must_use]
pub fn transcript_file(path: &Path) -> PathBuf {
    if path.file_name().is_some_and(|n| n == "updates.jsonl") {
        let history = path.with_file_name("chat_history.jsonl");
        if history.is_file() {
            return history;
        }
    }
    path.to_path_buf()
}

/// How long the hook waits for the memory server before it refuses: inside the hook's
/// own timeout, so a stalled relay is a refusal and not a read let through.
const WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// The PreToolUse hook against `projection`, reaching the memory server at `url` and
/// waiting `wait` for it. No projection, nothing to decide. A projection with no
/// server, a server that does not answer in time, or one that refuses are all
/// refusals: the hook fails closed.
async fn pre_tool_with(
    input: &HookInput,
    projection: Option<&Path>,
    session: &str,
    url: Option<&str>,
    wait: std::time::Duration,
) -> Decision {
    let Some(projection) = projection else {
        return Decision::Allow;
    };
    let (tool, tool_input) = input.tool();
    // the id to fetch is decided first, then fetched: the decision is pure, the fetch is not
    let mut wanted: Option<String> = None;
    let d = decide(&tool, &tool_input, projection, session, |id| {
        wanted = Some(id.to_string());
        Ok(())
    });
    let Some(id) = wanted else {
        return d;
    };
    let refused = |why: String| decide(&tool, &tool_input, projection, session, |_| Err(why));
    let Some(url) = url else {
        return refused(
            "the memory server's address is not set, so nothing here can be read".into(),
        );
    };
    let call = crate::mcp_http::call_tool(url, "get", serde_json::json!({ "id": id }));
    match tokio::time::timeout(wait, call).await {
        Ok(Ok(_)) => Decision::Allow,
        Ok(Err(why)) => refused(why),
        Err(_) => refused(format!("the memory server did not answer within {wait:?}")),
    }
}

/// The PreToolUse hook as the sandbox runs it: the projection and the server's address
/// come from the environment the launcher set. With `MEMORY_SUBAGENTS=board`, a call
/// that would spawn or orchestrate subagents is filed on the task board instead.
async fn pre_tool(input: &HookInput) -> Decision {
    let projection = std::env::var_os("MEMORY_PROJECTION").map(PathBuf::from);
    let session = std::env::var("MEMORY_SESSION").unwrap_or_default();
    let url = std::env::var("MEMORY_MCP_URL").ok();
    if std::env::var("MEMORY_SUBAGENTS").is_ok_and(|v| v == "board") {
        let (tool, tool_input) = input.tool();
        if crate::board::delegates(&tool) {
            return delegate(&tool, &tool_input, &session, url.as_deref(), WAIT).await;
        }
    }
    pre_tool_with(input, projection.as_deref(), &session, url.as_deref(), WAIT).await
}

/// File what a subagent call asks for as tasks, and refuse the call with their ids.
/// A call with nothing readable in it is let through: there is nothing to file. A
/// server that cannot be reached is a refusal that says so, since letting the harness
/// orchestrate after all would be the one thing the mode exists to prevent.
async fn delegate(
    tool: &str,
    tool_input: &Value,
    session: &str,
    url: Option<&str>,
    wait: std::time::Duration,
) -> Decision {
    let units = crate::board::units_of(tool, tool_input);
    if units.is_empty() {
        return Decision::Allow;
    }
    let Some(url) = url else {
        return Decision::Deny(
            "subagents go to the task board here, and the memory server's address is not set, so nothing can be filed; do this part yourself".into(),
        );
    };
    // under a task, the spawn is a fan-out of that task: children and a continuation
    if let Ok(parent) = std::env::var("MEMORY_TASK") {
        if !parent.is_empty() {
            return fan_out(url, &parent, tool, &units, wait).await;
        }
    }
    let mut ids = Vec::new();
    for u in units {
        let body = format!(
            "{}\n\n---\nFiled from session {session}: the harness asked for a subagent (`{tool}`) and the board runs it instead.",
            u.body
        );
        let call = crate::mcp_http::call_tool(
            url,
            "create_task",
            serde_json::json!({ "title": u.title, "body": body }),
        );
        match tokio::time::timeout(wait, call).await {
            Ok(Ok(v)) => {
                // the server answers with JSON in a text block
                let text = v
                    .pointer("/content/0/text")
                    .and_then(Value::as_str)
                    .unwrap_or("{}");
                let answer: Value = serde_json::from_str(text).unwrap_or(Value::Null);
                let id = answer
                    .pointer("/task/id")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
                    .to_string();
                ids.push(id);
            }
            Ok(Err(why)) => {
                return Decision::Deny(format!(
                    "subagents go to the task board here, and filing failed: {why}; do this part yourself"
                ))
            }
            Err(_) => {
                return Decision::Deny(format!(
                    "subagents go to the task board here, and the memory server did not answer within {wait:?}; do this part yourself"
                ))
            }
        }
    }
    Decision::Deny(crate::board::refusal(&ids))
}

/// File the units as the subagent tasks of `parent`, with a continuation that resumes
/// it when they are done, and refuse the call with what was filed.
async fn fan_out(
    url: &str,
    parent: &str,
    tool: &str,
    units: &[crate::board::Unit],
    wait: std::time::Duration,
) -> Decision {
    let items: Vec<Value> = units
        .iter()
        .map(|u| serde_json::json!({ "title": u.title, "body": u.body }))
        .collect();
    let call = crate::mcp_http::call_tool(
        url,
        "fan_out",
        serde_json::json!({ "parent": parent, "items": items }),
    );
    let answer = match tokio::time::timeout(wait, call).await {
        Ok(Ok(v)) => {
            let text = v
                .pointer("/content/0/text")
                .and_then(Value::as_str)
                .unwrap_or("{}");
            serde_json::from_str::<Value>(text).unwrap_or(Value::Null)
        }
        Ok(Err(why)) => {
            return Decision::Deny(format!(
                "subagents go to the task board here, and filing failed: {why}; do this part yourself"
            ))
        }
        Err(_) => {
            return Decision::Deny(format!(
                "subagents go to the task board here, and the memory server did not answer within {wait:?}; do this part yourself"
            ))
        }
    };
    let ids: Vec<String> = answer["children"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|c| c["id"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    // what an earlier run of this chain already asked for: done, with a handoff to
    // read, or still open on the board
    let mut earlier = Vec::new();
    for e in answer["existing"].as_array().into_iter().flatten() {
        let id = e["id"].as_str().unwrap_or("?");
        match e["handoff"].as_str() {
            Some(h) => earlier.push(format!(
                "`{id}` was already done; its handoff is `{h}`, read it with the memory server's `get`"
            )),
            None => earlier.push(format!(
                "`{id}` is already on the board ({}); do not ask again",
                e["status"].as_str().unwrap_or("open")
            )),
        }
    }
    let mut text = String::new();
    if !ids.is_empty() {
        let continuation = answer["continuation"]["id"].as_str().unwrap_or("?");
        text.push_str(&format!(
            "{} The board will resume `{parent}` as `{continuation}` once they are done, with their handoffs in its prompt (`{tool}` filed them). ",
            crate::board::refusal(&ids)
        ));
    }
    if !earlier.is_empty() {
        text.push_str("This was asked for before in this chain: ");
        text.push_str(&earlier.join("; "));
        text.push_str(". ");
    }
    text.push_str("Finish what you can from what is already known, then leave a handoff.");
    Decision::Deny(text)
}

/// Why a note was left; part of its filename so the launcher can tell them apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// Context is about to be compacted; what is in it is about to be lost.
    PreCompact,
    /// The session is ending.
    SessionEnd,
}

impl Event {
    fn from_name(name: &str) -> Option<Event> {
        match name {
            // Claude Code and Codex say PreCompact; Gemini CLI says PreCompress
            "PreCompact" | "PreCompress" => Some(Event::PreCompact),
            // Antigravity CLI says Stop
            "SessionEnd" | "Stop" => Some(Event::SessionEnd),
            _ => None,
        }
    }

    /// The filename suffix.
    #[must_use]
    pub fn suffix(self) -> &'static str {
        match self {
            Event::PreCompact => "precompact",
            Event::SessionEnd => "sessionend",
        }
    }
}

/// One text turn of a transcript.
struct Turn {
    role: String,
    text: String,
}

fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| match b.get("type").and_then(Value::as_str) {
                Some("text" | "input_text" | "output_text") | None => {
                    b.get("text").and_then(Value::as_str).map(str::to_string)
                }
                Some("tool_use") => b
                    .get("name")
                    .and_then(Value::as_str)
                    .map(|n| format!("[used tool {n}]")),
                Some("tool_result") => Some("[tool result]".to_string()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// The turns one transcript line holds, in whichever harness's shape it is.
///
/// Claude Code: `{"type":"user"|"assistant","message":{"content":…}}`, one turn a line.
/// Codex: `{"type":"response_item","payload":{"type":"message","role":…,"content":[{"text":…}]}}`.
/// Gemini CLI: a patch log, `{"$set":{"messages":[{"type":"user"|"gemini","content":[{"text":…}]}]}}`.
/// Antigravity CLI: `{"type":"USER_INPUT"|"PLANNER_RESPONSE","content":"…"}`, one a line.
/// Muse Code: the JSONL its headless run prints, records with a `payload_type`; the
/// prompt is `turn.input.user` and the answer is `run.terminal.completed`.
/// pi: `{"type":"message","message":{"role":…,"content":[{"type":"text","text":…}]}}`.
/// A Muse Code record's turn, when it is one: the prompt or the final text.
fn muse_turn(v: &Value) -> Option<(&'static str, String)> {
    let payload = v.get("payload")?;
    match v.get("payload_type").and_then(Value::as_str)? {
        "turn.input.user" => Some(("user", payload.get("prompt")?.as_str()?.to_string())),
        "run.terminal.completed" => Some(("assistant", payload.get("text")?.as_str()?.to_string())),
        _ => None,
    }
}

fn turns_in(v: &Value) -> Vec<Turn> {
    let keep = |role: &str, text: String| {
        let text = text.trim().to_string();
        (!text.is_empty() && text != "[tool result]").then(|| Turn {
            role: role.to_string(),
            text,
        })
    };
    if v.get("payload_type").is_some() {
        return muse_turn(v)
            .and_then(|(who, text)| keep(who, text))
            .into_iter()
            .collect();
    }
    if let Some(role) = v.get("type").and_then(Value::as_str) {
        let planner = match role {
            "USER_INPUT" => Some("user"),
            "PLANNER_RESPONSE" => Some("assistant"),
            _ => None,
        };
        if let Some(who) = planner {
            let text = v.get("content").map(text_of).unwrap_or_default();
            // agy wraps what the user typed in a request tag
            let text = text
                .replace("<USER_REQUEST>", "")
                .replace("</USER_REQUEST>", "");
            return keep(who, text).into_iter().collect();
        }
        // pi wraps a message, with the role inside it
        if role == "message" {
            if let Some(m) = v.get("message") {
                let who = m.get("role").and_then(Value::as_str).unwrap_or_default();
                if who == "user" || who == "assistant" {
                    return keep(who, text_of(m.get("content").unwrap_or(&Value::Null)))
                        .into_iter()
                        .collect();
                }
            }
            return Vec::new();
        }
        // Claude Code's message has content; Qwen Code's is the Gemini API's, with
        // parts; Grok Build's item carries its content at the top
        if role == "user" || role == "assistant" {
            let content = match v.get("message") {
                Some(m) => m.get("content").or_else(|| m.get("parts")),
                None => v.get("content"),
            };
            if let Some(c) = content {
                return keep(role, text_of(c)).into_iter().collect();
            }
        }
        if role == "response_item" {
            if let Some(p) = v.get("payload") {
                let is_message = p.get("type").and_then(Value::as_str) == Some("message");
                let who = p.get("role").and_then(Value::as_str).unwrap_or_default();
                if is_message && (who == "user" || who == "assistant") {
                    return keep(who, text_of(p.get("content").unwrap_or(&Value::Null)))
                        .into_iter()
                        .collect();
                }
            }
        }
    }
    let mut out = Vec::new();
    if let Some(messages) = v
        .pointer("/$set/messages")
        .or_else(|| v.get("messages"))
        .and_then(Value::as_array)
    {
        for m in messages {
            let who = match m.get("type").and_then(Value::as_str) {
                Some("user") => "user",
                Some("gemini" | "model" | "assistant") => "assistant",
                _ => continue,
            };
            out.extend(keep(who, text_of(m.get("content").unwrap_or(&Value::Null))));
        }
    }
    out
}

/// The last `turns` user and assistant turns of a transcript, as Markdown, capped at
/// `max_chars`. Tool calls are named, their results elided. Claude Code, Codex and Gemini
/// CLI shapes are all read.
#[must_use]
pub fn transcript_tail(jsonl: &str, turns: usize, max_chars: usize) -> String {
    let mut all: Vec<Turn> = jsonl
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .flat_map(|v| turns_in(&v))
        .collect();
    let keep = all.len().saturating_sub(turns);
    all.drain(..keep);
    let mut out = String::new();
    for t in &all {
        out.push_str(&format!("**{}**\n\n{}\n\n", t.role, t.text));
    }
    let out = out.trim_end().to_string();
    if out.chars().count() <= max_chars {
        return out;
    }
    let skip = out.chars().count() - max_chars;
    format!("…{}", out.chars().skip(skip).collect::<String>())
}

/// Title and body of the note for an event.
#[must_use]
pub fn note(input: &HookInput, event: Event, tail: &str) -> (String, String) {
    let when = match event {
        Event::PreCompact => format!(
            "before compaction ({})",
            input.trigger.as_deref().unwrap_or("auto")
        ),
        Event::SessionEnd => format!(
            "at session end ({})",
            input.reason.as_deref().unwrap_or("other")
        ),
    };
    let title = format!("Session {} {when}", short(&input.session_id));
    let transcript = input
        .transcript_path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "(none)".into());
    let body =
        format!(
        "# {title}\n\nLeft by the launcher's hook, not by the agent: the last exchanges of the \
         transcript, {when}. Treat as raw. The full transcript is at `{transcript}` in the \
         session's private home.\n\n## Last exchanges\n\n{}\n",
        if tail.is_empty() { "(nothing yet)" } else { tail }
    );
    (title, body)
}

fn short(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

/// Run as the hook: read the event from standard input; before a tool, decide and say
/// so on standard output; otherwise leave a note in the outbox. Exit 0 whatever
/// happens, so a hook never blocks the harness.
///
/// # Errors
///
/// When the input is not a hook event or the outbox cannot be written.
pub fn run(outbox: &Path, harness: &str) -> Fallible<()> {
    let mut text = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)?;
    if std::env::var_os("MEMORY_HOOK_DEBUG").is_some() {
        // what the harness really sends, for an adapter written from its docs
        std::fs::create_dir_all(outbox).ok();
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(outbox.join("hook-debug.log"))
        {
            use std::io::Write;
            let _ = writeln!(f, "{harness} {}", text.trim());
        }
    }
    let input = match HookInput::parse(&text) {
        Ok(input) => input,
        Err(e) => {
            if let Some(out) = unreadable(harness) {
                println!("{out}");
            }
            return Err(e.into());
        }
    };
    if pre_tool_event(&input.hook_event_name) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        if let Some(out) = rt.block_on(pre_tool(&input)).output(harness) {
            println!("{out}");
        }
        return Ok(());
    }
    let Some(event) = Event::from_name(&input.hook_event_name) else {
        if harness == "antigravity" {
            println!("{{}}");
        }
        return Ok(());
    };
    let transcript = input
        .transcript_path
        .as_deref()
        .and_then(|p| std::fs::read_to_string(transcript_file(p)).ok())
        .unwrap_or_default();
    let tail = transcript_tail(&transcript, 10, 6000);
    let (_, body) = note(&input, event, &tail);
    std::fs::create_dir_all(outbox)?;
    let stamp = time::OffsetDateTime::now_utc().unix_timestamp_nanos();
    // agy's Stop fires at the end of every turn, not once a session: its note is
    // named by the conversation and rewritten, so the outbox holds the last one
    let name = if harness == "antigravity" && event == Event::SessionEnd {
        format!("agy-{}-{}.md", short(&input.session_id), event.suffix())
    } else {
        format!("{stamp}-{}.md", event.suffix())
    };
    let tmp = outbox.join(format!("{name}.tmp"));
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, outbox.join(name))?;
    // agy wants a JSON object from every hook; the others take silence
    if harness == "antigravity" {
        println!("{{}}");
    }
    Ok(())
}

/// A note waiting in the outbox.
#[derive(Debug)]
pub struct Note {
    /// Its file, to remove once filed.
    pub path: PathBuf,
    /// Why it was left.
    pub event: Event,
    /// The first heading.
    pub title: String,
    /// The whole Markdown.
    pub body: String,
}

/// The notes in an outbox, oldest first.
///
/// # Errors
///
/// When the directory cannot be read.
pub fn notes(outbox: &Path) -> Fallible<Vec<Note>> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(outbox) else {
        return Ok(out);
    };
    for entry in rd {
        let path = entry?.path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if !name.ends_with(".md") {
            continue;
        }
        let event = if name.ends_with("-precompact.md") {
            Event::PreCompact
        } else if name.ends_with("-sessionend.md") {
            Event::SessionEnd
        } else {
            continue;
        };
        let body = std::fs::read_to_string(&path)?;
        let title = body
            .lines()
            .find_map(|l| l.strip_prefix("# "))
            .unwrap_or("Session note")
            .to_string();
        out.push(Note {
            path,
            event,
            title,
            body,
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SID: &str = "sess-abc12345";

    const TRANSCRIPT: &str = r#"{"type":"user","message":{"role":"user","content":"Load T0 and summarise it."}}
{"type":"cost-state","x":1}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Loading T0."},{"type":"tool_use","name":"mcp__memory__t0","input":{}}]}}
{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"...big..."}]}}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"T0 says: two handoffs, one decision."}]}}
not json
"#;

    #[test]
    fn the_tail_keeps_text_and_names_tools_but_drops_results() {
        let t = transcript_tail(TRANSCRIPT, 10, 10_000);
        assert!(t.starts_with("**user**\n\nLoad T0"), "{t}");
        assert!(t.contains("[used tool mcp__memory__t0]"), "{t}");
        assert!(!t.contains("big"), "tool results are elided: {t}");
        assert!(t.ends_with("one decision."), "{t}");
        let last = transcript_tail(TRANSCRIPT, 1, 10_000);
        assert!(last.starts_with("**assistant**\n\nT0 says"), "{last}");
        let capped = transcript_tail(TRANSCRIPT, 10, 20);
        assert!(
            capped.starts_with('…') && capped.chars().count() <= 21,
            "{capped}"
        );
        assert_eq!(transcript_tail("", 10, 100), "");
    }

    #[test]
    fn pi_s_session_file_is_read() {
        let t = concat!(
            r#"{"type":"session","version":3,"id":"s1"}"#,
            "\n",
            r#"{"type":"model_change","provider":"llmao","modelId":"qwen3.8-27b"}"#,
            "\n",
            r#"{"type":"message","message":{"role":"system","content":"","sections":{}}}"#,
            "\n",
            r#"{"type":"message","message":{"role":"user","content":[{"type":"text","text":"what is T0?"}]}}"#,
            "\n",
            r#"{"type":"message","message":{"role":"assistant","content":[{"type":"text","text":"The first page."}],"usage":{"input":120,"output":4}}}"#,
            "\n",
        );
        let tail = transcript_tail(t, 10, 10_000);
        assert!(
            tail.contains("what is T0?") && tail.contains("The first page."),
            "{tail}"
        );
        assert!(!tail.contains("system"), "{tail}");
        assert!(reads_one("read"), "pi's read tool");
    }

    #[test]
    fn muse_s_headless_stream_is_read() {
        // Muse Code's headless stream: records typed by payload, the prompt in
        // turn.input.user and the answer in run.terminal.completed
        let muse = concat!(
            r#"{"payload_type":"turn.input.user","payload":{"prompt":"what is the plan?"}}"#,
            "\n",
            r#"{"payload_type":"run.output.delta","payload":{"text":"The"}}"#,
            "\n",
            r#"{"payload_type":"run.terminal.completed","payload":{"terminal":"completed","text":"The plan is M7."}}"#,
            "\n",
        );
        let t = transcript_tail(muse, 10, 10_000);
        assert!(
            t.contains("what is the plan?") && t.contains("The plan is M7."),
            "{t}"
        );
        assert!(!t.contains("\"The\""), "deltas are not turns: {t}");
    }

    #[test]
    fn codex_and_gemini_transcripts_are_read_too() {
        let codex = concat!(
            r#"{"type":"session_meta","payload":{"id":"s"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"system stuff"}]}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Finish the import."}]}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Step 3 done."}]}}"#,
            "\n",
        );
        let t = transcript_tail(codex, 10, 10_000);
        assert!(
            !t.contains("system stuff"),
            "developer messages are not turns: {t}"
        );
        assert!(
            t.contains("**user**\n\nFinish the import.") && t.ends_with("Step 3 done."),
            "{t}"
        );
        let gemini = concat!(
            r#"{"sessionId":"g","kind":"main"}"#,
            "\n",
            r#"{"$set":{"messages":[{"type":"user","content":[{"text":"Hello there."}]},{"type":"gemini","content":[{"text":"General Kenobi."}]}]}}"#,
            "\n",
        );
        let t = transcript_tail(gemini, 10, 10_000);
        assert!(
            t.starts_with("**user**\n\nHello there.") && t.ends_with("General Kenobi."),
            "{t}"
        );
        // Qwen Code: one record a line, the message in the Gemini API's shape, parts
        // rather than content
        let qwen = concat!(
            r#"{"type":"user","message":{"role":"user","parts":[{"text":"Hello there."}]},"timestamp":"t"}"#,
            "\n",
            r#"{"type":"tool_result","message":{"role":"user","parts":[{"functionResponse":{"name":"read_file"}}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"role":"model","parts":[{"text":"General Kenobi."}]}}"#,
            "\n",
        );
        let t = transcript_tail(qwen, 10, 10_000);
        assert!(
            t.starts_with("**user**\n\nHello there.") && t.ends_with("General Kenobi."),
            "{t}"
        );
        // Grok Build's chat history: typed items with the content at the top, parts
        // for the user and a string for the assistant
        let grok = concat!(
            r#"{"type":"system","content":"be brief"}"#,
            "\n",
            r#"{"type":"user","content":[{"type":"text","text":"Hello there."}]}"#,
            "\n",
            r#"{"type":"assistant","content":"General Kenobi.","tool_calls":[]}"#,
            "\n",
        );
        let t = transcript_tail(grok, 10, 10_000);
        assert!(
            t.starts_with("**user**\n\nHello there.") && t.ends_with("General Kenobi."),
            "{t}"
        );
        assert!(!t.contains("be brief"), "a system item is not a turn: {t}");
        assert_eq!(Event::from_name("PreCompress"), Some(Event::PreCompact));
        assert_eq!(Event::from_name("Nope"), None);
        let agy = concat!(
            r#"{"type":"USER_INPUT","source":"USER_EXPLICIT","content":"<USER_REQUEST>Load T0.</USER_REQUEST>"}"#,
            "\n",
            r#"{"type":"PLANNER_RESPONSE","source":"MODEL","content":"T0 says: one handoff."}"#,
            "\n",
            r#"{"type":"TOOL_CALL","content":"ignored"}"#,
        );
        let t = transcript_tail(agy, 10, 10_000);
        assert_eq!(
            t,
            "**user**\n\nLoad T0.\n\n**assistant**\n\nT0 says: one handoff."
        );
    }

    #[test]
    fn a_note_says_why_and_where() {
        let input = HookInput {
            hook_event_name: "PreCompact".into(),
            session_id: "abcdef12-3456".into(),
            transcript_path: Some("/home/user/.claude/projects/p/s.jsonl".into()),
            trigger: Some("auto".into()),
            reason: None,
            tool_name: None,
            tool_input: Value::Null,
            tool_call: None,
        };
        let (title, body) = note(&input, Event::PreCompact, "**user**\n\nhi");
        assert_eq!(title, "Session abcdef12 before compaction (auto)");
        assert!(body.starts_with(&format!("# {title}\n")));
        assert!(body.contains("/home/user/.claude/projects/p/s.jsonl"));
        assert!(body.contains("## Last exchanges\n\n**user**\n\nhi"));
        let (title, body) = note(
            &HookInput {
                hook_event_name: "SessionEnd".into(),
                session_id: "s".into(),
                transcript_path: None,
                trigger: None,
                reason: Some("other".into()),
                tool_name: None,
                tool_input: Value::Null,
                tool_call: None,
            },
            Event::SessionEnd,
            "",
        );
        assert_eq!(title, "Session s at session end (other)");
        assert!(body.contains("(nothing yet)"));
    }

    #[test]
    fn notes_are_read_oldest_first_and_typed_by_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("2-sessionend.md"), "# End\n\nbye\n").unwrap();
        std::fs::write(dir.path().join("1-precompact.md"), "# Compact\n\nfull\n").unwrap();
        std::fs::write(dir.path().join("3-other.md"), "# Not ours\n").unwrap();
        std::fs::write(dir.path().join("x.tmp"), "").unwrap();
        let n = notes(dir.path()).unwrap();
        assert_eq!(n.len(), 2);
        assert_eq!(n[0].event, Event::PreCompact);
        assert_eq!(n[0].title, "Compact");
        assert_eq!(n[1].event, Event::SessionEnd);
        assert!(notes(&dir.path().join("missing")).unwrap().is_empty());
    }

    fn read(p: &str) -> Value {
        serde_json::json!({ "file_path": p })
    }

    #[test]
    fn a_read_of_an_entry_file_is_a_get_and_the_summaries_page_is_free() {
        let view = Path::new("/memory");
        let mut asked = Vec::new();
        let d = decide(
            "Read",
            &read("/memory/dom-me/mem-me-abc1.md"),
            view,
            SID,
            |id| {
                asked.push(id.to_string());
                Ok(())
            },
        );
        assert_eq!(
            (d, asked),
            (Decision::Allow, vec!["mem-me-abc1".to_string()])
        );
        let d = decide(
            "Read",
            &read("/memory/dom-me/mem-me-abc1.md"),
            view,
            SID,
            |_| Err("not found: mem-me-abc1".into()),
        );
        match d {
            Decision::Deny(why) => assert!(why.contains("not found") && why.contains("search")),
            Decision::Allow => panic!("a refusal is a denial"),
        }
        assert_eq!(
            decide(
                "Read",
                &read("/memory/dom-me/README.md"),
                view,
                SID,
                |_| panic!("free")
            ),
            Decision::Allow
        );
        // a path folded through `..` is the entry's, and is fetched
        let mut folded = Vec::new();
        let d = decide(
            "Read",
            &read("/memory/../memory/dom-me/mem-me-abc1.md"),
            view,
            SID,
            |id| {
                folded.push(id.to_string());
                Ok(())
            },
        );
        assert_eq!(
            (d, folded),
            (Decision::Allow, vec!["mem-me-abc1".to_string()])
        );
        assert!(matches!(
            decide(
                "NotebookRead",
                &serde_json::json!({ "notebook_path": "/memory/dom-me/mem-me-abc1.md" }),
                view,
                SID,
                |_| Err("not found".into())
            ),
            Decision::Deny(_)
        ));
        let out = Decision::Deny("why".into()).output("claude").unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
        assert_eq!(Decision::Allow.output("claude"), None);
        let input: HookInput = serde_json::from_str(
            r#"{"hook_event_name":"PreToolUse","tool_name":"Read","tool_input":{"file_path":"/memory/a/mem-a-1.md"}}"#,
        )
        .unwrap();
        assert_eq!(input.tool_name.as_deref(), Some("Read"));
    }

    #[test]
    fn gemini_s_before_tool_is_a_pre_tool_event_answered_with_a_decision() {
        // Gemini CLI names the event BeforeTool and reads a top-level decision
        assert!(pre_tool_event("BeforeTool") && pre_tool_event("PreToolUse"));
        assert!(!pre_tool_event("AfterTool"));
        let v: Value =
            serde_json::from_str(&Decision::Deny("why".into()).output("gemini").unwrap()).unwrap();
        assert_eq!(
            (v["decision"].as_str(), v["reason"].as_str()),
            (Some("deny"), Some("why"))
        );
        assert_eq!(Decision::Allow.output("gemini"), None, "silence is allowed");
        // Codex and Qwen Code read Claude Code's shape
        for h in ["codex", "qwen"] {
            let out = Decision::Deny("why".into()).output(h).unwrap();
            let v: Value = serde_json::from_str(&out).unwrap();
            assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny", "{h}");
        }
    }

    // the hook runs inside the Linux sandbox; these are unix paths
    #[cfg(unix)]
    #[test]
    fn the_file_tools_of_gemini_qwen_and_codex_are_known_by_their_own_argument_names() {
        let view = Path::new("/memory");
        // Gemini's read_file took absolute_path before file_path; both are the file
        let mut asked = Vec::new();
        let d = decide(
            "read_file",
            &serde_json::json!({ "absolute_path": "/memory/dom-me/mem-me-abc1.md" }),
            view,
            SID,
            |id| {
                asked.push(id.to_string());
                Ok(())
            },
        );
        assert_eq!(
            (d, asked),
            (Decision::Allow, vec!["mem-me-abc1".to_string()])
        );
        // list_directory names its directory dir_path
        assert!(matches!(
            decide(
                "list_directory",
                &serde_json::json!({ "dir_path": "/memory/dom-me" }),
                view,
                SID,
                |_| panic!("a list is not a get")
            ),
            Decision::Deny(_)
        ));
        // Codex's grep_files and Gemini's older search_file_content over a directory
        // that holds the projection are searches over bodies
        for tool in ["grep_files", "search_file_content"] {
            assert!(
                matches!(
                    decide(
                        tool,
                        &serde_json::json!({ "pattern": "x", "path": "/" }),
                        view,
                        SID,
                        |_| panic!("a search is not a get")
                    ),
                    Decision::Deny(_)
                ),
                "{tool}"
            );
        }
    }

    #[test]
    fn a_read_of_many_files_is_a_search_when_any_of_them_reaches_the_projection() {
        let view = Path::new("/memory");
        // read_many_files is a read of many files: refused when any is the projection's
        // or holds it, free when none is
        let many = |paths: &[&str]| {
            decide(
                "read_many_files",
                &serde_json::json!({ "paths": paths }),
                view,
                SID,
                |_| panic!("many is not one"),
            )
        };
        assert!(matches!(
            many(&["/work/a.md", "/memory/dom-me/mem-me-abc1.md"]),
            Decision::Deny(_)
        ));
        assert!(matches!(many(&["/"]), Decision::Deny(_)));
        assert_eq!(many(&["/work/a.md", "/work/b.md"]), Decision::Allow);
        assert_eq!(many(&[]), Decision::Allow);
        // a sibling whose name merely starts with the projection's is not the projection,
        // and two dots inside a name are not a climb
        assert_eq!(
            many(&["/memory-notes/*.md", "/work/a..b.md"]),
            Decision::Allow
        );
        // `include` is the spelling Gemini CLI's read_many_files sends today
        assert!(matches!(
            decide(
                "read_many_files",
                &serde_json::json!({ "include": ["/memory/**/*.md"] }),
                view,
                SID,
                |_| panic!("many is not one")
            ),
            Decision::Deny(_)
        ));
    }

    // the hook runs inside the Linux sandbox; these are unix paths
    #[cfg(unix)]
    #[test]
    fn grok_s_hook_sends_both_spellings_at_once_and_wants_a_decision() {
        // Grok Build writes its camelCase keys and the snake_case aliases side by side,
        // with Claude's PascalCase event name under the snake key only
        let text = r#"{"hookEventName":"pre_tool_use","hook_event_name":"PreToolUse","sessionId":"s1","session_id":"s1","cwd":"/work","workspaceRoot":"/work","timestamp":"t","toolName":"read_file","tool_name":"read_file","toolUseId":"u1","toolInput":{"target_file":"/memory/dom-me/mem-me-abc1.md"},"tool_input":{"target_file":"/memory/dom-me/mem-me-abc1.md"},"toolInputTruncated":false}"#;
        let input = HookInput::parse(text).unwrap();
        assert_eq!(input.hook_event_name, "PreToolUse");
        assert_eq!(input.session_id, "s1");
        let (tool, args) = input.tool();
        assert_eq!(tool, "read_file");
        let mut asked = Vec::new();
        let d = decide("read_file", &args, Path::new("/memory"), SID, |id| {
            asked.push(id.to_string());
            Ok(())
        });
        assert_eq!(
            (d, asked),
            (Decision::Allow, vec!["mem-me-abc1".to_string()])
        );
        // its own argument names for a list and a search
        assert!(matches!(
            decide(
                "list_dir",
                &serde_json::json!({ "target_directory": "/memory/dom-me" }),
                Path::new("/memory"),
                SID,
                |_| panic!("a list is not a get")
            ),
            Decision::Deny(_)
        ));
        assert!(matches!(
            decide(
                "grep",
                &serde_json::json!({ "pattern": "x", "path": "/", "glob": "*.md" }),
                Path::new("/memory"),
                SID,
                |_| panic!("a search is not a get")
            ),
            Decision::Deny(_)
        ));
        // a decision either way, as agy wants
        let v: Value = serde_json::from_str(&Decision::Allow.output("grok").unwrap()).unwrap();
        assert_eq!(v["decision"], "allow");
        let v: Value =
            serde_json::from_str(&Decision::Deny("why".into()).output("grok").unwrap()).unwrap();
        assert_eq!(
            (v["decision"].as_str(), v["reason"].as_str()),
            (Some("deny"), Some("why"))
        );
        // the other shapes still parse through the same door
        assert_eq!(
            HookInput::parse(r#"{"hook_event_name":"SessionEnd","session_id":"a"}"#)
                .unwrap()
                .hook_event_name,
            "SessionEnd"
        );
        assert_eq!(
            HookInput::parse(r#"{"hookEventName":"Stop","conversationId":"c"}"#)
                .unwrap()
                .session_id,
            "c"
        );
    }

    #[test]
    fn grok_s_unreadable_input_is_refused_and_its_history_is_read_beside_the_log() {
        // Grok truncates a payload over its size cap to one JSON string and lets a
        // tool through when the hook says nothing; the answer to input that cannot be
        // read is a refusal there, and silence where silence already refuses
        let v: Value = serde_json::from_str(&unreadable("grok").unwrap()).unwrap();
        assert_eq!(v["decision"], "deny");
        assert!(v["reason"].as_str().is_some_and(|r| !r.is_empty()));
        assert_eq!(unreadable("claude"), None);
        assert_eq!(unreadable("gemini"), None);
        // the hook is handed updates.jsonl, the ACP log; the chat history beside it is
        // the transcript the tail reader knows
        let dir = tempfile::tempdir().unwrap();
        let updates = dir.path().join("updates.jsonl");
        std::fs::write(&updates, "{}").unwrap();
        assert_eq!(transcript_file(&updates), updates, "nothing beside it yet");
        let history = dir.path().join("chat_history.jsonl");
        std::fs::write(&history, "{}").unwrap();
        assert_eq!(transcript_file(&updates), history);
        let other = dir.path().join("t.jsonl");
        std::fs::write(&other, "{}").unwrap();
        assert_eq!(
            transcript_file(&other),
            other,
            "only the ACP log has a twin"
        );
    }

    // the hook runs inside the Linux sandbox; these are unix paths
    #[cfg(unix)]
    #[test]
    fn antigravity_s_hook_shape_is_read_and_answered() {
        // agy wants a decision either way: an empty answer is refused as invalid
        let v: Value =
            serde_json::from_str(&Decision::Allow.output("antigravity").unwrap()).unwrap();
        assert_eq!(v["decision"], "allow");
        let v: Value =
            serde_json::from_str(&Decision::Deny("why".into()).output("antigravity").unwrap())
                .unwrap();
        assert_eq!(
            (v["decision"].as_str(), v["reason"].as_str()),
            (Some("deny"), Some("why"))
        );
        // agy's shape of the same event
        let agy: HookInput = serde_json::from_str(
            r#"{"hookEventName":"PreToolUse","conversationId":"c1","workspacePaths":["/work"],"transcriptPath":"/home/user/t.jsonl","toolCall":{"name":"view_file","args":{"AbsolutePath":"/memory/dom-me/mem-me-abc1.md","StartLine":1}},"stepIdx":3}"#,
        )
        .unwrap();
        assert_eq!(agy.hook_event_name, "PreToolUse");
        assert_eq!(agy.session_id, "c1");
        let (tool, input) = agy.tool();
        assert_eq!(tool, "view_file");
        assert!(reads_one(&tool));
        // agy's own argument names, PascalCase: a read is a get, a search is refused
        let view = Path::new("/memory");
        let mut fetched = Vec::new();
        let d = decide(&tool, &input, view, SID, |id| {
            fetched.push(id.to_string());
            Ok(())
        });
        assert_eq!(
            (d, fetched),
            (Decision::Allow, vec!["mem-me-abc1".to_string()])
        );
        for (tool, input) in [
            (
                "grep_search",
                serde_json::json!({ "SearchPath": "/memory", "Query": "key" }),
            ),
            (
                "list_dir",
                serde_json::json!({ "DirectoryPath": "/memory/dom-me" }),
            ),
            (
                "find_by_name",
                serde_json::json!({ "SearchDirectory": "/work", "Pattern": "/memory/**" }),
            ),
            (
                "find_by_name",
                serde_json::json!({ "SearchDirectory": "/", "Pattern": "*.md" }),
            ),
        ] {
            assert!(
                matches!(
                    decide(tool, &input, view, SID, |_| Ok(())),
                    Decision::Deny(_)
                ),
                "{tool} {input}"
            );
        }
        assert_eq!(
            decide(
                "grep_search",
                &serde_json::json!({ "SearchPath": "/work", "Query": "x" }),
                view,
                SID,
                |_| Ok(())
            ),
            Decision::Allow
        );
        assert_eq!(Event::from_name("Stop"), Some(Event::SessionEnd));
    }

    #[test]
    fn a_recall_page_is_free_by_place_not_by_name() {
        let view = Path::new("/memory");
        assert_eq!(
            decide(
                "Read",
                &read("/memory/recall/sess-abc12345/20261004T101010-000000001.md"),
                view,
                SID,
                |_| panic!("free: served when the recall ran")
            ),
            Decision::Allow
        );
        // by place, not by name: a file beside the pages, a `recall` directory inside a
        // domain, and an entry file in a domain named `recall` are not pages
        for p in [
            "/memory/recall/sess-abc12345/notes.txt",
            "/memory/recall/20261004T101010-000000001.md",
            "/memory/recall/sess-other999/20261004T101010-000000001.md",
            "/memory/dom-me/recall/sess-abc12345/20261004T101010-000000001.md",
        ] {
            assert!(
                matches!(
                    decide("Read", &read(p), view, SID, |_| Ok(())),
                    Decision::Deny(_)
                ),
                "{p}"
            );
        }
        let mut fetched = Vec::new();
        decide(
            "Read",
            &read("/memory/recall/mem-recall-abc1.md"),
            view,
            SID,
            |id| {
                fetched.push(id.to_string());
                Ok(())
            },
        );
        assert_eq!(
            fetched,
            vec!["mem-recall-abc1".to_string()],
            "an entry file under a domain called recall is still a get"
        );
    }

    #[test]
    fn under_the_projection_only_an_entry_file_opens_and_outside_it_nothing_is_decided() {
        let view = Path::new("/memory");
        // a stray name, a leftover sibling, a path folded out and back in to a stray name
        for p in [
            "/memory/dom-me/planted.md",
            "/memory/dom-me/mem-me-abc1.md.tmp",
            "/memory/../etc/passwd/../../memory/dom-me/x",
        ] {
            assert!(
                matches!(
                    decide("Read", &read(p), view, SID, |_| Ok(())),
                    Decision::Deny(_)
                ),
                "{p}"
            );
        }
        for p in [
            "/work/src/main.rs",
            "/memory-else/x.md",
            "/home/user/.memory-view/x.md",
        ] {
            assert_eq!(
                decide("Read", &read(p), view, SID, |_| panic!("not ours")),
                Decision::Allow,
                "{p}"
            );
        }
        for tool in ["Edit", "Write", "MultiEdit"] {
            assert!(matches!(
                decide(
                    tool,
                    &read("/memory/dom-me/mem-me-abc1.md"),
                    view,
                    SID,
                    |_| Ok(())
                ),
                Decision::Deny(_)
            ));
        }
        assert_eq!(
            decide(
                "Bash",
                &serde_json::json!({ "command": "cat /memory/x" }),
                view,
                SID,
                |_| Ok(())
            ),
            Decision::Allow,
            "a shell is not observed here; the plan says so"
        );
    }

    // the hook runs inside the Linux sandbox; these are unix paths
    #[cfg(unix)]
    #[test]
    fn a_search_over_the_projection_or_over_what_holds_it_is_refused() {
        let view = Path::new("/memory");
        for input in [
            serde_json::json!({ "pattern": "key", "path": "/memory/dom-me" }),
            serde_json::json!({ "pattern": "key", "path": "/" }),
            serde_json::json!({ "pattern": "key", "path": "/memory/.." }),
            serde_json::json!({ "pattern": "/memory/**/*.md" }),
        ] {
            for tool in ["Grep", "Glob"] {
                assert!(
                    matches!(
                        decide(tool, &input, view, SID, |_| Ok(())),
                        Decision::Deny(_)
                    ),
                    "{tool} {input}"
                );
            }
        }
    }

    // the hook runs inside the Linux sandbox; these are unix paths
    #[cfg(unix)]
    #[test]
    fn a_glob_s_pattern_is_a_place_and_a_grep_s_is_text() {
        let view = Path::new("/memory");
        // what the second pass got through: a pattern reaching the projection beside a
        // path that does not
        for input in [
            serde_json::json!({ "pattern": "/memory/**/*.md", "path": "/work" }),
            serde_json::json!({ "pattern": "../memory/**/*.md", "path": "/work" }),
            serde_json::json!({ "pattern": "/mem*/**/*.md", "path": "/work" }),
            serde_json::json!({ "pattern": "/memory/**/*.md", "path": "" }),
        ] {
            assert!(
                matches!(
                    decide("Glob", &input, view, SID, |_| Ok(())),
                    Decision::Deny(_)
                ),
                "{input}"
            );
        }
        // a grep's pattern is a regex over text: the projection's name in it is a word
        for input in [
            serde_json::json!({ "pattern": "/memory", "path": "/work" }),
            serde_json::json!({ "pattern": "INSIDE_PROJECTION = \"/memory\"", "path": "/work" }),
        ] {
            assert_eq!(
                decide("Grep", &input, view, SID, |_| Ok(())),
                Decision::Allow,
                "{input}"
            );
        }
        assert_eq!(
            decide(
                "Glob",
                &serde_json::json!({ "pattern": "**/memory/**", "path": "/work" }),
                view,
                SID,
                |_| Ok(())
            ),
            Decision::Allow,
            "a relative glob in the working tree"
        );
        assert_eq!(
            decide(
                "Glob",
                &serde_json::json!({ "pattern": "/memory-notes/*.md", "path": "/work" }),
                view,
                SID,
                |_| Ok(())
            ),
            Decision::Allow,
            "a sibling whose name starts with the projection's"
        );
        // the same tree under another name
        let mut via_proc = Vec::new();
        let d = decide(
            "Read",
            &read("/proc/self/root/memory/dom-me/mem-me-abc1.md"),
            view,
            SID,
            |id| {
                via_proc.push(id.to_string());
                Ok(())
            },
        );
        assert_eq!(
            (d, via_proc),
            (Decision::Allow, vec!["mem-me-abc1".to_string()])
        );
        assert!(matches!(
            decide(
                "Read",
                &read("/proc/1/root/memory/dom-me/stray.md"),
                view,
                SID,
                |_| Ok(())
            ),
            Decision::Deny(_)
        ));
        for input in [
            serde_json::json!({ "pattern": "key", "path": "/work" }),
            serde_json::json!({ "pattern": "key" }),
            serde_json::json!({ "pattern": "**/*.md", "path": "/memor" }),
            // Claude Code's own spelling of the working tree, and its parent
            serde_json::json!({ "pattern": "key", "path": "." }),
            serde_json::json!({ "pattern": "key", "path": ".." }),
            serde_json::json!({ "pattern": "key", "path": "./src" }),
        ] {
            assert_eq!(
                decide("Grep", &input, view, SID, |_| Ok(())),
                Decision::Allow,
                "beside the projection, or in the working tree: {input}"
            );
        }
    }

    fn read_input(p: &str) -> HookInput {
        serde_json::from_str(&format!(
            r#"{{"hook_event_name":"PreToolUse","tool_name":"Read","tool_input":{{"file_path":"{p}"}}}}"#
        ))
        .unwrap()
    }

    #[tokio::test]
    async fn the_hook_fails_closed_without_a_server_or_without_an_answer() {
        let view = Path::new("/memory");
        let entry = read_input("/memory/dom-me/mem-me-abc1.md");
        let wait = std::time::Duration::from_millis(200);
        assert_eq!(
            pre_tool_with(&entry, None, SID, None, wait).await,
            Decision::Allow,
            "no projection, nothing to decide"
        );
        assert!(
            matches!(
                pre_tool_with(&entry, Some(view), SID, None, wait).await,
                Decision::Deny(_)
            ),
            "a projection with no server address is refused, not let through"
        );
        assert_eq!(
            pre_tool_with(
                &read_input("/work/src/main.rs"),
                Some(view),
                SID,
                None,
                wait
            )
            .await,
            Decision::Allow,
            "outside the projection nothing is decided, server or not"
        );
        // a server that accepts and never answers
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let hold = tokio::spawn(async move {
            let mut kept = Vec::new();
            loop {
                if let Ok((s, _)) = listener.accept().await {
                    kept.push(s);
                }
            }
        });
        let started = std::time::Instant::now();
        let d = pre_tool_with(&entry, Some(view), SID, Some(&url), wait).await;
        hold.abort();
        assert!(
            matches!(&d, Decision::Deny(why) if why.contains("did not answer")),
            "{d:?}"
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }
}
