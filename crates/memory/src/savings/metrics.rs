//! The paper's retrieval metrics (§9), from the read log and the write log: fetch-through
//! rate, retrieval depth per task, prefetch precision and the wasted-read ratio, per
//! session and in total. Nothing here asks the agent; every number is a count over what
//! the server logged.
//!
//! The paper judges "used" with a sampled cheap-model judge. Here an entry is used when
//! the session's handoff cites it by id, which needs no model and undercounts what a
//! session used without saying so.

use dm_server::reads::{Read, Wrote};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

/// What "used" means in these numbers, for the report to say.
pub const USED: &str = "used means cited by id in the session's handoff; the paper samples a cheap-model judge, this needs no model";

/// What the metrics are computed from.
pub struct Inputs<'a> {
    /// The read log, oldest first.
    pub reads: &'a [Read],
    /// The write log: which session wrote which entry.
    pub writes: &'a [Wrote],
    /// Handoff entries by id, with the text a citation is looked for in.
    pub handoffs: &'a BTreeMap<String, String>,
    /// The handoffs of completed tasks, by entry id.
    pub completed: &'a BTreeSet<String>,
}

/// The four metrics as counts and the ratios they make.
#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct Counts {
    /// Distinct entries `search` served as hits.
    pub search_hits: usize,
    /// Of those, opened with `get` after the search served them.
    pub fetched_through: usize,
    /// `fetched_through` over `search_hits`.
    pub fetch_through_rate: Option<f64>,
    /// Completed tasks whose handoff the session wrote.
    pub tasks_completed: usize,
    /// Pulls: every search, list, get and recall, the pushes (T0, prefetch) left out.
    /// In the total, only sessions that completed a task.
    pub pulls: usize,
    /// `pulls` over `tasks_completed`: retrieval depth per task.
    pub retrieval_depth: Option<f64>,
    /// Distinct entries prefetched into the session's prompt.
    pub prefetched: usize,
    /// Of those, opened with `get` afterwards or cited in the session's handoff.
    pub prefetch_used: usize,
    /// `prefetch_used` over `prefetched`.
    pub prefetch_precision: Option<f64>,
    /// Distinct bodies opened with `get`. In the total, only sessions that left a
    /// handoff, since a session without one cannot be judged.
    pub opened: usize,
    /// Of those, bodies the session's handoff does not cite.
    pub opened_uncited: usize,
    /// `opened_uncited` over `opened`, for a session that left a handoff.
    pub wasted_read_ratio: Option<f64>,
}

/// One session's metrics.
#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct SessionMetrics {
    /// The session id.
    pub session: String,
    /// The handoffs it wrote.
    pub handoffs: Vec<String>,
    /// Its numbers.
    #[serde(flatten)]
    pub counts: Counts,
}

/// The metrics section of the savings report.
#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct Metrics {
    /// What "used" means here.
    pub used: String,
    /// Per session, those that searched, prefetched, opened or completed something.
    pub sessions: Vec<SessionMetrics>,
    /// Over every session.
    pub total: Counts,
}

fn ratio(part: usize, whole: usize) -> Option<f64> {
    (whole > 0).then(|| part as f64 / whole as f64)
}

/// Whether `text` names `id` as a whole token: `mem-a-1` is not cited by `mem-a-10`.
#[must_use]
pub fn cites(text: &str, id: &str) -> bool {
    let part = |c: char| c.is_alphanumeric() || c == '-' || c == '_';
    !id.is_empty()
        && text.match_indices(id).any(|(at, _)| {
            let before = text[..at].chars().next_back();
            let after = text[at + id.len()..].chars().next();
            !before.is_some_and(part) && !after.is_some_and(part)
        })
}

/// What one session's reads add up to, before the handoff is consulted.
#[derive(Default)]
struct Trail {
    searched: BTreeSet<String>,
    fetched: BTreeSet<String>,
    prefetched: BTreeSet<String>,
    prefetch_opened: BTreeSet<String>,
    opened: BTreeSet<String>,
    pulls: usize,
}

impl Trail {
    /// Follow the reads in order: a get counts as fetch-through only for an entry a
    /// search or a prefetch had served before it.
    fn of<'a>(reads: impl Iterator<Item = &'a Read>) -> Trail {
        let mut t = Trail::default();
        for r in reads {
            match r.tool.as_str() {
                "t0" => {}
                "prefetch" => t.prefetched.extend(r.ids.iter().cloned()),
                "search" => {
                    t.pulls += 1;
                    t.searched.extend(r.ids.iter().cloned());
                }
                "get" => {
                    t.pulls += 1;
                    for id in &r.ids {
                        if t.searched.contains(id) {
                            t.fetched.insert(id.clone());
                        }
                        if t.prefetched.contains(id) {
                            t.prefetch_opened.insert(id.clone());
                        }
                        t.opened.insert(id.clone());
                    }
                }
                _ => t.pulls += 1,
            }
        }
        t
    }

    fn counts(&self, citing: &[&str], tasks_completed: usize) -> Counts {
        let cited = |id: &String| citing.iter().any(|text| cites(text, id));
        let prefetch_used = self
            .prefetched
            .iter()
            .filter(|id| self.prefetch_opened.contains(*id) || cited(id))
            .count();
        let opened_uncited = if citing.is_empty() {
            0
        } else {
            self.opened.iter().filter(|id| !cited(id)).count()
        };
        Counts {
            search_hits: self.searched.len(),
            fetched_through: self.fetched.len(),
            fetch_through_rate: ratio(self.fetched.len(), self.searched.len()),
            tasks_completed,
            pulls: self.pulls,
            retrieval_depth: ratio(self.pulls, tasks_completed),
            prefetched: self.prefetched.len(),
            prefetch_used,
            prefetch_precision: ratio(prefetch_used, self.prefetched.len()),
            opened: self.opened.len(),
            opened_uncited,
            wasted_read_ratio: if citing.is_empty() {
                None
            } else {
                ratio(opened_uncited, self.opened.len())
            },
        }
    }
}

/// The total over sessions: counts summed, ratios recomputed from the sums.
fn total(sessions: &[SessionMetrics]) -> Counts {
    let mut t = Counts::default();
    for s in sessions {
        let c = &s.counts;
        t.search_hits += c.search_hits;
        t.fetched_through += c.fetched_through;
        t.prefetched += c.prefetched;
        t.prefetch_used += c.prefetch_used;
        if c.tasks_completed > 0 {
            t.tasks_completed += c.tasks_completed;
            t.pulls += c.pulls;
        }
        if !s.handoffs.is_empty() {
            t.opened += c.opened;
            t.opened_uncited += c.opened_uncited;
        }
    }
    t.fetch_through_rate = ratio(t.fetched_through, t.search_hits);
    t.retrieval_depth = ratio(t.pulls, t.tasks_completed);
    t.prefetch_precision = ratio(t.prefetch_used, t.prefetched);
    t.wasted_read_ratio = ratio(t.opened_uncited, t.opened);
    t
}

/// Compute the metrics.
#[must_use]
pub fn compute(inputs: &Inputs<'_>) -> Metrics {
    let mut by_session: BTreeMap<&str, Vec<&Read>> = BTreeMap::new();
    for r in inputs.reads {
        by_session.entry(r.session.as_str()).or_default().push(r);
    }
    let mut handoffs: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut completed: BTreeMap<&str, usize> = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for w in inputs.writes {
        if !seen.insert(w.id.as_str()) {
            continue;
        }
        if inputs.handoffs.contains_key(&w.id) {
            handoffs.entry(w.session.as_str()).or_default().push(&w.id);
            by_session.entry(w.session.as_str()).or_default();
        }
        if inputs.completed.contains(&w.id) {
            *completed.entry(w.session.as_str()).or_default() += 1;
        }
    }
    let mut sessions = Vec::new();
    for (session, reads) in by_session {
        let ids = handoffs.get(session).cloned().unwrap_or_default();
        let citing: Vec<&str> = ids
            .iter()
            .filter_map(|id| inputs.handoffs.get(*id).map(String::as_str))
            .collect();
        let counts = Trail::of(reads.into_iter())
            .counts(&citing, completed.get(session).copied().unwrap_or(0));
        if counts.search_hits + counts.prefetched + counts.opened + counts.tasks_completed == 0 {
            continue;
        }
        sessions.push(SessionMetrics {
            session: session.to_string(),
            handoffs: ids.iter().map(|i| (*i).to_string()).collect(),
            counts,
        });
    }
    let total = total(&sessions);
    Metrics {
        used: USED.to_string(),
        sessions,
        total,
    }
}

/// `part/whole (pct)`, or a dash when there is nothing to divide by.
fn cell(part: usize, whole: usize) -> String {
    match ratio(part, whole) {
        Some(r) => format!("{part}/{whole} {:.0}%", r * 100.0),
        None => "-".into(),
    }
}

fn depth_cell(c: &Counts) -> String {
    match c.retrieval_depth {
        Some(d) => format!("{d:.1} ({} tasks)", c.tasks_completed),
        None => "-".into(),
    }
}

fn row(name: &str, c: &Counts, judged: bool) -> String {
    let wasted = if judged {
        cell(c.opened_uncited, c.opened)
    } else {
        "-".into()
    };
    format!(
        "  {:<22} {:>14}  {:>14}  {:>14}  {:>12}\n",
        name,
        cell(c.fetched_through, c.search_hits),
        depth_cell(c),
        cell(c.prefetch_used, c.prefetched),
        wasted
    )
}

/// The metrics as text.
#[must_use]
pub fn render(m: &Metrics) -> String {
    let mut s =
        String::from("\nMetrics: the paper's retrieval metrics, from the read and write logs\n");
    s.push_str(&format!("  ({})\n", m.used));
    if m.sessions.is_empty() {
        s.push_str("  no session has searched, prefetched, opened or completed anything yet\n");
        return s;
    }
    s.push_str(&format!(
        "  {:<22} {:>14}  {:>14}  {:>14}  {:>12}\n",
        "session", "fetch-through", "depth/task", "prefetch prec.", "wasted reads"
    ));
    for x in &m.sessions {
        s.push_str(&row(&x.session, &x.counts, !x.handoffs.is_empty()));
    }
    s.push_str(&row("total", &m.total, m.total.opened > 0));
    s.push_str(
        "  fetch-through: search hits later opened with get; depth: pulls (search, list, get, recall) per completed task;\n  \
         prefetch precision: prefetched entries later opened or cited; wasted reads: bodies opened and not cited\n",
    );
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(session: &str, tool: &str, ids: &[&str]) -> Read {
        Read {
            at: "2026-10-10T00:00:00Z".into(),
            session: session.into(),
            tool: tool.into(),
            query: None,
            hits: ids.len(),
            tokens: 10,
            cached: false,
            ids: ids.iter().map(|i| (*i).to_string()).collect(),
            via: None,
        }
    }

    fn wrote(session: &str, id: &str) -> Wrote {
        Wrote {
            at: "2026-10-10T00:00:00Z".into(),
            session: session.into(),
            id: id.into(),
            author: "agent:test".into(),
            supersedes: None,
        }
    }

    #[test]
    fn a_citation_is_a_whole_id() {
        assert!(cites("see `mem-a-1` for this", "mem-a-1"));
        assert!(cites("mem-a-1", "mem-a-1"));
        assert!(!cites("see mem-a-10", "mem-a-1"));
        assert!(!cites("see xmem-a-1", "mem-a-1"));
        assert!(!cites("anything", ""));
    }

    /// Session one: prefetched a, b, c; searched and got d, e, a; opened d (through),
    /// a (prefetched and searched), f (neither); completed a task with a handoff
    /// citing c and d. Session two: searched g, opened g, left no handoff.
    fn synthetic() -> Metrics {
        let reads = vec![
            read("s1", "t0", &[]),
            read("s1", "prefetch", &["a", "b", "c"]),
            read("s1", "get", &["d"]),
            read("s1", "search", &["d", "e", "a"]),
            read("s1", "get", &["d"]),
            read("s1", "get", &["a"]),
            read("s1", "get", &["f"]),
            read("s1", "recall", &[]),
            read("s2", "search", &["g"]),
            read("s2", "get", &["g"]),
            read("s3", "t0", &[]),
        ];
        let writes = vec![wrote("s1", "h1"), wrote("s1", "x1"), wrote("s2", "x2")];
        let handoffs = BTreeMap::from([("h1".to_string(), "Done. Used c and d.".to_string())]);
        let completed = BTreeSet::from(["h1".to_string()]);
        compute(&Inputs {
            reads: &reads,
            writes: &writes,
            handoffs: &handoffs,
            completed: &completed,
        })
    }

    #[test]
    fn the_four_metrics_follow_the_reads_and_the_handoff() {
        let m = synthetic();
        assert_eq!(m.sessions.len(), 2, "s3 only loaded T0: {m:?}");
        let s1 = &m.sessions[0];
        assert_eq!(s1.session, "s1");
        assert_eq!(s1.handoffs, vec!["h1".to_string()]);
        let c = &s1.counts;
        // d was opened once before the search served it and once after: through
        assert_eq!(
            (c.fetched_through, c.search_hits),
            (2, 3),
            "d and a, of d e a"
        );
        assert_eq!(c.tasks_completed, 1);
        assert_eq!(
            c.pulls, 6,
            "a search, four gets and a recall; t0 and prefetch are pushes"
        );
        assert_eq!(c.retrieval_depth, Some(6.0));
        assert_eq!(
            (c.prefetch_used, c.prefetched),
            (2, 3),
            "a opened, c cited, b neither"
        );
        assert_eq!(c.prefetch_precision, Some(2.0 / 3.0));
        assert_eq!(
            (c.opened_uncited, c.opened),
            (2, 3),
            "a and f uncited; d cited"
        );
        let s2 = &m.sessions[1].counts;
        assert_eq!(s2.fetch_through_rate, Some(1.0));
        assert_eq!(s2.retrieval_depth, None, "completed nothing");
        assert_eq!(s2.wasted_read_ratio, None, "no handoff to judge by");
        assert_eq!(s2.prefetch_precision, None);
    }

    #[test]
    fn the_total_sums_counts_and_recomputes_ratios_over_judged_sessions() {
        let m = synthetic();
        let t = &m.total;
        assert_eq!((t.fetched_through, t.search_hits), (3, 4));
        assert_eq!(
            (t.pulls, t.tasks_completed),
            (6, 1),
            "only sessions that completed"
        );
        assert_eq!((t.prefetch_used, t.prefetched), (2, 3));
        assert_eq!((t.opened_uncited, t.opened), (2, 3), "s2 had no handoff");
        assert_eq!(t.fetch_through_rate, Some(0.75));
        let text = render(&m);
        assert!(
            text.contains("Metrics") && text.contains("cited by id"),
            "{text}"
        );
        assert!(
            text.contains("3/4 75%") && text.contains("6.0 (1 tasks)"),
            "{text}"
        );
        let json = serde_json::to_value(&m).unwrap();
        assert_eq!(json["total"]["prefetch_used"], 2);
        assert_eq!(json["sessions"][0]["wasted_read_ratio"], 2.0 / 3.0);
    }

    #[test]
    fn an_empty_log_has_no_metrics_and_says_so() {
        let m = compute(&Inputs {
            reads: &[],
            writes: &[],
            handoffs: &BTreeMap::new(),
            completed: &BTreeSet::new(),
        });
        assert!(m.sessions.is_empty());
        assert_eq!(m.total.fetch_through_rate, None);
        assert!(render(&m).contains("no session"));
    }
}
