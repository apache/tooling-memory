//! The read funnel's telemetry: every retrieval the server answers, with the tokens it
//! served, appended as JSON Lines under the state directory. The paper's efficiency
//! numbers (tokens per task, retrieval depth) are computed from this, without the agent
//! reporting anything.

use dm_core::approx_tokens;
use dm_index::Hit;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// One retrieval event.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Read {
    /// RFC 3339.
    pub at: String,
    /// The session that read.
    pub session: String,
    /// t0, search, list or get.
    pub tool: String,
    /// The query, for search.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// How many hits came back, or one for a get.
    pub hits: usize,
    /// Tokens served, approximately.
    pub tokens: usize,
    /// Whether the body came from the session cache: pushed by a pack, then asked for.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cached: bool,
}

/// The log file under a root.
#[must_use]
pub fn path(root: &Path) -> PathBuf {
    root.join("state/reads.jsonl")
}

/// Appends reads; one per server.
pub struct ReadLog {
    path: PathBuf,
    session: String,
    lock: Mutex<()>,
    enabled: bool,
}

impl ReadLog {
    /// A log for this session under `root`.
    #[must_use]
    pub fn new(root: &Path, session: &str) -> ReadLog {
        ReadLog {
            path: path(root),
            session: session.to_string(),
            lock: Mutex::new(()),
            enabled: true,
        }
    }

    /// Stop recording: for a reader whose reads are not a session's, such as a report.
    pub fn disable(&mut self) {
        self.enabled = false;
    }

    fn append(&self, tool: &str, query: Option<&str>, hits: usize, tokens: usize) {
        self.served(tool, query, hits, tokens, false);
    }

    /// Anything served: a page of hits, a text, a pack, a brief. `cached` says the
    /// body was one a pack had already pushed to this session.
    pub fn served(
        &self,
        tool: &str,
        query: Option<&str>,
        hits: usize,
        tokens: usize,
        cached: bool,
    ) {
        if !self.enabled {
            return;
        }
        let read = Read {
            at: time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_default(),
            session: self.session.clone(),
            tool: tool.to_string(),
            query: query.map(str::to_string),
            hits,
            tokens,
            cached,
        };
        let _g = self
            .lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(p) = self.path.parent() {
            std::fs::create_dir_all(p).ok();
        }
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            if let Ok(line) = serde_json::to_string(&read) {
                let _ = writeln!(f, "{line}");
            }
        }
    }

    /// A list or search answered with these hits.
    pub fn hits(&self, tool: &str, query: Option<&str>, hits: &[Hit]) {
        self.append(tool, query, hits.len(), hits_tokens(hits));
    }

    /// Text served whole: a T0, or an entry's body.
    pub fn text(&self, tool: &str, text: &str) {
        self.append(tool, None, 1, approx_tokens(text));
    }
}

/// What a page of hits costs the reader: id, title, summary and a little metadata each.
#[must_use]
pub fn hits_tokens(hits: &[Hit]) -> usize {
    hits.iter()
        .map(|h| approx_tokens(&h.title) + approx_tokens(&h.summary) + 12)
        .sum()
}

/// Every read in a log, oldest first. A missing log is empty.
///
/// # Errors
///
/// When the file exists but cannot be read.
pub fn all(root: &Path) -> std::io::Result<Vec<Read>> {
    let p = path(root);
    if !p.exists() {
        return Ok(Vec::new());
    }
    Ok(std::fs::read_to_string(p)?
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_append_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        assert!(all(dir.path()).unwrap().is_empty());
        let log = ReadLog::new(dir.path(), "sess-1");
        log.text("t0", "one two three four five six");
        log.append("search", Some("import"), 3, 90);
        let reads = all(dir.path()).unwrap();
        assert_eq!(reads.len(), 2);
        assert_eq!(reads[0].tool, "t0");
        assert_eq!(reads[0].tokens, 6 + 2);
        assert_eq!(reads[1].query.as_deref(), Some("import"));
        assert_eq!(reads[1].session, "sess-1");
    }
}
