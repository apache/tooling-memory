//! Golden queries: questions with the entries that answer them, scored against the
//! record with no model in the loop. The paper runs a suite like this per domain on
//! every change to the indexer, the ranker or a summary prompt, and this is the first
//! one. It answers two things a person otherwise judges by reading: whether the pack
//! for a question ranks the right entry, and what each way of asking costs in tokens.
//!
//! A suite is TOML: a domain, and questions with the titles of the entries that answer
//! them. Titles rather than ids, because ids are minted per write and a suite has to
//! survive a reseed. Each question is asked three ways, the question as written, the
//! question with its stopwords dropped, and the keyword query a person wrote for it when
//! the suite has one, which stands in for the query a model writes. With a deputy, the
//! brief is scored too: does it cite an answer, and what did it cost beside the pack.

use dm_core::approx_tokens;
use dm_server::recall::{Budget, Depth, Synthesis};
use dm_server::Memory;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

type Fallible<T> = Result<T, Box<dyn std::error::Error>>;

/// A suite file.
#[derive(Debug, Deserialize, PartialEq, Eq)]
pub struct Suite {
    /// The domain the questions are about; every granted domain when omitted.
    #[serde(default)]
    pub domain: Option<String>,
    /// The questions.
    #[serde(rename = "question")]
    pub questions: Vec<Question>,
}

/// One golden question.
#[derive(Debug, Deserialize, PartialEq, Eq)]
pub struct Question {
    /// The question, as a person or an agent would ask it.
    pub ask: String,
    /// Titles, or parts of titles, of the entries that answer it; any one is a hit.
    pub answers: Vec<String>,
    /// The keyword query a person would write for it, when there is one.
    #[serde(default)]
    pub keywords: Option<String>,
}

impl Suite {
    /// Parse a suite.
    ///
    /// # Errors
    ///
    /// Malformed TOML, or a suite with no questions or a question with no answers.
    pub fn parse(text: &str) -> Fallible<Suite> {
        let s: Suite = toml::from_str(text)?;
        if s.questions.is_empty() {
            return Err("a suite needs at least one [[question]]".into());
        }
        if let Some(q) = s
            .questions
            .iter()
            .find(|q| q.answers.is_empty() || q.ask.trim().is_empty())
        {
            return Err(format!("question {:?} needs an ask and answers", q.ask).into());
        }
        Ok(s)
    }

    /// Read a suite from a file.
    ///
    /// # Errors
    ///
    /// The file cannot be read, or does not parse.
    pub fn load(path: &Path) -> Fallible<Suite> {
        Suite::parse(&std::fs::read_to_string(path)?)
    }
}

/// How one question went under one strategy.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct Row {
    /// The question.
    pub ask: String,
    /// question, stripped, keywords, or brief.
    pub strategy: String,
    /// Where the first answering entry ranked, from 1; none when not in the top k.
    pub rank: Option<usize>,
    /// What the pack at section depth costs to read; for a brief, the brief and its
    /// summaries.
    pub tokens: usize,
}

/// A strategy's score over the suite.
#[derive(Debug, Default, Serialize, PartialEq)]
pub struct Score {
    /// The strategy.
    pub strategy: String,
    /// Questions it was asked; a question without keywords is skipped by that strategy.
    pub asked: usize,
    /// Questions with an answer in the top k.
    pub hits: usize,
    /// Mean reciprocal rank over the questions asked.
    pub mrr: f64,
    /// Tokens over the questions asked.
    pub tokens: usize,
}

/// The suite, scored.
#[derive(Debug, Default, Serialize)]
pub struct Report {
    /// Hits are counted within this many.
    pub k: usize,
    /// Questions in the suite.
    pub questions: usize,
    /// Questions whose answers name no entry in the record.
    pub unresolved: Vec<String>,
    /// Per strategy.
    pub scores: Vec<Score>,
    /// Every question under every strategy.
    pub rows: Vec<Row>,
}

/// Words that carry no query: dropped by the `stripped` strategy.
const STOPWORDS: &[&str] = &[
    "a", "about", "an", "and", "are", "as", "at", "be", "between", "by", "can", "could", "decide",
    "decides", "do", "does", "for", "from", "get", "has", "have", "how", "if", "in", "into", "is",
    "it", "its", "let", "letting", "mean", "means", "of", "on", "or", "paper", "say", "says",
    "should", "than", "that", "the", "their", "them", "then", "there", "these", "they", "this",
    "to", "was", "we", "were", "what", "when", "where", "which", "who", "why", "will", "with",
    "would", "you",
];

/// The question without its stopwords and punctuation, lower-cased.
#[must_use]
pub fn stripped(ask: &str) -> String {
    ask.split(|c: char| !c.is_alphanumeric() && c != '-')
        .map(str::to_lowercase)
        .filter(|w| !w.is_empty() && !STOPWORDS.contains(&w.as_str()))
        .collect::<Vec<_>>()
        .join(" ")
}

fn answers_of(q: &Question, titles: &BTreeMap<String, String>) -> Vec<String> {
    titles
        .iter()
        .filter(|(title, _)| {
            q.answers.iter().any(|a| {
                title
                    .to_lowercase()
                    .contains(a.trim().to_lowercase().as_str())
            })
        })
        .map(|(_, id)| id.clone())
        .collect()
}

fn rank_of(ids: &[String], answers: &[String]) -> Option<usize> {
    ids.iter()
        .position(|id| answers.contains(id))
        .map(|i| i + 1)
}

/// Every active entry's title and id, within the suite's domain.
async fn titles(memory: &Memory, domain: Option<&str>) -> Fallible<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for h in memory.list(domain, None, 100_000).await? {
        out.insert(h.title, h.id);
    }
    Ok(out)
}

/// Score the suite: the three query strategies through the pack, and the brief when the
/// memory has a deputy. Open the memory without its read log, or the suite's reads land
/// in the funnel as a session's.
///
/// # Errors
///
/// An ungranted domain, or an index, store or deputy error.
pub async fn score(memory: &Memory, suite: &Suite, k: usize) -> Fallible<Report> {
    let domain = suite.domain.as_deref();
    let titles = titles(memory, domain).await?;
    let budget = Budget {
        hits: k.max(1),
        links: 0,
        ..Budget::default()
    };
    let mut report = Report {
        k: budget.hits,
        questions: suite.questions.len(),
        ..Report::default()
    };
    let mut scores: BTreeMap<&str, Score> = BTreeMap::new();
    for q in &suite.questions {
        let answers = answers_of(q, &titles);
        if answers.is_empty() {
            report.unresolved.push(q.ask.clone());
            continue;
        }
        let mut ways = vec![("question", q.ask.clone()), ("stripped", stripped(&q.ask))];
        if let Some(kw) = &q.keywords {
            ways.push(("keywords", kw.clone()));
        }
        for (name, query) in ways {
            let pack = memory
                .pack(&[query], domain, Depth::Sections, budget)
                .await?;
            let row = Row {
                ask: q.ask.clone(),
                strategy: name.to_string(),
                rank: rank_of(&pack.ids(), &answers),
                tokens: pack.tokens,
            };
            tally(scores.entry(name).or_default(), name, &row);
            report.rows.push(row);
        }
        if memory.deputy().is_some() {
            let r = memory.recall(&q.ask, domain, None, budget).await?;
            let brief = r.brief.as_deref().unwrap_or_default();
            let row = Row {
                ask: q.ask.clone(),
                strategy: "brief".into(),
                rank: match r.synthesis {
                    Synthesis::Server { .. } => rank_of(&r.citations, &answers).map(|_| 1),
                    Synthesis::Owed => None,
                },
                tokens: approx_tokens(brief) + r.pack.tokens,
            };
            tally(scores.entry("brief").or_default(), "brief", &row);
            report.rows.push(row);
        }
    }
    report.scores = scores
        .into_values()
        .map(|mut s| {
            s.mrr /= s.asked.max(1) as f64;
            s
        })
        .collect();
    Ok(report)
}

fn tally(s: &mut Score, name: &str, row: &Row) {
    s.strategy = name.to_string();
    s.asked += 1;
    s.tokens += row.tokens;
    if let Some(r) = row.rank {
        s.hits += 1;
        s.mrr += 1.0 / r as f64;
    }
}

/// The report as text, for a terminal.
#[must_use]
pub fn render(r: &Report) -> String {
    let mut s = format!(
        "Golden: {} questions, hits counted in the top {}\n\n",
        r.questions, r.k
    );
    s.push_str("  strategy    asked   hits    MRR    tokens each\n");
    for sc in &r.scores {
        s.push_str(&format!(
            "  {:<10} {:>6} {:>6}  {:>5.2}  {:>11}\n",
            sc.strategy,
            sc.asked,
            sc.hits,
            sc.mrr,
            sc.tokens.checked_div(sc.asked).unwrap_or(0)
        ));
    }
    let misses: Vec<&Row> = r.rows.iter().filter(|row| row.rank.is_none()).collect();
    if !misses.is_empty() {
        s.push_str("\nMissed:\n");
        for row in misses {
            s.push_str(&format!("  {:<10} {}\n", row.strategy, row.ask));
        }
    }
    if !r.unresolved.is_empty() {
        s.push_str("\nAnswers naming no entry in this record:\n");
        for ask in &r.unresolved {
            s.push_str(&format!("  {ask}\n"));
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use dm_server::WriteRequest;

    const SUITE: &str = r#"
domain = "dom-me"

[[question]]
ask = "How does the import retry when a step fails?"
answers = ["Retry policy"]
keywords = "retry backoff"

[[question]]
ask = "What is the commit convention?"
answers = ["Commit convention"]

[[question]]
ask = "Where is the deployment runbook?"
answers = ["Deployment runbook"]
"#;

    #[test]
    fn a_suite_parses_and_refuses_an_empty_one() {
        let s = Suite::parse(SUITE).unwrap();
        assert_eq!(s.domain.as_deref(), Some("dom-me"));
        assert_eq!(s.questions.len(), 3);
        assert_eq!(s.questions[0].keywords.as_deref(), Some("retry backoff"));
        assert!(Suite::parse("domain = \"x\"\n").is_err());
        assert!(Suite::parse("[[question]]\nask = \"q\"\nanswers = []\n").is_err());
        assert_eq!(
            stripped("How does the import retry when a step fails?"),
            "import retry step fails"
        );
    }

    async fn write(m: &Memory, title: &str, body: &str) {
        m.write(WriteRequest {
            title: Some(title.into()),
            body: body.into(),
            ..WriteRequest::default()
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn the_suite_scores_each_strategy_and_names_what_it_cannot_resolve() {
        let dir = tempfile::tempdir().unwrap();
        let m = Memory::init(dir.path(), "human:test:me", "dom-me")
            .unwrap()
            .without_read_log();
        write(
            &m,
            "Retry policy",
            "Operation R retries with exponential backoff.\n",
        )
        .await;
        write(
            &m,
            "Import steps",
            "The import has five steps; the fourth is slow.\n",
        )
        .await;
        write(&m, "Commit convention", "Short subject, body says why.\n").await;
        let r = score(&m, &Suite::parse(SUITE).unwrap(), 5).await.unwrap();
        assert_eq!(r.questions, 3);
        assert_eq!(r.unresolved, ["Where is the deployment runbook?"]);
        let by: BTreeMap<&str, &Score> =
            r.scores.iter().map(|s| (s.strategy.as_str(), s)).collect();
        assert_eq!(by["question"].asked, 2);
        assert_eq!(by["stripped"].asked, 2);
        assert_eq!(by["keywords"].asked, 1, "one question has keywords");
        assert_eq!(by["keywords"].hits, 1);
        assert!((by["keywords"].mrr - 1.0).abs() < f64::EPSILON);
        assert!(by["question"].hits >= 1, "{r:?}");
        assert!(by["question"].tokens > 0);
        assert!(!by.contains_key("brief"), "no deputy, no brief");
        let kw = r
            .rows
            .iter()
            .find(|row| row.strategy == "keywords")
            .unwrap();
        assert_eq!(kw.rank, Some(1));
        let text = render(&r);
        assert!(text.contains("Golden: 3 questions"), "{text}");
        assert!(text.contains("deployment runbook"), "{text}");
        assert!(
            dir.path().join("state/reads.jsonl").metadata().is_err(),
            "no funnel rows"
        );
    }
}
