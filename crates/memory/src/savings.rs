//! The savings report: the paper's read-path claims, measured on this record.
//!
//! Three numbers, each against the alternative it replaces. Recovery: what a fresh
//! session loads (T0 and the latest handoff) against replaying the transcripts of the
//! sessions that ran here. Retrieval: what a page of hits costs against the bodies it
//! stands for and the corpus it was chosen from, and whether known items land in the top
//! three. The funnel: what memory served each session, from the reads log, beside what
//! the session's model calls ingested, from the egress telemetry.

use dm_core::approx_tokens;
use dm_egress::Telemetry;
use dm_server::{reads, Memory};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

type Fallible<T> = Result<T, Box<dyn std::error::Error>>;

/// The report, as numbers.
#[derive(Debug, Default, Serialize)]
pub struct Report {
    /// Entries in the record.
    pub entries: usize,
    /// Tokens of every body, what "read everything" costs.
    pub corpus_tokens: usize,
    /// Tokens of T0.
    pub t0_tokens: usize,
    /// Tokens of the latest handoff, when there is one.
    pub handoff_tokens: usize,
    /// Transcripts found under the sandbox homes.
    pub transcripts: usize,
    /// Their tokens in total.
    pub transcript_tokens: usize,
    /// The query the retrieval section used.
    pub query: String,
    /// Hits returned.
    pub hits: usize,
    /// Tokens of the page of hits.
    pub hits_tokens: usize,
    /// Tokens of those hits' bodies.
    pub hit_bodies_tokens: usize,
    /// Known-item lookups tried: every entry searched by its own title.
    pub known_items: usize,
    /// Of those, how many ranked in the top three.
    pub known_items_top3: usize,
    /// Search latency over those lookups, in milliseconds, measured in process: the floor
    /// under the paper's SLO of 100 at p50 and 500 at p99, which is end to end, through
    /// the serving binding and the network, so this cannot show the SLO is met.
    pub search_p50_ms: f64,
    pub search_p99_ms: f64,
    /// Per session: tokens memory served, tokens the model ingested.
    pub sessions: Vec<SessionRow>,
    /// Recall, by regime: the brief against the pack it would be written from.
    pub recall: RecallRow,
}

/// Recall in the funnel. One turn for the caller either way; what differs is the
/// tokens the turn carries and whether the caller then opens what was pushed.
#[derive(Debug, Default, Serialize, PartialEq, Eq)]
pub struct RecallRow {
    /// Server-executed recalls: a brief came back.
    pub briefs: usize,
    /// Tokens those briefs cost the caller, summaries beside them included.
    pub brief_tokens: usize,
    /// Caller-executed recalls: a pack came back with synthesis owed.
    pub packs: usize,
    /// Of those, packs that carried sections.
    pub packs_with_sections: usize,
    /// Tokens those packs cost the caller.
    pub pack_tokens: usize,
    /// Gets served from the session cache: pushed by a pack, then opened.
    pub pushed_then_opened: usize,
    /// Gets in total, for the share.
    pub gets: usize,
}

/// One session in the funnel.
#[derive(Debug, Default, Serialize)]
pub struct SessionRow {
    /// The session id.
    pub session: String,
    /// Reads answered by memory.
    pub reads: usize,
    /// Tokens memory served.
    pub served_tokens: usize,
    /// Tokens the session's model calls ingested, cache reads excluded.
    pub input_tokens: u64,
    /// Cache reads.
    pub cache_read_tokens: u64,
    /// Model calls.
    pub calls: u64,
    /// Whether the harness counted (a tunnelled login) rather than the egress.
    pub reported: bool,
}

/// Transcripts the harnesses left in their private homes under the record's state.
fn transcripts(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(homes) = std::fs::read_dir(root.join("state/sandbox")) else {
        return out;
    };
    for home in homes.flatten() {
        let projects = home.path().join(".claude/projects");
        let Ok(dirs) = std::fs::read_dir(&projects) else {
            continue;
        };
        for d in dirs.flatten() {
            if let Ok(files) = std::fs::read_dir(d.path()) {
                out.extend(
                    files
                        .flatten()
                        .map(|f| f.path())
                        .filter(|p| p.extension().is_some_and(|e| e == "jsonl")),
                );
            }
        }
    }
    out.sort();
    out
}

/// Tokens a transcript costs to replay: the text of its messages, tool results included,
/// since a replay carries them too.
fn transcript_tokens(jsonl: &str) -> usize {
    jsonl
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| {
            matches!(
                v.get("type").and_then(|t| t.as_str()),
                Some("user" | "assistant")
            )
        })
        .map(|v| {
            let content = v.get("message").and_then(|m| m.get("content"));
            match content {
                Some(serde_json::Value::String(s)) => approx_tokens(s),
                Some(other) => approx_tokens(&other.to_string()),
                None => 0,
            }
        })
        .sum()
}

async fn corpus(memory: &Memory, report: &mut Report) -> Fallible<Vec<(String, String)>> {
    let hits = memory.list(None, None, 100_000).await?;
    let mut titles = Vec::new();
    for h in &hits {
        if let Ok(e) = memory.get(&h.id).await {
            report.corpus_tokens += approx_tokens(&e.body);
            titles.push((h.id.clone(), e.title()));
        }
    }
    report.entries = titles.len();
    Ok(titles)
}

async fn retrieval(
    memory: &Memory,
    report: &mut Report,
    query: Option<&str>,
    titles: &[(String, String)],
) -> Fallible<()> {
    let q = match query {
        Some(q) => q.to_string(),
        None => titles
            .first()
            .map(|(_, t)| t.clone())
            .unwrap_or_else(|| "handoff".into()),
    };
    report.query = q.clone();
    let hits = memory.search(&q, None, 10).await?;
    report.hits = hits.len();
    report.hits_tokens = reads::hits_tokens(&hits);
    for h in &hits {
        if let Ok(e) = memory.get(&h.id).await {
            report.hit_bodies_tokens += approx_tokens(&e.body);
        }
    }
    let mut took = Vec::new();
    for (id, title) in titles.iter().take(200) {
        let q = title.trim();
        if q.is_empty() {
            continue;
        }
        report.known_items += 1;
        let started = std::time::Instant::now();
        let hits = memory.search(q, None, 3).await?;
        took.push(started.elapsed().as_secs_f64() * 1000.0);
        if hits.iter().any(|h| &h.id == id) {
            report.known_items_top3 += 1;
        }
    }
    report.search_p50_ms = percentile(&mut took, 0.50);
    report.search_p99_ms = percentile(&mut took, 0.99);
    Ok(())
}

/// The `q` quantile of the samples, nearest rank; zero for none.
fn percentile(samples: &mut [f64], q: f64) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    samples.sort_by(f64::total_cmp);
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )] // a rank
    let rank = ((q * samples.len() as f64).ceil() as usize).clamp(1, samples.len());
    samples[rank - 1]
}

/// One read's share of the recall row.
fn count_recall(row: &mut RecallRow, r: &reads::Read) {
    match (r.tool.as_str(), r.via.as_deref()) {
        ("recall", Some("brief")) => {
            row.briefs += 1;
            row.brief_tokens = row.brief_tokens.saturating_add(r.tokens);
        }
        ("recall", Some(via)) => {
            row.packs += 1;
            row.pack_tokens = row.pack_tokens.saturating_add(r.tokens);
            if via == "pack:sections" {
                row.packs_with_sections += 1;
            }
        }
        ("get", _) => {
            row.gets += 1;
            if r.cached {
                row.pushed_then_opened += 1;
            }
        }
        _ => {}
    }
}

fn funnel(root: &Path, report: &mut Report) -> Fallible<()> {
    let mut rows: BTreeMap<String, SessionRow> = BTreeMap::new();
    for r in reads::all(root)? {
        let row = rows.entry(r.session.clone()).or_insert_with(|| SessionRow {
            session: r.session.clone(),
            ..SessionRow::default()
        });
        row.reads += 1;
        // a log line is a file anyone can append to: a number that cannot be is clamped
        row.served_tokens = row.served_tokens.saturating_add(r.tokens);
        count_recall(&mut report.recall, &r);
    }
    if let Ok(t) = Telemetry::open(&root.join("state/telemetry.db")) {
        for s in t.by_session()? {
            let row = rows.entry(s.session.clone()).or_insert_with(|| SessionRow {
                session: s.session.clone(),
                ..SessionRow::default()
            });
            row.input_tokens = s.input_tokens;
            row.cache_read_tokens = s.cache_read_tokens;
            row.calls = s.calls;
            row.reported = s.reported;
        }
    }
    report.sessions = rows
        .into_values()
        .filter(|r| r.calls > 0 || r.reads > 0)
        .collect();
    Ok(())
}

/// Measure.
///
/// # Errors
///
/// When the record cannot be read.
pub async fn measure(root: &Path, query: Option<&str>) -> Fallible<Report> {
    let memory = Memory::open(root)?.without_read_log();
    let mut report = Report::default();
    let titles = corpus(&memory, &mut report).await?;
    let t0 = memory.t0(None).await?;
    report.t0_tokens = approx_tokens(&t0);
    if let Some(h) = memory
        .list(None, Some(dm_core::EntryType::Handoff), 1)
        .await?
        .first()
    {
        if let Ok(e) = memory.get(&h.id).await {
            report.handoff_tokens = approx_tokens(&e.body);
        }
    }
    for t in transcripts(root) {
        if let Ok(text) = std::fs::read_to_string(&t) {
            report.transcripts += 1;
            report.transcript_tokens += transcript_tokens(&text);
        }
    }
    retrieval(&memory, &mut report, query, &titles).await?;
    funnel(root, &mut report)?;
    Ok(report)
}

fn pct(part: usize, whole: usize) -> String {
    if whole == 0 {
        return "-".into();
    }
    format!("{:.1}%", part as f64 * 100.0 / whole as f64)
}

/// The report as text, for a terminal.
#[must_use]
pub fn render(r: &Report) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "Record: {} entries, {} tokens of bodies.\n\n",
        r.entries, r.corpus_tokens
    ));
    s.push_str("Recovery: what a fresh session loads, against replaying a transcript\n");
    s.push_str(&format!(
        "  T0                      {:>8} tokens  ({} of the corpus)\n",
        r.t0_tokens,
        pct(r.t0_tokens, r.corpus_tokens)
    ));
    s.push_str(&format!(
        "  latest handoff          {:>8} tokens\n",
        r.handoff_tokens
    ));
    if let Some(per) = r.transcript_tokens.checked_div(r.transcripts) {
        let load = r.t0_tokens + r.handoff_tokens;
        s.push_str(&format!(
            "  {} transcripts here      {:>8} tokens, {} each on average\n",
            r.transcripts, r.transcript_tokens, per
        ));
        s.push_str(&format!(
            "  T0 + handoff is {} of an average transcript replay\n",
            pct(load, per)
        ));
    } else {
        s.push_str("  no transcripts under state/sandbox yet; run a session first\n");
    }
    s.push_str(&format!(
        "\nRetrieval: search \"{}\"\n  {} hits                 {:>8} tokens  (their bodies: {}; the corpus: {})\n",
        r.query, r.hits, r.hits_tokens, r.hit_bodies_tokens, r.corpus_tokens
    ));
    s.push_str(&format!(
        "  known items in top 3    {:>5}/{:<5}  ({})\n",
        r.known_items_top3,
        r.known_items,
        pct(r.known_items_top3, r.known_items)
    ));
    s.push_str(&format!(
        "  search, in process  p50 {:>6.1} ms   p99 {:>6.1} ms  (the floor under the paper's end-to-end 100 and 500 ms: no transport, no network, one machine)\n",
        r.search_p50_ms, r.search_p99_ms
    ));
    s.push_str(
        "\nFunnel: tokens memory served each session, beside what its model calls ingested\n",
    );
    s.push_str("  session        reads   served   model calls   input    cache read\n");
    let mut any_reported = false;
    for row in &r.sessions {
        any_reported |= row.reported;
        s.push_str(&format!(
            "  {:<13} {:>5}  {:>7}   {:>11}  {:>6}  {:>10}{}\n",
            row.session,
            row.reads,
            row.served_tokens,
            row.calls,
            row.input_tokens,
            row.cache_read_tokens,
            if row.reported { " *" } else { "" }
        ));
    }
    if any_reported {
        s.push_str(
            "  * counted by the harness itself: a tunnelled login the egress could not meter\n",
        );
    }
    if r.sessions.is_empty() {
        s.push_str("  nothing recorded yet\n");
    }
    s.push_str(&render_recall(&r.recall));
    s
}

/// The recall section: one caller turn either way, so the comparison is tokens per
/// turn and what the caller opened afterwards.
fn render_recall(r: &RecallRow) -> String {
    let mut s = String::new();
    s.push_str("\nRecall: one turn either way; the brief against the pack it is written from\n");
    if r.briefs + r.packs == 0 {
        s.push_str("  none yet; `memory recall <question>` on this record\n");
        return s;
    }
    let each = |tokens: usize, n: usize| tokens.checked_div(n).unwrap_or(0);
    s.push_str(&format!(
        "  server briefs      {:>5}   {:>7} tokens each\n",
        r.briefs,
        each(r.brief_tokens, r.briefs)
    ));
    s.push_str(&format!(
        "  owed packs         {:>5}   {:>7} tokens each  ({} with sections, {} summaries only)\n",
        r.packs,
        each(r.pack_tokens, r.packs),
        r.packs_with_sections,
        r.packs - r.packs_with_sections
    ));
    s.push_str(&format!(
        "  pushed then opened {:>5}   of {} gets served from a pack's push ({})\n",
        r.pushed_then_opened,
        r.gets,
        pct(r.pushed_then_opened, r.gets)
    ));
    s
}

#[cfg(test)]
mod percentile_tests {
    #[test]
    fn nearest_rank() {
        let mut v = vec![5.0, 1.0, 3.0, 2.0, 4.0];
        assert!((super::percentile(&mut v, 0.5) - 3.0).abs() < 1e-9);
        assert!((super::percentile(&mut v, 0.99) - 5.0).abs() < 1e-9);
        assert!(super::percentile(&mut [], 0.5).abs() < 1e-9);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_tokens_count_messages_only() {
        let t = concat!(
            r#"{"type":"user","message":{"content":"one two three"}}"#,
            "\n",
            r#"{"type":"cost-state","x":"not counted at all"}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"four five"}]}}"#,
            "\n",
        );
        let n = transcript_tokens(t);
        assert!(n >= 4 + 2, "{n}");
        assert!(n < 40, "{n}");
    }

    struct Scripted;

    impl dm_server::recall::Deputy for Scripted {
        fn model(&self) -> String {
            "scripted".into()
        }

        fn complete<'a>(
            &'a self,
            _prompt: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>>
        {
            Box::pin(async { Ok("{\"brief\": \"The import has five steps.\"}".into()) })
        }
    }

    #[tokio::test]
    async fn recall_is_counted_by_regime_with_what_was_pushed_and_then_opened() {
        use dm_server::recall::{Budget, Depth};
        let dir = tempfile::tempdir().unwrap();
        let m = Memory::init(dir.path(), "human:test:me", "dom-me").unwrap();
        let w = m
            .write(dm_server::WriteRequest {
                body: "# Import steps\n\nSteps one to five of the import.".into(),
                ..dm_server::WriteRequest::default()
            })
            .await
            .unwrap();
        m.recall("import", None, Some(Depth::Sections), Budget::default())
            .await
            .unwrap();
        m.recall("import", None, Some(Depth::Summaries), Budget::default())
            .await
            .unwrap();
        m.get(&w.id).await.unwrap();
        let deputy: std::sync::Arc<dyn dm_server::recall::Deputy> = std::sync::Arc::new(Scripted);
        m.set_deputy(&deputy);
        m.recall("import", None, None, Budget::default())
            .await
            .unwrap();
        drop(m);
        let r = measure(dir.path(), Some("import")).await.unwrap();
        assert_eq!(
            r.recall,
            RecallRow {
                briefs: 1,
                brief_tokens: r.recall.brief_tokens,
                packs: 2,
                packs_with_sections: 1,
                pack_tokens: r.recall.pack_tokens,
                pushed_then_opened: 1,
                gets: 1,
            },
            "{r:?}"
        );
        assert!(r.recall.brief_tokens > 0 && r.recall.pack_tokens > r.recall.brief_tokens);
        let text = render(&r);
        assert!(
            text.contains("Recall") && text.contains("pushed then opened"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn a_hostile_read_log_line_cannot_overflow_the_report() {
        let dir = tempfile::tempdir().unwrap();
        let m = Memory::init(dir.path(), "human:test:me", "dom-me").unwrap();
        m.write(dm_server::WriteRequest {
            body: "# Import steps\n\nSteps one to five.".into(),
            ..dm_server::WriteRequest::default()
        })
        .await
        .unwrap();
        drop(m);
        let log = dir.path().join("state/reads.jsonl");
        let huge = usize::MAX;
        std::fs::write(
            &log,
            format!(
                "{{\"at\":\"2026-10-03T00:00:00Z\",\"session\":\"sess-x\",\"tool\":\"recall\",\"hits\":1,\"tokens\":{huge},\"via\":\"brief\"}}\n\
                 {{\"at\":\"2026-10-03T00:00:01Z\",\"session\":\"sess-x\",\"tool\":\"recall\",\"hits\":1,\"tokens\":{huge},\"via\":\"pack:sections\"}}\n\
                 {{\"at\":\"2026-10-03T00:00:02Z\",\"session\":\"sess-x\",\"tool\":\"get\",\"hits\":1,\"tokens\":5}}\n\
                 {{\"at\":\"2026-10-03T00:00:03Z\",\"session\":\"sess-x\",\"tool\":\"recall\",\"hits\":1,\"tokens\":5,\"via\":\"brief\"}}\n\
                 {{\"at\":\"2026-10-03T00:00:04Z\",\"session\":\"sess-x\",\"tool\":\"recall\",\"hits\":1,\"tokens\":5,\"via\":\"pack:summaries\"}}\n"
            ),
        )
        .unwrap();
        let r = measure(dir.path(), Some("import")).await.unwrap();
        assert_eq!(
            r.sessions[0].served_tokens,
            usize::MAX,
            "saturated, not wrapped"
        );
        assert_eq!(r.recall.brief_tokens, usize::MAX);
        assert_eq!(r.recall.pack_tokens, usize::MAX);
        assert_eq!(r.recall.briefs, 2);
    }

    #[tokio::test]
    async fn the_report_measures_a_small_record() {
        let dir = tempfile::tempdir().unwrap();
        let m = Memory::init(dir.path(), "human:test:me", "dom-me")
            .unwrap()
            .with_author("agent:test");
        for (title, body) in [
            (
                "Import steps",
                "Steps one to five of the import, with the gotchas.",
            ),
            ("Commit convention", "Short subject, body says why."),
            (
                "Stopped at step 4",
                "Handoff: step 3 done, step 4 half done.",
            ),
        ] {
            m.write(dm_server::WriteRequest {
                body: format!("# {title}\n\n{body}"),
                entry_type: if title.starts_with("Stopped") {
                    dm_core::EntryType::Handoff
                } else {
                    dm_core::EntryType::Fact
                },
                ..dm_server::WriteRequest::default()
            })
            .await
            .unwrap();
        }
        let home = dir.path().join("state/sandbox/claude/.claude/projects/p");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("s.jsonl"), format!("{}\n", r#"{"type":"user","message":{"content":"a very long transcript line repeated many many many times to cost tokens"}}"#).repeat(50)).unwrap();
        let r = measure(dir.path(), Some("import")).await.unwrap();
        assert_eq!(r.entries, 3);
        assert!(r.corpus_tokens > 0 && r.t0_tokens > 0);
        assert_eq!(r.transcripts, 1);
        assert!(
            r.transcript_tokens > r.t0_tokens + r.handoff_tokens,
            "{r:?}"
        );
        assert!(r.hits >= 1 && r.hits_tokens > 0);
        assert_eq!(r.known_items, 3);
        assert!(r.known_items_top3 >= 2, "{r:?}");
        assert!(
            r.sessions.is_empty(),
            "the report's own reads are not logged: {r:?}"
        );
        let text = render(&r);
        assert!(
            text.contains("Recovery") && text.contains("Retrieval") && text.contains("Funnel"),
            "{text}"
        );
        assert!(text.contains("known items in top 3"), "{text}");
        assert!(text.contains("search, in process"), "{text}");
        assert!(r.search_p99_ms >= r.search_p50_ms);
    }
}
