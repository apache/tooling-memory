//! The read funnel's telemetry: every retrieval the server answers, with the tokens it
//! served, appended as JSON Lines under the state directory. The paper's efficiency
//! numbers (tokens per task, retrieval depth) are computed from this, without the agent
//! reporting anything.

use dm_core::approx_tokens;
use dm_index::Hit;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
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
    /// Who it read as: the owner, or an agent. Empty in lines written before this was kept.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub author: String,
    /// t0, search, list, get, recall, or prefetch: the summaries a claimed task's prompt
    /// carried.
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
    /// curator's signals are computed from, co-retrieval and opened bodies.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ids: Vec<String>,
    /// How a recall was served: `brief`, or `pack:sections` and `pack:summaries`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    /// How long the server took to answer, in milliseconds: kept for a search. Absent
    /// in lines written before this was kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ms: Option<u64>,
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
    /// Who the session reads as: the owner, or the agent it was opened for.
    author: Mutex<String>,
    lock: Mutex<()>,
    enabled: AtomicBool,
    /// The entries served since the last decision context was kept. In memory only.
    returned: Mutex<Returned>,
    /// The digest of the T0 last served. In memory only.
    t0: Mutex<Option<String>>,
}

/// Entry ids, each once, in the order they were first served.
#[derive(Default)]
struct Returned {
    order: Vec<String>,
    seen: HashSet<String>,
}

impl Returned {
    fn add(&mut self, ids: &[String]) {
        for id in ids {
            if self.seen.insert(id.clone()) {
                self.order.push(id.clone());
            }
        }
    }
}

/// A line appended to a JSONL file under the state directory; a failure is dropped,
/// as a log's is.
pub fn append_line(path: &Path, line: &str) {
    let _ = try_append_line(path, line);
}

/// A line appended to a JSONL file under the state directory, for a caller that has to
/// know whether it was kept.
///
/// # Errors
///
/// The file could not be opened or written.
pub fn try_append_line(path: &Path, line: &str) -> std::io::Result<()> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(f, "{line}")
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
            author: Mutex::new(String::new()),
            lock: Mutex::new(()),
            enabled: AtomicBool::new(true),
            returned: Mutex::new(Returned::default()),
            t0: Mutex::new(None),
        }
    }

    /// The entries served since this was last asked, each once, in the order they were
    /// first served. The answer is taken: the next call starts from nothing. A decision
    /// context keeps it, so a context names what its session was served since the
    /// context before it.
    pub fn take_returned(&self) -> Vec<String> {
        let mut r = self
            .returned
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::mem::take(&mut *r).order
    }

    /// The digest of the T0 last served to this session; none when none was.
    pub fn last_t0(&self) -> Option<String> {
        self.t0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Record reads as this author's from now on.
    pub fn set_author(&self, author: &str) {
        *self
            .author
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = author.to_string();
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
        self.record(tool, query, (hits, tokens), cached, via, ids, None);
    }

    /// Append one read, and trace it. `size` is the hits and the tokens served.
    #[allow(clippy::too_many_arguments)] // one per thing a read is
    fn record(
        &self,
        tool: &str,
        query: Option<&str>,
        size: (usize, usize),
        cached: bool,
        via: Option<&str>,
        ids: &[String],
        ms: Option<u64>,
    ) {
        if !self.enabled.load(Ordering::Relaxed) {
            return;
        }
        self.returned
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .add(ids);
        let read = Read {
            at: time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_default(),
            session: self.session.clone(),
            author: self
                .author
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
            tool: tool.to_string(),
            query: query.map(str::to_string),
            hits: size.0,
            tokens: size.1,
            cached,
            ids: ids.to_vec(),
            via: via.map(str::to_string),
            ms,
        };
        trace(&read);
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

    /// A search answered with these hits, in `ms` milliseconds.
    pub fn searched(&self, query: &str, hits: &[Hit], ms: u64) {
        let ids: Vec<String> = hits.iter().map(|h| h.id.clone()).collect();
        let size = (hits.len(), hits_tokens(hits));
        self.record("search", Some(query), size, false, None, &ids, Some(ms));
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
        if tool == "t0" && self.enabled.load(Ordering::Relaxed) {
            *self
                .t0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(dm_core::digest(text));
        }
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

/// The same read as a span for a collector: an instant, since a read is answered whole.
fn trace(read: &Read) {
    if !dm_otel::enabled() {
        return;
    }
    let mut attrs: Vec<(&str, dm_otel::Value<'_>)> = vec![
        ("memory.session", read.session.as_str().into()),
        ("memory.tool", read.tool.as_str().into()),
        ("memory.hits", read.hits.into()),
        ("memory.tokens", read.tokens.into()),
        ("memory.cached", read.cached.into()),
        ("memory.ids", read.ids.join(",").into()),
    ];
    if let Some(q) = &read.query {
        attrs.push(("memory.query", q.as_str().into()));
    }
    if let Some(v) = &read.via {
        attrs.push(("memory.via", v.as_str().into()));
    }
    if let Some(ms) = read.ms {
        attrs.push(("memory.ms", ms.into()));
    }
    dm_otel::event("memory.read", &attrs);
}

/// What a page of hits costs the reader: id, title, summary and a little metadata each,
/// and the snippet of a hit that has one.
#[must_use]
pub fn hits_tokens(hits: &[Hit]) -> usize {
    hits.iter()
        .map(|h| {
            let snippet = h.snippet.as_deref().map_or(0, approx_tokens);
            approx_tokens(&h.title) + approx_tokens(&h.summary) + snippet + 12
        })
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

/// How many times each pair of entries came back from one search, over a read log: the
/// first five hits of every search that `keep` admits, as a pair in id order. The
/// curator's merge signal and the prefetch's association are this one count.
#[must_use]
pub fn co_retrieved(
    reads: &[Read],
    keep: impl Fn(&str) -> bool,
) -> BTreeMap<(String, String), usize> {
    let mut together: BTreeMap<(String, String), usize> = BTreeMap::new();
    for r in reads.iter().filter(|r| r.tool == "search") {
        let ids: Vec<&String> = r.ids.iter().filter(|i| keep(i)).take(5).collect();
        for (i, a) in ids.iter().enumerate() {
            for b in &ids[i + 1..] {
                let key = if a < b {
                    ((*a).clone(), (*b).clone())
                } else {
                    ((*b).clone(), (*a).clone())
                };
                *together.entry(key).or_default() += 1;
            }
        }
    }
    together
}

/// How many searches returned each entry, over a read log: the first five hits of every
/// search, the same rule as [`co_retrieved`]. A pair count over this count is the share
/// of the searches for one entry that returned the other with it.
#[must_use]
pub fn searches_with(reads: &[Read]) -> BTreeMap<String, usize> {
    let mut searches: BTreeMap<String, usize> = BTreeMap::new();
    for r in reads.iter().filter(|r| r.tool == "search") {
        for id in r.ids.iter().take(5) {
            *searches.entry(id.clone()).or_default() += 1;
        }
    }
    searches
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn searches_with_counts_the_first_five_hits_of_each_search() {
        let dir = tempfile::tempdir().unwrap();
        let log = ReadLog::new(dir.path(), "sess-1");
        let ids = |names: &[&str]| -> Vec<String> { names.iter().map(|n| (*n).into()).collect() };
        let six = ids(&["a", "b", "c", "d", "e", "f"]);
        log.served_ids("search", Some("q"), 6, 10, false, None, &six);
        log.served_ids("search", Some("q"), 2, 10, false, None, &ids(&["a", "f"]));
        log.served_ids("list", None, 1, 10, false, None, &ids(&["a"]));
        let n = searches_with(&all(dir.path()).unwrap());
        assert_eq!(n.get("a"), Some(&2), "a list is no search");
        assert_eq!(n.get("e"), Some(&1));
        assert_eq!(
            n.get("f"),
            Some(&1),
            "the sixth hit of a search does not count"
        );
    }

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

    #[test]
    fn a_line_without_the_time_of_the_answer_reads_back_and_a_search_keeps_it() {
        let dir = tempfile::tempdir().unwrap();
        let old = r#"{"at":"2026-10-10T00:00:00Z","session":"s-old","tool":"search","query":"import","hits":2,"tokens":40,"ids":["mem-a","mem-b"]}"#;
        std::fs::create_dir_all(dir.path().join("state")).unwrap();
        std::fs::write(path(dir.path()), format!("{old}\n")).unwrap();
        let log = ReadLog::new(dir.path(), "sess-new");
        log.searched("import", &[], 7);
        log.text("t0", "a text");
        let reads = all(dir.path()).unwrap();
        assert_eq!(reads.len(), 3, "the old line parses: {reads:?}");
        assert_eq!((reads[0].session.as_str(), reads[0].ms), ("s-old", None));
        assert_eq!(reads[0].ids, ["mem-a", "mem-b"]);
        assert_eq!((reads[1].tool.as_str(), reads[1].ms), ("search", Some(7)));
        assert_eq!(reads[2].ms, None, "only a search is timed");
        let text = std::fs::read_to_string(path(dir.path())).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[1].contains(r#""ms":7"#), "{}", lines[1]);
        assert!(
            !lines[2].contains("\"ms\""),
            "an untimed read has no ms: {}",
            lines[2]
        );
    }

    #[test]
    fn replay_state_a_read_log_keeps_the_ids_served_and_the_digest_of_t0() {
        let dir = tempfile::tempdir().unwrap();
        let log = ReadLog::new(dir.path(), "sess-1");
        let ids = |xs: &[&str]| xs.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        assert!(log.take_returned().is_empty() && log.last_t0().is_none());
        log.served_ids("search", None, 2, 9, false, None, &ids(&["mem-b", "mem-a"]));
        log.opened("mem-a", "The body.", false);
        log.opened("mem-c", "Another body.", false);
        assert_eq!(
            log.take_returned(),
            ids(&["mem-b", "mem-a", "mem-c"]),
            "each id once, in the order first served"
        );
        assert!(log.take_returned().is_empty(), "taken");
        log.opened("mem-a", "The body.", false);
        assert_eq!(log.take_returned(), ids(&["mem-a"]), "served again");
        // only a T0 is the T0
        log.text("transcripts", "A transcript.");
        assert_eq!(log.last_t0(), None);
        log.text("t0", "The first T0.");
        log.text("t0", "The second T0.");
        assert_eq!(log.last_t0(), Some(dm_core::digest("The second T0.")));
        // a disabled log keeps nothing
        log.disable();
        log.opened("mem-d", "A body.", false);
        log.text("t0", "A third T0.");
        assert!(log.take_returned().is_empty());
        assert_eq!(log.last_t0(), Some(dm_core::digest("The second T0.")));
    }

    #[test]
    fn reads_are_traced_while_the_log_is_enabled() {
        use opentelemetry::Value;
        use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let _guard = dm_otel::install(provider);
        let dir = tempfile::tempdir().unwrap();
        let log = ReadLog::new(dir.path(), "sess-traced");
        log.served_ids(
            "search",
            Some("import"),
            2,
            90,
            false,
            Some("brief"),
            &["mem-a".to_string(), "mem-b".to_string()],
        );
        log.disable();
        log.text("t0", "not a session's read");
        let spans: Vec<_> = exporter
            .get_finished_spans()
            .unwrap()
            .into_iter()
            .filter(|s| {
                s.attributes.iter().any(|kv| {
                    kv.key.as_str() == "memory.session" && kv.value.as_str() == "sess-traced"
                })
            })
            .collect();
        assert_eq!(spans.len(), 1, "one read traced; the disabled log's is not");
        let s = &spans[0];
        assert_eq!(s.name, "memory.read");
        let attr = |k: &str| {
            s.attributes
                .iter()
                .find(|kv| kv.key.as_str() == k)
                .map(|kv| kv.value.clone())
        };
        assert_eq!(attr("memory.tool"), Some(Value::from("search")));
        assert_eq!(attr("memory.query"), Some(Value::from("import")));
        assert_eq!(attr("memory.hits"), Some(Value::I64(2)));
        assert_eq!(attr("memory.tokens"), Some(Value::I64(90)));
        assert_eq!(attr("memory.cached"), Some(Value::Bool(false)));
        assert_eq!(attr("memory.via"), Some(Value::from("brief")));
        assert_eq!(attr("memory.ids"), Some(Value::from("mem-a,mem-b")));
    }
}
