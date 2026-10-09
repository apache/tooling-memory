//! Non-interactive runs: a prompt, or a task claimed from the ledger.
//!
//! The launcher reaches the ledger and the record the way a harness does, through the
//! memory server's tool surface, over an in-process pipe. The ledger's rules stay on the
//! server; this module knows how to turn a task into a prompt and an exit into a handoff.

use crate::hooks::{self, Event};
use crate::Session;
use dm_client::{MemoryClient, Task};
use dm_sandbox::Harness;
use dm_server::{mcp, Memory};
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

type Fallible<T> = Result<T, Box<dyn std::error::Error>>;

/// Exit code: nothing was claimable.
pub const NOTHING_TO_DO: i32 = 3;
/// Exit code: the harness stopped without completing, and the task went back.
pub const STOPPED: i32 = 4;

/// How `work` runs.
pub struct Options {
    /// Claim from this domain; the only granted domain when omitted.
    pub domain: Option<String>,
    /// Lease length; heartbeats come every third of it.
    pub lease_seconds: i64,
    /// Keep claiming until interrupted.
    pub keep_going: bool,
    /// How long to wait when the queue is empty, with `keep_going`.
    pub idle: Duration,
    /// Mine the transcript after each task, when set.
    pub mine: Option<MinePlan>,
}

/// How to mine after a task.
pub struct MinePlan {
    /// The model, or the destination's small default.
    pub model: Option<String>,
    /// Through a prompted run of the harness rather than a direct call.
    pub via_harness: bool,
}

/// A client joined to this process's memory server by a byte pipe: the same JSON-RPC
/// a harness speaks, with no socket between.
///
/// # Errors
///
/// When the MCP handshake fails.
pub async fn client(memory: Arc<Memory>) -> Fallible<MemoryClient> {
    let (client_io, server_io) = tokio::io::duplex(256 * 1024);
    let (sr, sw) = tokio::io::split(server_io);
    tokio::spawn(async move {
        if let Err(e) = mcp::serve_pipe(memory, sr, sw).await {
            eprintln!("memory: in-process server: {e}");
        }
    });
    let (cr, cw) = tokio::io::split(client_io);
    Ok(MemoryClient::connect((cr, cw)).await?)
}

async fn handoff_ids(client: &MemoryClient, domain: Option<&str>) -> Fallible<BTreeSet<String>> {
    Ok(client
        .handoffs(domain, 100)
        .await?
        .into_iter()
        .map(|(id, _)| id)
        .collect())
}

/// File what the harness's hooks left in the outbox. A note from before compaction is
/// always filed: what it holds is gone from the harness's context. A note from session
/// end is filed only when the run left no handoff of its own, as the handoff of last
/// resort; otherwise it is dropped. Returns the ids filed.
///
/// # Errors
///
/// When the outbox cannot be read or the memory server refuses a write.
pub async fn file_outbox(
    session: &Session,
    client: &MemoryClient,
    wrote_handoff: bool,
) -> Fallible<Vec<String>> {
    let mut filed = Vec::new();
    for note in hooks::notes(&session.outbox())? {
        if note.event == Event::SessionEnd && wrote_handoff {
            std::fs::remove_file(&note.path).ok();
            continue;
        }
        let id = client.write_handoff(&note.title, &note.body, None).await?;
        eprintln!(
            "memory: handoff {id} from the harness's hook ({})",
            note.event.suffix()
        );
        std::fs::remove_file(&note.path).ok();
        filed.push(id);
    }
    Ok(filed)
}

/// An interactive run: the harness as the person knows it, with the outbox filed when
/// it exits.
///
/// # Errors
///
/// When the harness cannot be run or the memory server cannot be reached.
pub async fn interactive(session: &Session, args: &[String]) -> Fallible<i32> {
    let client = client(session.memory()).await?;
    let before = handoff_ids(&client, None).await?;
    let (code, _) = session.run(args, false).await?;
    let after = handoff_ids(&client, None).await?;
    file_outbox(session, &client, after.difference(&before).next().is_some()).await?;
    client.close().await.ok();
    Ok(code)
}

/// Run one prompt in the harness's headless form. Unless `no_handoff`, a run that left
/// no handoff exits [`STOPPED`]: a non-interactive run whose outcome is only in its
/// transcript has left nothing for the next session.
///
/// # Errors
///
/// When the harness cannot be run or the memory server cannot be reached.
pub async fn prompted(
    session: &Session,
    prompt: &str,
    args: &[String],
    no_handoff: bool,
) -> Fallible<i32> {
    let client = client(session.memory()).await?;
    let before = handoff_ids(&client, None).await?;
    // the harness's own arguments follow its headless form, where every harness takes
    // a flag after the prompt
    let mut argv = session.harness().headless(prompt);
    argv.extend(args.iter().cloned());
    let (code, output) = session.run(&argv, true).await?;
    let output = output.map(|o| answer_of(session, o));
    if let Some(out) = &output {
        print!("{out}");
        if !out.ends_with('\n') {
            println!();
        }
    }
    let mut after = handoff_ids(&client, None).await?;
    let wrote = after.difference(&before).next().is_some();
    after.extend(file_outbox(session, &client, wrote).await?);
    let new: Vec<&String> = after.difference(&before).collect();
    if new.is_empty() && !no_handoff {
        eprintln!(
            "memory: the run left no handoff; the next session starts from nothing (pass --no-handoff if that is intended)"
        );
        client.close().await.ok();
        return Ok(if code == 0 { STOPPED } else { code });
    }
    for id in new {
        eprintln!("memory: handoff {id}");
    }
    client.close().await.ok();
    Ok(code)
}

/// What a headless run printed, as the answer. Muse Code's `exec --json` prints its
/// record stream, one JSON object a line: that stream is kept under the private home
/// as the run's transcript, since Muse's own session store is binary, and the answer is
/// the text of its `run.terminal.completed` record. Every other harness prints the
/// answer itself.
fn answer_of(session: &Session, output: String) -> String {
    if session.harness() != Harness::Muse {
        return output;
    }
    let dir = session.home().join(".local/share/muse/exec");
    if std::fs::create_dir_all(&dir).is_ok() {
        let stamp = time::OffsetDateTime::now_utc().unix_timestamp_nanos();
        std::fs::write(dir.join(format!("{stamp}.jsonl")), &output).ok();
    }
    muse_answer(&output).unwrap_or(output)
}

/// The final text of a Muse Code record stream, when it holds one: what the run said
/// when it completed, or why it failed.
fn muse_answer(jsonl: &str) -> Option<String> {
    jsonl
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|v| {
            let text = match v.get("payload_type").and_then(serde_json::Value::as_str)? {
                "run.terminal.completed" => v.pointer("/payload/text")?.as_str()?.to_string(),
                "run.terminal.failed" => {
                    let reason = v
                        .pointer("/payload/reason")
                        .and_then(serde_json::Value::as_str);
                    format!(
                        "muse: the run failed: {}",
                        reason.unwrap_or("no reason given")
                    )
                }
                _ => return None,
            };
            Some(text)
        })
        .next_back()
}

/// Claim and run tasks.
///
/// # Errors
///
/// When the harness cannot be run or the memory server cannot be reached.
pub async fn run(session: &Session, options: &Options) -> Fallible<i32> {
    let client = Arc::new(client(session.memory()).await?);
    let mut last = NOTHING_TO_DO;
    loop {
        let claimed = client
            .claim_next(options.domain.as_deref(), options.lease_seconds)
            .await?;
        let Some(task) = claimed else {
            if !options.keep_going {
                eprintln!("memory: nothing claimable");
                return Ok(if last == NOTHING_TO_DO {
                    NOTHING_TO_DO
                } else {
                    last
                });
            }
            tokio::time::sleep(options.idle).await;
            continue;
        };
        last = one(session, &client, &task, options.lease_seconds).await?;
        if let Some(plan) = &options.mine {
            let via = if plan.via_harness {
                crate::MineVia::Harness
            } else {
                crate::MineVia::Api
            };
            if let Err(e) = session.mine(plan.model.as_deref(), via).await {
                eprintln!("memory: mining failed: {e}");
            }
        }
        if !options.keep_going {
            return Ok(last);
        }
    }
}

async fn one(
    session: &Session,
    client: &Arc<MemoryClient>,
    task: &Task,
    lease_seconds: i64,
) -> Fallible<i32> {
    eprintln!("memory: claimed {}: {}", task.id, task.title);
    let prompt = prompt_for(client, task).await?;
    // the reads above were the launcher's: neither the fan-out rule nor the pushed set
    // should carry them into the harness's session
    session.memory().harness_starts();
    let heartbeat = {
        let client = client.clone();
        let id = task.id.clone();
        tokio::spawn(async move {
            let every = Duration::from_secs((lease_seconds / 3).max(5).unsigned_abs());
            let mut tick = tokio::time::interval(every);
            tick.tick().await;
            loop {
                tick.tick().await;
                if let Err(e) = client.heartbeat(&id, lease_seconds).await {
                    eprintln!("memory: heartbeat on {id}: {e}");
                    return;
                }
            }
        })
    };
    let run = session
        .run_task(&session.harness().headless(&prompt), true, Some(&task.id))
        .await;
    heartbeat.abort();
    let (code, output) = run?;
    let output = output.map(|o| answer_of(session, o));
    let now = client.get_task(&task.id).await?;
    file_outbox(session, client, now.status == "completed").await?;
    let output = output.unwrap_or_default();
    if !output.trim().is_empty() {
        print!("{output}");
        if !output.ends_with('\n') {
            println!();
        }
    }
    settle(session.harness(), client, task, code, output).await
}

/// After the harness exits: a completed task stays complete; a task still held goes
/// back to the pool with the harness's output filed as a handoff.
async fn settle(
    harness: Harness,
    client: &MemoryClient,
    task: &Task,
    code: i32,
    output: String,
) -> Fallible<i32> {
    let now = client.get_task(&task.id).await?;
    match now.status.as_str() {
        "completed" => {
            eprintln!(
                "memory: {} completed, handoff {}",
                task.id,
                now.handoff.as_deref().unwrap_or("-")
            );
            Ok(0)
        }
        "claimed" => {
            let tail = tail_of(&output, 6000);
            let body = format!(
                "# {title}\n\n`{harness}` stopped without completing task {id} (exit {code}). \
                 The task is back in the pool; what follows is what it printed, so the next \
                 claimant can start from here.\n\n## Output\n\n{tail}\n",
                title = task.title,
                harness = harness.binary(),
                id = task.id,
            );
            let handoff = client
                .write_handoff(
                    &format!("Task {} stopped: {}", task.id, task.title),
                    &body,
                    Some(&task.domain),
                )
                .await?;
            client.release(&task.id, Some(&handoff)).await?;
            eprintln!(
                "memory: {} released; its output is filed as handoff {handoff}",
                task.id
            );
            Ok(STOPPED)
        }
        other => {
            eprintln!("memory: {} is now {other}", task.id);
            Ok(if code == 0 { STOPPED } else { code })
        }
    }
}

fn tail_of(s: &str, chars: usize) -> String {
    let s = s.trim();
    if s.is_empty() {
        return "(nothing)".into();
    }
    let n = s.chars().count();
    if n <= chars {
        return s.to_string();
    }
    let skip = n - chars;
    format!("…\n{}", s.chars().skip(skip).collect::<String>())
}

/// The prompt: the task, what the tasks it depends on left behind, T0 for its domain,
/// and how to stop.
async fn prompt_for(client: &MemoryClient, task: &Task) -> Fallible<String> {
    let mut p = String::new();
    p.push_str(
        "You are working one task from a shared task ledger, reached through the `memory` MCP server. \
         The launcher holds the lease and renews it for you; do not call `heartbeat`.\n\n",
    );
    p.push_str(&format!(
        "# Task {}: {}\n\n{}\n",
        task.id,
        task.title,
        task.body.trim()
    ));
    if !task.requirements.is_empty() {
        p.push_str(&format!("\nRequires: {}\n", task.requirements.join(", ")));
    }
    if let Some(h) = &task.handoff {
        if let Ok(e) = client.get(h).await {
            p.push_str(&format!(
                "\n# Where the last attempt stopped ({h})\n\n{}\n",
                e.body.trim()
            ));
        }
    }
    if !task.depends_on.is_empty() {
        p.push_str("\n# What the tasks this one depends on left behind\n");
        p.push_str(
            "\nYour first act is to verify these against what this task assumes: an entry's `review_by` may have passed, a decision may have been superseded, a handoff may describe a world that has moved on. Check what matters with `get` and `search`. A mismatch is not absorbed silently: say so in your handoff and release the task, so the board can re-decompose it.\n",
        );
        for dep in &task.depends_on {
            let Ok(d) = client.get_task(dep).await else {
                continue;
            };
            match &d.handoff {
                Some(h) => match client.get(h).await {
                    Ok(e) => p.push_str(&format!(
                        "\n## {dep}: {} → handoff {h}\n\n{}\n",
                        d.title,
                        tail_of(&e.body, 4000)
                    )),
                    Err(_) => p.push_str(&format!(
                        "\n## {dep}: {} → handoff {h} (unreadable here)\n",
                        d.title
                    )),
                },
                None => p.push_str(&format!("\n## {dep}: {} ({})\n", d.title, d.status)),
            }
        }
    }
    let t0 = client.t0(Some(&task.domain)).await?;
    p.push_str(&format!("\n# T0 for {}\n\n{}\n", task.domain, t0.trim()));
    p.push_str(&format!(
        "\n# When you stop\n\nWrite a `handoff` entry with the `write` tool (entry_type handoff): what you did, \
         what is left, where to look. Then call `complete` with task id `{id}` and the handoff's id. \
         If you cannot finish, still write the handoff and stop; do not call `complete`, and the task \
         returns to the pool with your handoff attached.\n",
        id = task.id
    ));
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_keeps_the_end() {
        assert_eq!(tail_of("  ", 10), "(nothing)");
        assert_eq!(tail_of("short", 10), "short");
        let t = tail_of("abcdefghij", 4);
        assert!(t.ends_with("ghij"), "{t}");
        assert!(t.starts_with('…'));
    }

    #[test]
    fn muse_s_answer_is_the_terminal_record_s_text() {
        let stream = concat!(
            r#"{"payload_type":"turn.input.user","payload":{"prompt":"hi"}}"#,
            "\n",
            r#"{"payload_type":"run.output.delta","payload":{"text":"gate"}}"#,
            "\n",
            r#"{"payload_type":"run.terminal.completed","payload":{"text":"gateway"}}"#,
            "\n",
        );
        assert_eq!(muse_answer(stream).as_deref(), Some("gateway"));
        assert!(muse_answer("plain text\n").is_none());
        let failed =
            r#"{"payload_type":"run.terminal.failed","payload":{"reason":"API error 400"}}"#;
        assert_eq!(
            muse_answer(failed).as_deref(),
            Some("muse: the run failed: API error 400")
        );
    }
}
