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
    spend: Option<&Spend<'_>>,
) -> Result<Report, Box<dyn std::error::Error>> {
    let domain = memory.resolve_domain(domain)?;
    let hits = sample_of(memory, &domain, sample).await?;
    let mut report = Report {
        domain,
        model: model.to_string(),
        checked: Vec::new(),
    };
    for h in hits {
        if spend.is_some_and(Spend::exhausted) {
            break;
        }
        let Ok(e) = memory.get(&h.id).await else {
            continue;
        };
        if e.front.entry_type == dm_core::EntryType::Handoff {
            continue;
        }
        let body: String = e.body.chars().take(8000).collect();
        let (path, req) =
            crate::mine::request_with(destination, model, &question(&h.title, &h.summary, &body));
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

/// A token budget for one cycle's model calls, read back from the egress's own
/// metering of this session: what the checker has spent is what the egress saw.
pub struct Spend<'a> {
    pub egress: &'a Egress,
    pub session: &'a str,
    /// Input and output tokens the cycle may spend in all.
    pub limit: u64,
}

impl Spend<'_> {
    /// Tokens this session has spent so far, input and output, as the egress counted.
    #[must_use]
    pub fn spent(&self) -> u64 {
        self.egress
            .telemetry()
            .session(self.session)
            .ok()
            .flatten()
            .map_or(0, |t| t.input_tokens + t.output_tokens)
    }

    /// Whether the budget is spent.
    #[must_use]
    pub fn exhausted(&self) -> bool {
        self.spent() >= self.limit
    }
}

/// The newest entries of a domain that are not handoffs: what the checker samples.
///
/// # Errors
///
/// When the record cannot be read.
pub async fn sample_of(
    memory: &Memory,
    domain: &str,
    sample: usize,
) -> Result<Vec<dm_index::Hit>, Box<dyn std::error::Error>> {
    // each kind's list is newest first; taking them in turn keeps that order near enough
    let mut lists = Vec::new();
    for kind in [
        dm_core::EntryType::Fact,
        dm_core::EntryType::Decision,
        dm_core::EntryType::Convention,
        dm_core::EntryType::Method,
    ] {
        lists.push(
            memory
                .list(Some(domain), Some(kind), sample)
                .await?
                .into_iter(),
        );
    }
    let mut hits = Vec::new();
    while hits.len() < sample {
        let before = hits.len();
        for l in &mut lists {
            if let Some(h) = l.next() {
                hits.push(h);
            }
        }
        if hits.len() == before {
            break;
        }
    }
    hits.truncate(sample);
    Ok(hits)
}

/// Two entries the checker found at odds.
#[derive(Clone, Debug, Serialize)]
pub struct Contradiction {
    pub a: String,
    pub b: String,
    pub why: String,
}

/// The question put to the model about two entries that retrieval puts side by side.
#[must_use]
pub fn contradiction_question(a_title: &str, a_body: &str, b_title: &str, b_body: &str) -> String {
    format!(
        "You are a checker auditing a record. Below are two entries that a search puts side by side. Do they contradict each other: does one state as true what the other states as false, or give a different answer to the same question? Two entries that merely cover different things, or one of which is more detailed, do not contradict. Answer on the first line with exactly `yes` or `no`, and on the second line one sentence naming the claim at odds.\n\n## Entry A: {a_title}\n\n{a_body}\n\n## Entry B: {b_title}\n\n{b_body}"
    )
}

/// Hunt contradictions: for each of up to `sample` entries, newest first, the two
/// entries a search for its title ranks beside it, and the model's judgement on each
/// pair. Pairs are asked once whichever way round they come up.
///
/// # Errors
///
/// When the record cannot be read or the model cannot be reached.
pub async fn hunt(
    egress: &Egress,
    memory: &Memory,
    destination: &str,
    model: &str,
    domain: Option<&str>,
    sample: usize,
    spend: Option<&Spend<'_>>,
) -> Result<Vec<Contradiction>, Box<dyn std::error::Error>> {
    let domain = memory.resolve_domain(domain)?;
    let hits = sample_of(memory, &domain, sample).await?;
    let mut asked = std::collections::BTreeSet::new();
    let mut found = Vec::new();
    for h in hits {
        if spend.is_some_and(Spend::exhausted) {
            break;
        }
        if h.entry_type == dm_core::EntryType::Handoff {
            continue;
        }
        let Ok(a) = memory.get(&h.id).await else {
            continue;
        };
        let beside = memory.search(&h.title, Some(&domain), 3).await?;
        for o in beside.into_iter().filter(|o| o.id != h.id).take(2) {
            if o.entry_type == dm_core::EntryType::Handoff {
                continue;
            }
            let mut pair = [h.id.clone(), o.id.clone()];
            pair.sort();
            if !asked.insert(pair.clone()) {
                continue;
            }
            let Ok(b) = memory.get(&o.id).await else {
                continue;
            };
            let clip = |t: &str| t.chars().take(5000).collect::<String>();
            let q = contradiction_question(&h.title, &clip(&a.body), &o.title, &clip(&b.body));
            let (path, req) = crate::mine::request_with(destination, model, &q);
            let answer = egress.call(destination, &path, &req).await?;
            let (yes, why) = verdict_of(&crate::mine::answer_text(destination, &answer));
            if yes {
                found.push(Contradiction {
                    a: pair[0].clone(),
                    b: pair[1].clone(),
                    why,
                });
            }
        }
    }
    Ok(found)
}

/// The task that has a curator settle a contradiction the checker found.
fn reconcile_body(model: &str, c: &Contradiction) -> String {
    format!(
        "The checker, on {model}, found `{}` and `{}` at odds: {}\n\nRead both with `get`. Find out which is right, from the record and the working tree. `write` the corrected entry with `supersedes` naming the wrong one: its body says what is right, and its `source` names what shows it, as `file:<path>` relative to the working tree or as an entry id. If both are right about different things, give each a title that says which. Change no facts you cannot verify. Leave a handoff that names the entry you wrote and the one it supersedes, and says what was wrong and how you know; keep that out of the entry, so whoever checks it judges it on its sources first.",
        c.a,
        c.b,
        if c.why.is_empty() { "no claim named" } else { &c.why }
    )
}

/// File a task to reconcile each contradiction, once per pair.
///
/// # Errors
///
/// A ledger error.
pub async fn file_contradictions(
    memory: &Memory,
    domain: &str,
    model: &str,
    found: &[Contradiction],
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let mut ids = Vec::new();
    for c in found {
        let created = memory
            .file_own(dm_ledger::TaskSpec {
                domain: domain.to_string(),
                title: format!("Librarian: reconcile {} and {}", c.a, c.b),
                body: reconcile_body(model, c),
                requirements: Vec::new(),
                depends_on: Vec::new(),
                origin: Some("checker:contradiction".into()),
                upstream_event: Some(
                    dm_core::digest(&format!("{}\n{}", c.a, c.b))[..16].to_string(),
                ),
                created_by: String::new(),
                classification: dm_core::Label::Public,
                priority: 1,
            })
            .await?;
        if !created.deduplicated {
            ids.push(created.task.id);
        }
    }
    Ok(ids)
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
            .file_own(dm_ledger::TaskSpec {
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
        let q = contradiction_question("A", "a", "B", "b");
        assert!(q.contains("## Entry A: A") && q.contains("## Entry B: B"));
    }

    #[test]
    fn a_reconciled_entry_leaves_the_reasoning_to_the_handoff() {
        let c = Contradiction {
            a: "mem-t-1".into(),
            b: "mem-t-2".into(),
            why: "one says MoinMoin stays, the other that it went".into(),
        };
        crate::librarian::tests::reasoning_goes_to_the_handoff(&reconcile_body("m", &c));
    }
}
