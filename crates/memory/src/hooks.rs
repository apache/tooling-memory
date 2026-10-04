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
    /// Which event.
    pub hook_event_name: String,
    /// The harness's own session id.
    #[serde(default)]
    pub session_id: String,
    /// The transcript, a JSON Lines file.
    #[serde(default)]
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
    /// The hook's standard output, in Claude Code's shape; nothing for an allow.
    #[must_use]
    pub fn output(&self) -> Option<String> {
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

/// The path a tool call is about, for the tools that touch files: `file_path` for a
/// read or an edit, `notebook_path` for a notebook, `path` for a search over a tree,
/// and for a search without one, its pattern when the pattern is a path.
fn path_of(input: &Value) -> Option<PathBuf> {
    ["file_path", "notebook_path", "path", "pattern"]
        .iter()
        .find_map(|k| input.get(*k).and_then(Value::as_str))
        .filter(|p| !p.is_empty())
        .map(|p| normalise(Path::new(p)))
}

/// A path with `.` and `..` folded away lexically, so `/memory/../memory/x` is
/// `/memory/x` and `/memory/..` is `/`.
fn normalise(path: &Path) -> PathBuf {
    use std::path::Component;
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

/// A tool that reads one file.
fn reads_one(tool: &str) -> bool {
    matches!(tool, "Read" | "read_file" | "view" | "NotebookRead")
}

/// A tool that searches a tree, by path or by glob.
fn searches(tool: &str) -> bool {
    matches!(
        tool,
        "Grep" | "Glob" | "grep" | "glob" | "search_files" | "list_directory"
    )
}

/// Decide a tool call against the projection at `projection`. A read of an entry file
/// is served through `get`, which returns the server's refusal in words when it has
/// one; a read of the summaries page is free, as a list is; any other name under the
/// projection is refused, whatever put it there; a search over the projection, or over
/// a directory that holds it, is refused, since a `grep -r` over bodies is a
/// full-corpus read the memory would not have served; so is a write, which the mount
/// refuses anyway. Anything outside the projection is not the hook's.
pub fn decide<F>(tool: &str, input: &Value, projection: &Path, get: F) -> Decision
where
    F: FnOnce(&str) -> Result<(), String>,
{
    let Some(path) = path_of(input) else {
        return Decision::Allow;
    };
    let projection = normalise(projection);
    let covers = searches(tool) && projection.starts_with(&path);
    if !path.starts_with(&projection) && !covers {
        return Decision::Allow;
    }
    let whole = format!(
        "{} is the memory's projection: its bodies are read one file at a time, each \
         read a `get` the memory serves and counts. Use the memory's `search` tool, or \
         open one entry file.",
        projection.display()
    );
    if !reads_one(tool) {
        return Decision::Deny(whole);
    }
    if path
        .file_name()
        .is_some_and(|n| n == dm_server::projection::SUMMARIES)
    {
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

/// The PreToolUse hook: decide against the projection the environment names, reaching
/// the memory server at the MCP URL the environment names. With neither set there is
/// no projection and nothing to decide.
async fn pre_tool(input: &HookInput) -> Decision {
    let (Some(projection), Some(url)) = (
        std::env::var_os("MEMORY_PROJECTION").map(PathBuf::from),
        std::env::var("MEMORY_MCP_URL").ok(),
    ) else {
        return Decision::Allow;
    };
    let tool = input.tool_name.clone().unwrap_or_default();
    // the id to fetch is decided first, then fetched: the decision is pure, the fetch is not
    let mut wanted: Option<String> = None;
    let d = decide(&tool, &input.tool_input, &projection, |id| {
        wanted = Some(id.to_string());
        Ok(())
    });
    let Some(id) = wanted else {
        return d;
    };
    match crate::mcp_http::call_tool(&url, "get", serde_json::json!({ "id": id })).await {
        Ok(_) => Decision::Allow,
        Err(why) => decide(&tool, &input.tool_input, &projection, |_| Err(why)),
    }
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
            "SessionEnd" => Some(Event::SessionEnd),
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
fn turns_in(v: &Value) -> Vec<Turn> {
    let keep = |role: &str, text: String| {
        let text = text.trim().to_string();
        (!text.is_empty() && text != "[tool result]").then(|| Turn {
            role: role.to_string(),
            text,
        })
    };
    if let Some(role) = v.get("type").and_then(Value::as_str) {
        if role == "user" || role == "assistant" {
            if let Some(c) = v.get("message").and_then(|m| m.get("content")) {
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
pub fn run(outbox: &Path) -> Fallible<()> {
    let mut text = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)?;
    let input: HookInput = serde_json::from_str(&text)?;
    if input.hook_event_name == "PreToolUse" {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        if let Some(out) = rt.block_on(pre_tool(&input)).output() {
            println!("{out}");
        }
        return Ok(());
    }
    let Some(event) = Event::from_name(&input.hook_event_name) else {
        return Ok(());
    };
    let transcript = input
        .transcript_path
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default();
    let tail = transcript_tail(&transcript, 10, 6000);
    let (_, body) = note(&input, event, &tail);
    std::fs::create_dir_all(outbox)?;
    let stamp = time::OffsetDateTime::now_utc().unix_timestamp_nanos();
    let name = format!("{stamp}-{}.md", event.suffix());
    let tmp = outbox.join(format!("{name}.tmp"));
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, outbox.join(name))?;
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
        assert_eq!(Event::from_name("PreCompress"), Some(Event::PreCompact));
        assert_eq!(Event::from_name("Stop"), None);
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
        let d = decide("Read", &read("/memory/dom-me/mem-me-abc1.md"), view, |id| {
            asked.push(id.to_string());
            Ok(())
        });
        assert_eq!(
            (d, asked),
            (Decision::Allow, vec!["mem-me-abc1".to_string()])
        );
        let d = decide("Read", &read("/memory/dom-me/mem-me-abc1.md"), view, |_| {
            Err("not found: mem-me-abc1".into())
        });
        match d {
            Decision::Deny(why) => assert!(why.contains("not found") && why.contains("search")),
            Decision::Allow => panic!("a refusal is a denial"),
        }
        assert_eq!(
            decide("Read", &read("/memory/dom-me/README.md"), view, |_| panic!(
                "free"
            )),
            Decision::Allow
        );
        // a path folded through `..` is the entry's, and is fetched
        let mut folded = Vec::new();
        let d = decide(
            "Read",
            &read("/memory/../memory/dom-me/mem-me-abc1.md"),
            view,
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
                |_| Err("not found".into())
            ),
            Decision::Deny(_)
        ));
        let out = Decision::Deny("why".into()).output().unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
        assert_eq!(Decision::Allow.output(), None);
        let input: HookInput = serde_json::from_str(
            r#"{"hook_event_name":"PreToolUse","tool_name":"Read","tool_input":{"file_path":"/memory/a/mem-a-1.md"}}"#,
        )
        .unwrap();
        assert_eq!(input.tool_name.as_deref(), Some("Read"));
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
                    decide("Read", &read(p), view, |_| Ok(())),
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
                decide("Read", &read(p), view, |_| panic!("not ours")),
                Decision::Allow,
                "{p}"
            );
        }
        for tool in ["Edit", "Write", "MultiEdit"] {
            assert!(matches!(
                decide(tool, &read("/memory/dom-me/mem-me-abc1.md"), view, |_| Ok(
                    ()
                )),
                Decision::Deny(_)
            ));
        }
        assert_eq!(
            decide(
                "Bash",
                &serde_json::json!({ "command": "cat /memory/x" }),
                view,
                |_| Ok(())
            ),
            Decision::Allow,
            "a shell is not observed here; the plan says so"
        );
    }

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
                    matches!(decide(tool, &input, view, |_| Ok(())), Decision::Deny(_)),
                    "{tool} {input}"
                );
            }
        }
        for input in [
            serde_json::json!({ "pattern": "key", "path": "/work" }),
            serde_json::json!({ "pattern": "key" }),
            serde_json::json!({ "pattern": "**/*.md", "path": "/memor" }),
        ] {
            assert_eq!(
                decide("Grep", &input, view, |_| Ok(())),
                Decision::Allow,
                "beside the projection, or in the working tree: {input}"
            );
        }
    }
}
