//! What a session started from the web launcher says about itself while it runs: where
//! its model calls go and at what ceiling, the label it has reached, what the calls have
//! cost so far, and what it has written. The launcher is the one process that knows all
//! of it, so it writes the file and the web launcher reads it for the session's strip.
//!
//! The web launcher names the session in `AMEM_WEB_SESSION`: an id, never a path, checked
//! before it becomes one, under `state/web/` in the root.

use dm_core::Label;
use dm_egress::Telemetry;
use dm_server::Memory;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// How often a running session's file is written.
const EVERY: Duration = Duration::from_secs(2);

/// The web launcher's id for this session, if it started it: 32 hex digits.
#[must_use]
pub fn web_session() -> Option<String> {
    std::env::var("AMEM_WEB_SESSION")
        .ok()
        .filter(|id| id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// What does not change over the session.
#[derive(Clone, Debug, Serialize)]
pub struct Fixed {
    /// The memory's session id, which telemetry is kept under.
    pub memory_session: String,
    /// The harness's binary name.
    pub harness: String,
    /// subscription, key or gateway.
    pub auth: String,
    /// The destination the model calls go to.
    pub destination: String,
    /// The grants entry whose ceiling caps the session.
    pub ceiling_of: String,
    /// The highest label the destination may receive.
    pub ceiling: Label,
    /// Whether the egress meters the calls; a tunnel is pinned without metering.
    pub metered: bool,
    /// The model, when one was named.
    pub model: Option<String>,
}

/// Calls and tokens so far.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Usage {
    /// Model calls.
    pub calls: u64,
    /// Input tokens.
    pub input: u64,
    /// Output tokens.
    pub output: u64,
}

#[derive(Serialize)]
struct Snapshot<'a> {
    #[serde(flatten)]
    fixed: &'a Fixed,
    /// The highest label the session has read.
    label: Label,
    usage: Usage,
    written: usize,
    handoffs: usize,
    running: bool,
    updated: String,
}

fn usage(telemetry: Option<&Telemetry>, session: &str) -> Usage {
    telemetry
        .and_then(|t| t.session(session).ok().flatten())
        .map(|s| Usage {
            calls: s.calls,
            input: s.input_tokens,
            output: s.output_tokens,
        })
        .unwrap_or_default()
}

/// What one write needs, behind one lock so the last word is never overtaken by a
/// write that was already under way when the session ended.
struct Source {
    path: PathBuf,
    fixed: Fixed,
    memory: Arc<Memory>,
    telemetry: Option<Telemetry>,
}

fn write(src: &Source, running: bool) {
    let (written, handoffs) = src.memory.written();
    let snap = Snapshot {
        fixed: &src.fixed,
        label: src.memory.label(),
        usage: usage(src.telemetry.as_ref(), &src.fixed.memory_session),
        written,
        handoffs,
        running,
        updated: time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default(),
    };
    let Ok(text) = serde_json::to_string(&snap) else {
        return;
    };
    // written beside and renamed, so a reader never sees half a file
    let tmp = src.path.with_extension("json.tmp");
    if std::fs::write(&tmp, text).is_ok() {
        let _ = std::fs::rename(&tmp, &src.path);
    }
}

/// The writer for one session: a task that rewrites the file while the session runs, and
/// a last write, saying it stopped, when it is dropped.
pub struct Writer {
    source: Arc<std::sync::Mutex<Source>>,
    task: tokio::task::JoinHandle<()>,
}

fn locked(m: &std::sync::Mutex<Source>) -> std::sync::MutexGuard<'_, Source> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Writer {
    /// Start writing, when the web launcher named this session.
    #[must_use]
    pub fn start(root: &Path, fixed: Fixed, memory: Arc<Memory>) -> Option<Writer> {
        let path = dm_web::state::status_file(root, &web_session()?)?;
        std::fs::create_dir_all(path.parent()?).ok()?;
        let source = Arc::new(std::sync::Mutex::new(Source {
            path,
            fixed,
            memory,
            telemetry: Telemetry::open(&root.join("state/telemetry.db")).ok(),
        }));
        let s = source.clone();
        // file and database work, off the runtime the egress proxy serves on
        let task = tokio::spawn(async move {
            loop {
                let s = s.clone();
                let _ = tokio::task::spawn_blocking(move || write(&locked(&s), true)).await;
                tokio::time::sleep(EVERY).await;
            }
        });
        Some(Writer { source, task })
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.task.abort();
        // the lock waits out a write already under way, so this one is the last
        write(&locked(&self.source), false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_id_becomes_a_path() {
        let root = Path::new("/r");
        let id = "0123456789abcdef0123456789abcdef";
        assert_eq!(
            dm_web::state::status_file(root, id).unwrap(),
            root.join("state/web").join(format!("{id}.json"))
        );
        for bad in [
            "../../etc/passwd",
            "",
            "0123",
            "0123456789abcdef0123456789abcdeg",
            &format!("{id}/.."),
        ] {
            assert!(dm_web::state::status_file(root, bad).is_none(), "{bad}");
        }
    }

    #[tokio::test]
    async fn the_file_says_what_the_session_has_done_and_when_it_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let m = Arc::new(Memory::init(dir.path(), "human:test:me", "dom-me").unwrap());
        let fixed = Fixed {
            memory_session: m.session_id().to_string(),
            harness: "claude".into(),
            auth: "gateway".into(),
            destination: "llmao".into(),
            ceiling_of: "llmao".into(),
            ceiling: Label::Internal,
            metered: true,
            model: Some("m1".into()),
        };
        let p = dir.path().join("s.json");
        let t = Telemetry::open(&dir.path().join("t.db")).unwrap();
        t.record(
            m.session_id(),
            "llmao",
            200,
            &dm_egress::Usage {
                model: Some("m1".into()),
                input_tokens: 120,
                output_tokens: 30,
                ..dm_egress::Usage::default()
            },
        )
        .unwrap();
        m.write(dm_server::WriteRequest {
            entry_type: dm_core::EntryType::Handoff,
            body: "Stopped.\n".into(),
            ..dm_server::WriteRequest::default()
        })
        .await
        .unwrap();
        let src = Source {
            path: p.clone(),
            fixed,
            memory: m.clone(),
            telemetry: Some(t),
        };
        write(&src, false);
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(v["destination"], "llmao");
        assert_eq!(v["ceiling"], "internal");
        assert_eq!(v["usage"]["calls"], 1);
        assert_eq!(v["usage"]["input"], 120);
        assert_eq!(v["written"], 1);
        assert_eq!(v["handoffs"], 1);
        assert_eq!(v["running"], false);
        assert_eq!(v["model"], "m1");
    }
}
