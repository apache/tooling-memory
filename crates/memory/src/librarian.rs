//! The librarian's first rung: a survey of the record and its telemetry that proposes
//! curation, and files each proposal as a task for a cheap-model run to do with the
//! memory server's own tools. The paper's librarian verifies stale facts, merges
//! duplicates, writes and maintains the summaries the index serves, splits oversized
//! files, and acts on telemetry: failed queries become authoring tasks. Everything here
//! stays within the paper's containment: the librarian proposes, a task is born
//! proposed unless the domain admits its own filings, every curation write is an entry
//! that `supersedes` another and deletes nothing, and the survey itself writes nothing.

use dm_core::{Entry, EntryType, TrustTier};
use dm_server::reads;
use dm_server::Memory;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;

/// What a proposal asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// A summary the index serves that is missing, a table, a heading, too short or clipped.
    Resummarise,
    /// Two live entries whose titles are the same once normalised.
    Merge,
    /// A raw entry past its review date, or older than the window with no review date.
    Verify,
    /// A body too large for one entry.
    Split,
    /// A question searched for more than once that found nothing: something to write.
    Author,
}

impl Kind {
    fn title(self) -> &'static str {
        match self {
            Kind::Resummarise => "Librarian: rewrite the summary of",
            Kind::Merge => "Librarian: merge",
            Kind::Verify => "Librarian: verify",
            Kind::Split => "Librarian: split",
            Kind::Author => "Librarian: write what was asked for and not found",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Kind::Resummarise => "resummarise",
            Kind::Merge => "merge",
            Kind::Verify => "verify",
            Kind::Split => "split",
            Kind::Author => "author",
        }
    }
}

/// One thing the survey found.
#[derive(Clone, Debug, Serialize)]
pub struct Proposal {
    pub kind: Kind,
    /// The entries concerned, by id.
    pub entries: Vec<String>,
    /// Why, in a sentence.
    pub why: String,
    /// What the task body tells the run to do.
    pub body: String,
}

/// The survey's result.
#[derive(Debug, Default, Serialize)]
pub struct Survey {
    pub domain: String,
    pub entries: usize,
    pub proposals: Vec<Proposal>,
}

/// Options for a survey.
#[derive(Clone, Debug)]
pub struct Options {
    /// Days after which a raw entry with no review date is due for a look.
    pub stale_days: i64,
    /// Characters of body above which an entry is split.
    pub split_chars: usize,
    /// How many entries to read.
    pub limit: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            stale_days: 30,
            split_chars: 12_000,
            limit: 2000,
        }
    }
}

fn normalise(title: &str) -> String {
    title
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether a summary is one the index should not be serving.
#[must_use]
pub fn poor_summary(summary: &str) -> Option<&'static str> {
    let s = summary.trim();
    if s.is_empty() {
        return Some("it has none");
    }
    if s.starts_with('|') || s.contains("|---") {
        return Some("it is a table, not a sentence");
    }
    if s.starts_with('#') {
        return Some("it is a heading, not a sentence");
    }
    if s.chars().count() < 25 {
        return Some("it is shorter than a sentence");
    }
    if s.ends_with('…') && s.chars().count() >= 200 {
        return Some("it is the first paragraph clipped, not a summary written for the index");
    }
    None
}

/// Survey a domain: the record for what needs curation, the read log for what was
/// asked and not found.
///
/// # Errors
///
/// When the record cannot be read.
pub async fn survey(
    memory: &Memory,
    root: &Path,
    domain: Option<&str>,
    options: &Options,
) -> Result<Survey, Box<dyn std::error::Error>> {
    let domain = memory.resolve_domain(domain)?;
    let hits = memory.list(Some(&domain), None, options.limit).await?;
    let mut survey = Survey {
        domain: domain.clone(),
        entries: hits.len(),
        proposals: Vec::new(),
    };
    let today = time::OffsetDateTime::now_utc().date();
    let mut by_title: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for h in &hits {
        let Ok(e) = memory.get(&h.id).await else {
            continue;
        };
        by_title
            .entry(normalise(&e.title()))
            .or_default()
            .push(e.front.id.clone());
        survey_entry(&e, &h.summary, options, today, &mut survey.proposals);
    }
    merges(by_title, &mut survey.proposals);
    authoring(root, &mut survey.proposals);
    Ok(survey)
}

/// The proposals one entry gives rise to: its summary, its size, its age.
fn survey_entry(
    e: &Entry,
    summary: &str,
    options: &Options,
    today: time::Date,
    out: &mut Vec<Proposal>,
) {
    let id = &e.front.id;
    // a handoff is a report for the next session, not a fact the index should curate
    let handoff = e.front.entry_type == EntryType::Handoff;
    if let (false, Some(why)) = (handoff, poor_summary(summary)) {
        out.push(Proposal {
            kind: Kind::Resummarise,
            entries: vec![id.clone()],
            why: format!("the summary of `{id}` is not one the index should serve: {why}"),
            body: format!(
                "Read `{id}` with `get`. Write a one- or two-sentence summary of what it says and why it matters, the kind a search hit should show. Then `write` the entry again with the same body and type, your summary as `summary`, and `supersedes: {id}`. Do not change the body. Leave a handoff naming the new entry."
            ),
        });
    }
    let chars = e.body.chars().count();
    if chars > options.split_chars {
        out.push(Proposal {
            kind: Kind::Split,
            entries: vec![id.clone()],
            why: format!("`{id}` is {chars} characters, more than one entry should hold"),
            body: format!(
                "Read `{id}` with `get`. It is too large for one entry. Split it into entries of one topic each, every one with a title and a one-sentence summary, written with `write`; the first carries `supersedes: {id}` and a short body pointing at the others by id. Change no facts. Leave a handoff listing the new ids."
            ),
        });
    }
    let stale = match e.front.review_by {
        Some(d) => d < today,
        None => {
            e.front.trust_tier == TrustTier::Raw
                && (today - e.front.created.date()).whole_days() > options.stale_days
        }
    };
    if stale && !handoff {
        let why = match e.front.review_by {
            Some(d) => format!("`{id}` was due for review on {d}"),
            None => format!(
                "`{id}` is raw and older than {} days with no review date",
                options.stale_days
            ),
        };
        out.push(Proposal {
            kind: Kind::Verify,
            entries: vec![id.clone()],
            why,
            body: format!(
                "Read `{id}` with `get`. Check whether what it says still holds, against the record (`search`) and the working tree. If it holds, `write` it again unchanged with `supersedes: {id}` and a `review_by` a few months out. If it does not, write the corrected entry with `supersedes: {id}` and say in its body what changed and how you know. You propose; you do not promote to verified. Leave a handoff."
            ),
        });
    }
}

/// Live entries that share a title, once normalised, are merge candidates.
fn merges(by_title: BTreeMap<String, Vec<String>>, out: &mut Vec<Proposal>) {
    for (title, ids) in by_title {
        if ids.len() < 2 || title.is_empty() {
            continue;
        }
        let named = ids
            .iter()
            .map(|i| format!("`{i}`"))
            .collect::<Vec<_>>()
            .join(" and ");
        out.push(Proposal {
            kind: Kind::Merge,
            entries: ids.clone(),
            why: format!("{} live entries share the title \"{title}\"", ids.len()),
            body: format!(
                "Read {named} with `get`. They share a title. If they say the same thing, `write` one entry that holds what both say, with `supersedes` naming the older one, and leave the other as it is; if they differ, give each a title that says how. Change no facts. Leave a handoff."
            ),
        });
    }
}

/// The sensory plane: what sessions searched for more than once and did not find.
fn authoring(root: &Path, out: &mut Vec<Proposal>) {
    let mut misses: BTreeMap<String, usize> = BTreeMap::new();
    for r in reads::all(root).unwrap_or_default() {
        if r.tool == "search" && r.hits == 0 {
            if let Some(q) = r.query.as_deref().map(normalise).filter(|q| q.len() >= 6) {
                *misses.entry(q).or_default() += 1;
            }
        }
    }
    for (q, n) in misses.into_iter().filter(|(_, n)| *n >= 2) {
        out.push(Proposal {
            kind: Kind::Author,
            entries: Vec::new(),
            why: format!("\"{q}\" was searched for {n} times and found nothing"),
            body: format!(
                "Sessions searched the record for \"{q}\" {n} times and found nothing. Find out the answer from the working tree and the record, and `write` an entry that answers it, titled so that the same search finds it. If the answer cannot be found, write a question entry saying so. Leave a handoff."
            ),
        });
    }
}

/// File the proposals as tasks, one each, with `origin: librarian:<kind>` and a hash of
/// what they concern, so a survey run twice files each once. Returns the tasks' ids and
/// how many were already there.
///
/// # Errors
///
/// A ledger error.
pub async fn file(
    memory: &Memory,
    survey: &Survey,
) -> Result<(Vec<String>, usize), Box<dyn std::error::Error>> {
    let mut ids = Vec::new();
    let mut already = 0;
    for p in &survey.proposals {
        let subject = if p.entries.is_empty() {
            p.why.clone()
        } else {
            p.entries.join(",")
        };
        let title = if p.entries.is_empty() {
            p.kind.title().to_string()
        } else {
            format!("{} {}", p.kind.title(), p.entries.join(", "))
        };
        let created = memory
            .create_task(dm_ledger::TaskSpec {
                domain: survey.domain.clone(),
                title,
                body: format!("{}\n\n_{}_", p.body, p.why),
                requirements: Vec::new(),
                depends_on: Vec::new(),
                origin: Some(format!("librarian:{}", p.kind.name())),
                upstream_event: Some(dm_core::digest(&subject)[..16].to_string()),
                created_by: String::new(),
                classification: dm_core::Label::Public,
                priority: -1,
            })
            .await?;
        if created.deduplicated {
            already += 1;
        } else {
            ids.push(created.task.id);
        }
    }
    Ok((ids, already))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_summary_the_index_should_not_serve_is_named() {
        assert_eq!(poor_summary(""), Some("it has none"));
        assert_eq!(
            poor_summary("| a | b |"),
            Some("it is a table, not a sentence")
        );
        assert_eq!(
            poor_summary("# Heading"),
            Some("it is a heading, not a sentence")
        );
        assert_eq!(
            poor_summary("Too short."),
            Some("it is shorter than a sentence")
        );
        let clipped = format!("{}…", "x".repeat(199));
        assert!(poor_summary(&clipped).is_some());
        assert!(
            poor_summary("The wiki stays on MoinMoin until the S2 spike says otherwise.").is_none()
        );
        assert_eq!(
            normalise("DR-19 — Does the wiki stay on MoinMoin?"),
            "dr19 does the wiki stay on moinmoin"
        );
        assert_eq!(Kind::Resummarise.name(), "resummarise");
    }
}
