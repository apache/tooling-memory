//! Mining a transcript for durable knowledge: a model reads the session's tail and
//! proposes entries, which land at raw tier with low confidence and the transcript as
//! their source, for a person to promote or drop. The third source of memory the harness
//! did not write, after the hooks and the kept transcript; the only one that costs a
//! model call, made through the same egress and destination as the session's own calls.

use crate::hooks::transcript_tail;
pub use crate::model::{answer_text, default_model};
use dm_client::{MemoryClient, NewEntry};
use dm_egress::Egress;
use serde::Deserialize;
use serde_json::Value;
use std::path::Path;

type Fallible<T> = Result<T, Box<dyn std::error::Error>>;

/// What the model is asked to return, one per entry.
#[derive(Debug, Deserialize, PartialEq, Eq)]
pub struct Proposal {
    /// fact, decision, convention or method.
    pub kind: String,
    /// One line.
    pub title: String,
    /// Markdown.
    pub body: String,
}

const INSTRUCTIONS: &str = "You are a librarian for a shared memory record used by agents and people. \
Read the transcript excerpt and extract only knowledge that would help a future session that has \
not seen it: facts about the codebase or system, decisions and their reasons, conventions, methods \
that worked. Leave out chit-chat, tool noise, and anything already obvious from the task text. \
Answer with a JSON array only, no prose, each element {\"kind\": \"fact\"|\"decision\"|\"convention\"|\"method\", \
\"title\": one line, \"body\": Markdown, a paragraph or two}. An empty array is a fine answer.";

/// The prompt, for a model reached directly or through a harness.
#[must_use]
pub fn prompt_for(excerpt: &str) -> String {
    format!("{INSTRUCTIONS}\n\n## Transcript excerpt\n\n{excerpt}")
}

/// The excerpt a transcript yields, or none when there is nothing to read.
///
/// # Errors
///
/// When the transcript cannot be read.
pub fn excerpt_of(transcript: &Path) -> std::io::Result<Option<String>> {
    let text = std::fs::read_to_string(transcript)?;
    let excerpt = transcript_tail(&text, 60, 40_000);
    Ok((!excerpt.trim().is_empty()).then_some(excerpt))
}

/// Write proposals as entries: raw tier, low confidence, the transcript as source.
///
/// # Errors
///
/// When a write fails.
pub async fn write_proposals(
    client: &MemoryClient,
    found: &[Proposal],
    transcript: &Path,
    model: &str,
) -> Fallible<Vec<String>> {
    let source = format!("transcript:{}", transcript.display());
    let mut ids = Vec::new();
    for p in found {
        let id = client
            .write_entry(&NewEntry {
                kind: &p.kind,
                title: &p.title,
                body: &format!(
                    "{}\n\n*Mined from a session transcript by {model}; raw, low confidence, \
                     for a person to confirm or drop.*\n",
                    p.body.trim()
                ),
                confidence: Some("low"),
                source: vec![source.clone()],
                domain: None,
            })
            .await?;
        eprintln!("memory: mined {} {id}: {}", p.kind, p.title);
        ids.push(id);
    }
    Ok(ids)
}

/// The mining request for a destination's API shape.
#[must_use]
pub fn request(destination: &str, model: &str, excerpt: &str) -> (String, Value) {
    crate::model::request(destination, model, &prompt_for(excerpt))
}

/// Proposals out of the model's text, tolerant of a code fence around the JSON.
#[must_use]
pub fn proposals(text: &str) -> Vec<Proposal> {
    let t = text.trim();
    let t = t
        .strip_prefix("```json")
        .or_else(|| t.strip_prefix("```"))
        .unwrap_or(t);
    let t = t.strip_suffix("```").unwrap_or(t).trim();
    let start = t.find('[').unwrap_or(0);
    let end = t.rfind(']').map_or(t.len(), |i| i + 1);
    let slice = t.get(start..end).unwrap_or(t);
    serde_json::from_str::<Vec<Proposal>>(slice)
        .unwrap_or_default()
        .into_iter()
        .filter(|p| {
            matches!(
                p.kind.as_str(),
                "fact" | "decision" | "convention" | "method"
            ) && !p.title.trim().is_empty()
                && p.body.split_whitespace().count() >= 5
        })
        .collect()
}

/// Mine one transcript: ask the model through the egress, write what comes back at raw
/// tier and low confidence with the transcript as source. Returns the ids written.
///
/// # Errors
///
/// When the transcript cannot be read, the destination refuses or fails, or a write fails.
pub async fn mine(
    egress: &Egress,
    client: &MemoryClient,
    destination: &str,
    model: &str,
    transcript: &Path,
) -> Fallible<Vec<String>> {
    let Some(excerpt) = excerpt_of(transcript)? else {
        return Ok(Vec::new());
    };
    let (path, body) = request(destination, model, &excerpt);
    let answer = egress.call(destination, &path, &body).await?;
    let found = proposals(&answer_text(destination, &answer));
    write_proposals(client, &found, transcript, model).await
}

/// The newest transcript a harness left under its private home: Claude Code's project
/// transcripts, Codex's rollouts, Gemini's chats. Any `.jsonl` outside the outbox.
#[must_use]
pub fn newest_transcript(home: &Path) -> Option<std::path::PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<(std::time::SystemTime, std::path::PathBuf)>, depth: u8) {
        if depth > 8 {
            return;
        }
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.file_name().is_some_and(|n| n == ".memory-outbox") {
                continue;
            }
            if p.is_dir() {
                walk(&p, out, depth + 1);
            } else if p.extension().is_some_and(|x| x == "jsonl") {
                // agy keeps a full log beside the transcript proper, and Grok Build
                // three logs beside its chat history; the proper one is the shape the
                // tail reader knows
                let twin = p.with_file_name("transcript.jsonl");
                if p.file_name().is_some_and(|n| n == "transcript_full.jsonl") && twin.is_file() {
                    continue;
                }
                let history = p.with_file_name("chat_history.jsonl");
                let grok_log = p.file_name().is_some_and(|n| {
                    n == "updates.jsonl" || n == "prompt_history.jsonl" || n == "events.jsonl"
                });
                if grok_log && history.is_file() {
                    continue;
                }
                if let Ok(m) = p.metadata().and_then(|m| m.modified()) {
                    out.push((m, p));
                }
            }
        }
    }
    let mut found = Vec::new();
    walk(home, &mut found, 0);
    found.into_iter().max_by_key(|(m, _)| *m).map(|(_, p)| p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_follow_the_destination_shape() {
        let (p, b) = request("anthropic", "claude-haiku-4-5-20251001", "x");
        assert_eq!(p, "v1/messages");
        assert_eq!(b["model"], "claude-haiku-4-5-20251001");
        assert!(b["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("librarian"));
        let (p, _) = request("gemini", "gemini-2.5-flash", "x");
        assert_eq!(p, "v1beta/models/gemini-2.5-flash:generateContent");
    }

    #[test]
    fn proposals_are_filtered() {
        let text = "```json\n[{\"kind\":\"fact\",\"title\":\"The import has five steps\",\"body\":\"Steps one to five, the fourth is slow.\"},\
            {\"kind\":\"gossip\",\"title\":\"x\",\"body\":\"a b c d e f\"},{\"kind\":\"decision\",\"title\":\"short\",\"body\":\"too short\"}]\n```";
        let p = proposals(text);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].kind, "fact");
        assert!(proposals("no json here").is_empty());
        assert!(proposals("[]").is_empty());
    }

    #[test]
    fn the_newest_transcript_is_found_and_the_outbox_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join(".claude/projects/p");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::write(a.join("old.jsonl"), "{}").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let b = dir.path().join(".codex/sessions/2026/10/03");
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(b.join("rollout.jsonl"), "{}").unwrap();
        std::fs::create_dir_all(dir.path().join(".memory-outbox")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(dir.path().join(".memory-outbox/x.jsonl"), "{}").unwrap();
        let newest = newest_transcript(dir.path()).unwrap();
        assert!(newest.ends_with("rollout.jsonl"), "{newest:?}");
        assert!(newest_transcript(&dir.path().join("none")).is_none());
        // Grok keeps four logs side by side; the chat history is the one in a shape
        // the tail reader knows, however the others' times fall
        std::thread::sleep(std::time::Duration::from_millis(20));
        let g = dir.path().join(".grok/sessions/work/s1");
        std::fs::create_dir_all(&g).unwrap();
        std::fs::write(g.join("chat_history.jsonl"), "{}").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        for sibling in ["updates.jsonl", "prompt_history.jsonl", "events.jsonl"] {
            std::fs::write(g.join(sibling), "{}").unwrap();
        }
        let newest = newest_transcript(dir.path()).unwrap();
        assert!(newest.ends_with("chat_history.jsonl"), "{newest:?}");
    }
}
