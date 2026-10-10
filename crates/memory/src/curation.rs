//! The librarian's own telemetry, and the curation cycle. The paper has curation
//! monitored by the system rather than by the librarians: demotion rates, how much a
//! summary drifted against how much the body changed, and anomalies alerted per
//! librarian identity. Here the signals are computed from the record and the ledger,
//! which librarians do not write to except through entries that `supersedes` another,
//! so a librarian cannot suppress them. A cycle runs the survey, the checker, the
//! canaries and this telemetry together under a budget, and logs what it did.

use dm_server::Memory;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// What one author's curation looked like.
#[derive(Clone, Debug, Default, Serialize)]
pub struct AuthorStats {
    /// Entries this author wrote that supersede another.
    pub curations: usize,
    /// Of those, rewrites that left the body as it was and changed only the summary.
    pub summary_only: usize,
    /// Of those, rewrites that changed the body.
    pub body_changed: usize,
    /// How much the bodies changed, averaged: one minus the word overlap.
    pub mean_body_change: f64,
    /// Entries this author demoted.
    pub demotions: usize,
    /// Curation tasks this author completed, from the ledger.
    pub tasks_completed: usize,
    /// Entries this author wrote, from the write log.
    pub writes: usize,
}

/// Something the telemetry alerts on.
#[derive(Clone, Debug, Serialize)]
pub struct Alarm {
    pub kind: String,
    pub who: String,
    pub why: String,
}

/// The telemetry report.
#[derive(Debug, Default, Serialize)]
pub struct Telemetry {
    pub domain: String,
    pub by_author: BTreeMap<String, AuthorStats>,
    pub alarms: Vec<Alarm>,
}

/// One minus the Jaccard overlap of the two texts' words: zero for the same text, one
/// for texts with no word in common.
#[must_use]
pub fn body_change(a: &str, b: &str) -> f64 {
    let wa: BTreeSet<&str> = a.split_whitespace().collect();
    let wb: BTreeSet<&str> = b.split_whitespace().collect();
    if wa.is_empty() && wb.is_empty() {
        return 0.0;
    }
    let inter = wa.intersection(&wb).count() as f64;
    let union = wa.union(&wb).count() as f64;
    1.0 - inter / union
}

/// Compute the telemetry for a domain from the record and the ledger.
///
/// # Errors
///
/// When the record cannot be read.
pub async fn telemetry(
    memory: &Memory,
    domain: Option<&str>,
) -> Result<Telemetry, Box<dyn std::error::Error>> {
    let domain = memory.resolve_domain(domain)?;
    let mut t = Telemetry {
        domain: domain.clone(),
        ..Telemetry::default()
    };
    let active = memory.list(Some(&domain), None, 5000).await?;
    curations(memory, &active, &mut t).await;
    demotions(memory, &domain, &active, &mut t).await?;
    completed(memory, &domain, &mut t).await?;
    rates(&mut t);
    channels(memory.root(), &mut t);
    Ok(t)
}

/// Memory as a channel: the shapes a rogue agent's traffic takes in the write and
/// read logs, without reading the record. An author writing far more than curation
/// or handoffs explain; an agent's entry read by several other sessions within an
/// hour of being written, which is what a broadcast looks like from the outside.
fn channels(root: &Path, t: &mut Telemetry) {
    let writes = dm_server::reads::all_writes(root).unwrap_or_default();
    let reads = dm_server::reads::all(root).unwrap_or_default();
    let parse = |s: &str| {
        time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).ok()
    };
    let mut per_hour: BTreeMap<(String, String), usize> = BTreeMap::new();
    for w in &writes {
        let hour = w.at.get(..13).unwrap_or("").to_string();
        *per_hour.entry((w.author.clone(), hour)).or_default() += 1;
        t.by_author.entry(w.author.clone()).or_default().writes += 1;
    }
    for ((who, hour), n) in per_hour {
        if n >= 30 {
            t.alarms.push(Alarm {
                kind: "write-rate".into(),
                who: who.clone(),
                why: format!("{who} wrote {n} entries in the hour starting {hour}"),
            });
        }
    }
    for w in writes.iter().filter(|w| w.author.starts_with("agent:")) {
        let Some(at) = parse(&w.at) else { continue };
        let readers: BTreeSet<&str> = reads
            .iter()
            .filter(|r| r.session != w.session && r.ids.contains(&w.id))
            .filter(|r| {
                parse(&r.at).is_some_and(|ra| ra >= at && ra - at <= time::Duration::hours(1))
            })
            .map(|r| r.session.as_str())
            .collect();
        if readers.len() >= 3 {
            t.alarms.push(Alarm {
                kind: "relay".into(),
                who: w.author.clone(),
                why: format!(
                    "`{}` by {} was read by {} other sessions within an hour of being written; a broadcast looks like this",
                    w.id,
                    w.author,
                    readers.len()
                ),
            });
        }
    }
}

/// Every active entry that supersedes another is a curation: count it for its author,
/// and compare its body with the one it replaced.
async fn curations(memory: &Memory, active: &[dm_index::Hit], t: &mut Telemetry) {
    let mut changes: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for h in active {
        let Ok(e) = memory.get(&h.id).await else {
            continue;
        };
        let Some(old_id) = &e.front.supersedes else {
            continue;
        };
        let stats = t.by_author.entry(e.front.author.clone()).or_default();
        stats.curations += 1;
        let Ok(old) = memory.archived(old_id).await else {
            continue;
        };
        let change = body_change(&old.body, &e.body);
        if change < 0.001 {
            stats.summary_only += 1;
            continue;
        }
        stats.body_changed += 1;
        changes
            .entry(e.front.author.clone())
            .or_default()
            .push(change);
        // a summary rewrite that rewrote the body is the drift the paper watches
        if change > 0.3 && e.front.summary.is_some() && old.front.summary != e.front.summary {
            t.alarms.push(Alarm {
                kind: "body-drift".into(),
                who: e.front.author.clone(),
                why: format!(
                    "`{}` supersedes `{old_id}` with a new summary and a body changed by {:.0}%",
                    e.front.id,
                    change * 100.0
                ),
            });
        }
    }
    for (who, v) in changes {
        if let Some(s) = t.by_author.get_mut(&who) {
            s.mean_body_change = v.iter().sum::<f64>() / v.len() as f64;
        }
    }
}

/// Archived entries whose status says demoted, counted for the author of the demotion.
async fn demotions(
    memory: &Memory,
    domain: &str,
    active: &[dm_index::Hit],
    t: &mut Telemetry,
) -> Result<(), Box<dyn std::error::Error>> {
    let active_ids: BTreeSet<&str> = active.iter().map(|h| h.id.as_str()).collect();
    for h in memory.list_archive(Some(domain), 5000).await? {
        if active_ids.contains(h.id.as_str()) {
            continue;
        }
        if let Ok(e) = memory.archived(&h.id).await {
            if e.front.status == Some(dm_core::Status::Demoted) {
                t.by_author
                    .entry(e.front.author.clone())
                    .or_default()
                    .demotions += 1;
            }
        }
    }
    Ok(())
}

/// Curation tasks completed, per holder, from the ledger.
async fn completed(
    memory: &Memory,
    domain: &str,
    t: &mut Telemetry,
) -> Result<(), Box<dyn std::error::Error>> {
    for task in memory
        .tasks(Some(domain), dm_ledger::Status::Completed)
        .await?
    {
        let curation = task
            .spec
            .origin
            .as_deref()
            .is_some_and(|o| o.starts_with("librarian:") || o.starts_with("checker:"));
        if let (true, Some(holder)) = (curation, &task.holder) {
            t.by_author
                .entry(holder.clone())
                .or_default()
                .tasks_completed += 1;
        }
    }
    Ok(())
}

/// The per-author rates the paper alerts on: rewrites that keep changing bodies, and a
/// demotion rate out of line with the curation done.
fn rates(t: &mut Telemetry) {
    for (who, s) in &t.by_author {
        if s.curations >= 5 && s.body_changed * 2 > s.curations {
            t.alarms.push(Alarm {
                kind: "body-heavy".into(),
                who: who.clone(),
                why: format!(
                    "{who} changed the body in {} of {} curations; a librarian's rewrites should mostly be summaries",
                    s.body_changed, s.curations
                ),
            });
        }
        if s.demotions >= 5 && s.demotions * 4 > s.curations.max(1) {
            t.alarms.push(Alarm {
                kind: "demotion-rate".into(),
                who: who.clone(),
                why: format!(
                    "{who} demoted {} entries against {} curations",
                    s.demotions, s.curations
                ),
            });
        }
    }
}

/// File each alarm as a task for the owner, once per alarm.
///
/// # Errors
///
/// A ledger error.
pub async fn file_alarms(
    memory: &Memory,
    t: &Telemetry,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let mut ids = Vec::new();
    for a in &t.alarms {
        let created = memory
            .file_own(dm_ledger::TaskSpec {
                domain: t.domain.clone(),
                title: format!("Curation alarm, {}: {}", a.kind, a.who),
                body: format!(
                    "{}\n\nThe librarian's telemetry, computed from the record and the ledger rather than written by any librarian, raised this. Inspect the entries named and the tasks `{}` completed; every curation is a reversible write, so what was changed can be reverted. This is for the owner.",
                    a.why, a.who
                ),
                requirements: vec!["human".into()],
                depends_on: Vec::new(),
                origin: Some(format!("telemetry:{}", a.kind)),
                upstream_event: Some(dm_core::digest(&a.why)[..16].to_string()),
                created_by: String::new(),
                classification: dm_core::Label::Public,
                priority: 5,
            })
            .await?;
        if !created.deduplicated {
            ids.push(created.task.id);
        }
    }
    Ok(ids)
}

/// One curation cycle's record, appended to `state/curation.jsonl`.
#[derive(Debug, Default, Serialize)]
pub struct Cycle {
    pub at: String,
    pub domain: String,
    pub proposals: usize,
    pub filed: usize,
    pub already_filed: usize,
    pub open_before: usize,
    pub budget_tasks: usize,
    pub checked: usize,
    pub rejected: usize,
    pub contradictions: usize,
    pub canaries_tripped: usize,
    pub alarms: usize,
    pub checker_model: String,
    /// Model calls the cycle made, and the tokens they took, as the egress metered them.
    pub calls: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    /// The cycle's token budget; the checker stops at it.
    pub budget_tokens: u64,
    /// Held writes the quarantine's reader reviewed this cycle, and how many of them it
    /// called malicious.
    pub reviewed: usize,
    pub malicious: usize,
}

/// Append a cycle's record.
///
/// # Errors
///
/// When the log cannot be written.
pub fn log_cycle(root: &Path, cycle: &Cycle) -> std::io::Result<()> {
    use std::io::Write;
    let path = root.join("state/curation.jsonl");
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(f, "{}", serde_json::to_string(cycle).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_change_is_word_overlap() {
        assert_eq!(body_change("a b c", "a b c"), 0.0);
        assert!((body_change("a b c d", "a b") - 0.5).abs() < 1e-9);
        assert_eq!(body_change("a", "b"), 1.0);
        assert_eq!(body_change("", ""), 0.0);
    }
}
