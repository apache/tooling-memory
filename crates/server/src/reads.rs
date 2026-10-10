//! The read funnel's telemetry: every retrieval the server answers, with the tokens it
//! served, appended as JSON Lines under the state directory. The paper's efficiency
//! numbers (tokens per task, retrieval depth) are computed from this, without the agent
//! reporting anything.

use dm_core::approx_tokens;
use dm_index::Hit;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
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
    /// The entries served: a page's hit ids, or the one a get opened. What the
    /// librarian's signals are computed from, co-retrieval and opened bodies.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ids: Vec<String>,
    /// How a recall was served: `brief`, or `pack:sections` and `pack:summaries`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
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
    enabled: AtomicBool,
}

/// A line appended to a JSONL file under the state directory; a failure is dropped,
/// as a log's is.
pub fn append_line(path: &Path, line: &str) {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).ok();
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{line}");
    }
}

/// One write, as the write log records it: which session wrote which entry, so a
/// poisoned or erased entry's reach can be followed from what its readers wrote.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Wrote {
    /// When.
    pub at: String,
    /// Which session.
    pub session: String,
    /// The entry written.
    pub id: String,
    /// Its author.
    pub author: String,
    /// The entry it replaced, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
}

/// Record a write.
pub fn wrote(root: &Path, session: &str, id: &str, author: &str, supersedes: Option<&str>) {
    let w = Wrote {
        at: time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default(),
        session: session.to_string(),
        id: id.to_string(),
        author: author.to_string(),
        supersedes: supersedes.map(str::to_string),
    };
    if let Ok(line) = serde_json::to_string(&w) {
        append_line(&root.join("state/writes.jsonl"), &line);
    }
}

/// Every write recorded under a root.
///
/// # Errors
///
/// When the log exists and cannot be read.
pub fn all_writes(root: &Path) -> std::io::Result<Vec<Wrote>> {
    let path = root.join("state/writes.jsonl");
    if !path.exists() {
        return Ok(Vec::new());
    }
    Ok(std::fs::read_to_string(path)?
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect())
}

impl ReadLog {
    /// A log for this session under `root`.
    #[must_use]
    pub fn new(root: &Path, session: &str) -> ReadLog {
        ReadLog {
            path: path(root),
            session: session.to_string(),
            lock: Mutex::new(()),
            enabled: AtomicBool::new(true),
        }
    }

    /// Stop recording: for a reader whose reads are not a session's, such as a report.
    pub fn disable(&self) {
        self.enabled.store(false, Ordering::Relaxed);
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
        self.served_via(tool, query, hits, tokens, cached, None);
    }

    /// As [`ReadLog::served`], with how it was served: a recall's regime.
    pub fn served_via(
        &self,
        tool: &str,
        query: Option<&str>,
        hits: usize,
        tokens: usize,
        cached: bool,
        via: Option<&str>,
    ) {
        self.served_ids(tool, query, hits, tokens, cached, via, &[]);
    }

    /// As [`ReadLog::served_via`], naming the entries served.
    #[allow(clippy::too_many_arguments)] // one per thing a read is
    pub fn served_ids(
        &self,
        tool: &str,
        query: Option<&str>,
        hits: usize,
        tokens: usize,
        cached: bool,
        via: Option<&str>,
        ids: &[String],
    ) {
        if !self.enabled.load(Ordering::Relaxed) {
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
            ids: ids.to_vec(),
            via: via.map(str::to_string),
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
        let ids: Vec<String> = hits.iter().map(|h| h.id.clone()).collect();
        self.served_ids(
            tool,
            query,
            hits.len(),
            hits_tokens(hits),
            false,
            None,
            &ids,
        );
    }

    /// Text served whole: a T0, or an entry's body.
    pub fn text(&self, tool: &str, text: &str) {
        self.append(tool, None, 1, approx_tokens(text));
    }

    /// An entry's body served whole, by id; `cached` says a pack had already pushed it.
    pub fn opened(&self, id: &str, text: &str, cached: bool) {
        self.served_ids(
            "get",
            None,
            1,
            approx_tokens(text),
            cached,
            None,
            &[id.to_string()],
        );
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
