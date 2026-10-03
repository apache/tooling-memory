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
    /// Per session: tokens memory served, tokens the model ingested.
    pub sessions: Vec<SessionRow>,
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
    for (id, title) in titles.iter().take(200) {
        let q = title.trim();
        if q.is_empty() {
            continue;
        }
        report.known_items += 1;
        let hits = memory.search(q, None, 3).await?;
        if hits.iter().any(|h| &h.id == id) {
            report.known_items_top3 += 1;
        }
    }
    Ok(())
}

fn funnel(root: &Path, report: &mut Report) -> Fallible<()> {
    let mut rows: BTreeMap<String, SessionRow> = BTreeMap::new();
    for r in reads::all(root)? {
        let row = rows.entry(r.session.clone()).or_insert_with(|| SessionRow {
            session: r.session.clone(),
            ..SessionRow::default()
        });
        row.reads += 1;
        row.served_tokens += r.tokens;
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
    s.push_str(
        "\nFunnel: tokens memory served each session, beside what its model calls ingested\n",
    );
    s.push_str("  session        reads   served   model calls   input    cache read\n");
    for row in &r.sessions {
        s.push_str(&format!(
            "  {:<13} {:>5}  {:>7}   {:>11}  {:>6}  {:>10}\n",
            row.session,
            row.reads,
            row.served_tokens,
            row.calls,
            row.input_tokens,
            row.cache_read_tokens
        ));
    }
    if r.sessions.is_empty() {
        s.push_str("  nothing recorded yet\n");
    }
    s
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
    }
}
