//! The checker: the librarian's independent counterpart. The paper's audit agents
//! verify that a summary entails its body, sampled continuously, on a different base
//! model and prompt lineage than the librarians, sharing no writable state with them.
//! Here the checker samples a domain's entries, asks a model of its own whether each
//! summary entails its body, and writes nothing to the record: a summary found wanting
//! becomes a task for a librarian run, with the checker's model and reason on it.

use dm_egress::Egress;
use dm_server::Memory;
use serde::Serialize;

/// What the checker found about one entry.
#[derive(Clone, Debug, Serialize)]
pub struct Verdict {
    pub id: String,
    pub title: String,
    /// Whether the summary entails the body, in the model's judgement.
    pub entails: bool,
    /// The model's reason, when it said no.
    pub why: String,
}

/// A checking pass.
#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub domain: String,
    /// The checker's model: by design not the one that wrote the summaries.
    pub model: String,
    pub checked: Vec<Verdict>,
}

/// The question put to the checker's model, one entry at a time.
#[must_use]
pub fn question(title: &str, summary: &str, body: &str) -> String {
    format!(
        "You are a checker auditing an index. Below are an entry's body and the one-line summary a search index serves for it. Does the summary entail the body: does it say nothing the body does not say, and leave out nothing a reader deciding whether to open the entry would need? Answer on the first line with exactly `yes` or `no`, and on the second line one sentence of reason.\n\n## Title\n\n{title}\n\n## Summary\n\n{summary}\n\n## Body\n\n{body}"
    )
}

/// The model's answer as a verdict: `yes` or `no` on the first line, the reason after.
#[must_use]
pub fn verdict_of(answer: &str) -> (bool, String) {
    let mut lines = answer
        .trim()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty());
    let first = lines.next().unwrap_or("").to_lowercase();
    let first = first.trim_matches(|c: char| !c.is_alphanumeric());
    let entails = first.starts_with("yes");
    let why = lines.collect::<Vec<_>>().join(" ");
    (entails, why)
}

/// Check up to `sample` entries of a domain, newest first, through `destination` with
/// `model`. Entries whose body is longer than the model should read in one go are
/// clipped to their first eight thousand characters, which is what a summary is for.
///
/// # Errors
///
/// When the record cannot be read or the model cannot be reached.
pub async fn check(
    egress: &Egress,
    memory: &Memory,
    destination: &str,
    model: &str,
    domain: Option<&str>,
    sample: usize,
) -> Result<Report, Box<dyn std::error::Error>> {
    let domain = memory.resolve_domain(domain)?;
    let hits = memory.list(Some(&domain), None, sample).await?;
    let mut report = Report {
        domain,
        model: model.to_string(),
        checked: Vec::new(),
    };
    for h in hits {
        let Ok(e) = memory.get(&h.id).await else {
            continue;
        };
        if e.front.entry_type == dm_core::EntryType::Handoff {
            continue;
        }
        let body: String = e.body.chars().take(8000).collect();
        let (path, req) =
            crate::mine::request(destination, model, &question(&h.title, &h.summary, &body));
        let answer = egress.call(destination, &path, &req).await?;
        let (entails, why) = verdict_of(&crate::mine::answer_text(destination, &answer));
        report.checked.push(Verdict {
            id: h.id.clone(),
            title: h.title.clone(),
            entails,
            why,
        });
    }
    Ok(report)
}

/// File a task for each summary the checker found wanting, with the checker's model
/// and reason, deduplicated on the entry and the summary it judged.
///
/// # Errors
///
/// A ledger error.
pub async fn file(
    memory: &Memory,
    report: &Report,
    summaries: &[(String, String)],
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let mut ids = Vec::new();
    for v in report.checked.iter().filter(|v| !v.entails) {
        let summary = summaries
            .iter()
            .find(|(id, _)| id == &v.id)
            .map(|(_, s)| s.as_str())
            .unwrap_or("");
        let created = memory
            .create_task(dm_ledger::TaskSpec {
                domain: report.domain.clone(),
                title: format!("Librarian: rewrite the summary of {}, which the checker rejected", v.id),
                body: format!(
                    "The checker, on {}, judged that the summary the index serves for `{id}` does not entail its body: {}\n\nRead `{id}` with `get`. Write a one- or two-sentence summary that says what the body says and no more. Then `write` the entry again with the same body and type, your summary as `summary`, and `supersedes: {id}`. Do not change the body. Leave a handoff naming the new entry.",
                    report.model,
                    if v.why.is_empty() { "no reason given" } else { &v.why },
                    id = v.id
                ),
                requirements: Vec::new(),
                depends_on: Vec::new(),
                origin: Some("checker:summary".into()),
                upstream_event: Some(dm_core::digest(&format!("{}\n{summary}", v.id))[..16].to_string()),
                created_by: String::new(),
                classification: dm_core::Label::Public,
                priority: 0,
            })
            .await?;
        if !created.deduplicated {
            ids.push(created.task.id);
        }
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_verdict_is_the_first_word_and_the_reason_after() {
        assert_eq!(
            verdict_of("yes\nIt says what the body says."),
            (true, "It says what the body says.".into())
        );
        assert_eq!(
            verdict_of("No.\nThe summary claims a decision the body calls open."),
            (
                false,
                "The summary claims a decision the body calls open.".into()
            )
        );
        assert_eq!(verdict_of("**Yes** - fine"), (true, String::new()));
        assert_eq!(verdict_of(""), (false, String::new()));
        let q = question("T", "S", "B");
        assert!(q.contains("## Summary\n\nS") && q.contains("## Body\n\nB"));
    }
}
