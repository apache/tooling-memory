//! The task fabric's ranking: the rubric an L1 strategy compiles into, applied to a
//! domain's open tasks once per cycle, as the paper has it. A task's rank starts from
//! the priority it was filed with; the rubric adds weight by where it came from and how
//! long it has waited; a task that blocks a ranked one rises above it, so unblocking is
//! up-weighted; a child rises to its parent's rank; and curation has a protected floor
//! of the completed work, so it neither starves delivery nor is starved by it. Every
//! change is a `rescored` event on the ledger with its reason, so the queue's order is
//! as replayable as everything else; a claimed task keeps its lease whatever the
//! rubric says.

use crate::{Error, Memory};
use dm_ledger::{Status, Task};
use dm_policy::record::Rubric;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use time::OffsetDateTime;

/// One task moved in the queue.
#[derive(Clone, Debug, Serialize)]
pub struct Change {
    /// The task.
    pub id: String,
    /// Its title.
    pub title: String,
    /// Its place before.
    pub from: i64,
    /// Its place now.
    pub to: i64,
    /// Why, in the rubric's terms.
    pub why: String,
}

/// What a rescore did.
#[derive(Clone, Debug, Serialize)]
pub struct Rescore {
    /// The domain.
    pub domain: String,
    /// The policy whose rubric ranked it, or `default`.
    pub rubric: String,
    /// The tasks it moved.
    pub changed: Vec<Change>,
    /// Curation's share of the completed work, and the floor.
    pub maintenance_share: f64,
    /// The rubric's floor.
    pub floor: f64,
}

/// Whether a task is curation: the survey's, the checker's, the telemetry's.
#[must_use]
pub fn maintenance(t: &Task) -> bool {
    t.spec.origin.as_deref().is_some_and(|o| {
        o.starts_with("librarian:") || o.starts_with("checker:") || o.starts_with("telemetry:")
    })
}

/// A task's family for the statistics: the origin before its colon, or `filed`.
fn family(t: &Task) -> String {
    t.spec
        .origin
        .as_deref()
        .and_then(|o| o.split(':').next())
        .unwrap_or("filed")
        .to_string()
}

type Ranks = HashMap<String, (i64, String)>;

/// Raise blockers above what they block and children to their parents, until nothing
/// moves; bounded by the number of tasks, since each pass settles at least one edge.
fn propagate(open: &[Task], claimed: &[Task], rubric: &Rubric, ranks: &mut Ranks) {
    let current = |ranks: &Ranks, id: &str| -> Option<i64> {
        ranks
            .get(id)
            .map(|(r, _)| *r)
            .or_else(|| claimed.iter().find(|c| c.id == id).map(|c| c.spec.priority))
    };
    for _ in 0..=open.len() {
        let mut moved = false;
        for t in open {
            let mut best = ranks.get(&t.id).map_or(0, |(r, _)| *r);
            let mut why = None;
            for d in open
                .iter()
                .chain(claimed)
                .filter(|d| d.spec.depends_on.contains(&t.id))
            {
                if let Some(dr) = current(ranks, &d.id) {
                    if dr + rubric.unblock_weight > best {
                        best = dr + rubric.unblock_weight;
                        why = Some(format!("unblocks {}", d.id));
                    }
                }
            }
            if let Some(parent) = t
                .spec
                .origin
                .as_deref()
                .and_then(|o| o.strip_prefix("task:"))
            {
                if let Some(pr) = current(ranks, parent) {
                    if pr > best {
                        best = pr;
                        why = Some(format!("a child of {parent}"));
                    }
                }
            }
            if let Some(why) = why {
                ranks.insert(t.id.clone(), (best, why));
                moved = true;
            }
        }
        if !moved {
            break;
        }
    }
}

/// Curation's floor: when its share of the completed work is under the floor, the best
/// curation task waiting goes to the head of the queue. Returns the share.
fn apply_floor(open: &[Task], completed: &[Task], rubric: &Rubric, ranks: &mut Ranks) -> f64 {
    if completed.is_empty() {
        return 1.0;
    }
    #[allow(clippy::cast_precision_loss)] // counts of tasks
    let share = completed.iter().filter(|t| maintenance(t)).count() as f64 / completed.len() as f64;
    if share >= rubric.maintenance_floor {
        return share;
    }
    let rank_of = |t: &Task, ranks: &Ranks| ranks.get(&t.id).map_or(t.spec.priority, |(r, _)| *r);
    let top_delivery = open
        .iter()
        .filter(|t| !maintenance(t))
        .map(|t| rank_of(t, ranks))
        .max();
    // the floor is for curation an agent can do; an alarm for the owner waits for a person
    let best_curation = open
        .iter()
        .filter(|t| maintenance(t) && !t.spec.requirements.iter().any(|r| r == "human"))
        .max_by_key(|t| rank_of(t, ranks));
    if let (Some(top), Some(c)) = (top_delivery, best_curation) {
        if rank_of(c, ranks) <= top {
            ranks.insert(
                c.id.clone(),
                (
                    top + 1,
                    format!(
                        "curation's floor: {:.0}% of the completed work, under {:.0}%",
                        share * 100.0,
                        rubric.maintenance_floor * 100.0
                    ),
                ),
            );
        }
    }
    share
}

/// Filings held back for requirements their filer does not hold, per creator, from the
/// decision log: the paper's requirements inflation as telemetry, since a task with
/// inflated requirements is a message addressed to the most capable claimant.
#[must_use]
pub fn inflated_by_creator(root: &std::path::Path) -> BTreeMap<String, usize> {
    let text = std::fs::read_to_string(root.join("state/decisions.jsonl")).unwrap_or_default();
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let inflated = v["action"] == "file_task"
            && v["reason"]
                .as_str()
                .is_some_and(|r| r.contains("does not hold"));
        if let (true, Some(who)) = (inflated, v["principal"].as_str()) {
            *out.entry(who.to_string()).or_default() += 1;
        }
    }
    out
}

impl Memory {
    /// The rubric in force: the last verified L1 policy that carries one, by id, or the
    /// default.
    #[must_use]
    pub fn rubric(&self) -> (Rubric, String) {
        let p = self.policy();
        let mut l1: Vec<_> = p
            .entries()
            .iter()
            .filter(|e| e.tier == dm_core::TrustTier::Verified && e.meta.has_rubric())
            .collect();
        l1.sort_by(|a, b| a.meta.id.cmp(&b.meta.id));
        l1.last()
            .and_then(|e| e.meta.rubric.clone().map(|r| (r, e.meta.id.clone())))
            .unwrap_or_else(|| (Rubric::default(), "default".into()))
    }

    /// Rank a domain's open tasks by the rubric, once: the paper's cycle.
    ///
    /// # Errors
    ///
    /// An ungranted domain, or a ledger error.
    pub async fn rescore(&self, domain: Option<&str>) -> Result<Rescore, Error> {
        let domain = self.resolve_domain(domain)?;
        let (rubric, from) = self.rubric();
        let mut l = self.ledger.lock().await;
        let mut open = l.list(&domain, Status::Admitted).await?;
        open.extend(l.list(&domain, Status::Proposed).await?);
        let claimed = l.list(&domain, Status::Claimed).await?;
        let completed = l.list(&domain, Status::Completed).await?;
        let now = OffsetDateTime::now_utc();
        let mut ranks: Ranks = HashMap::new();
        for t in &open {
            let filed = l.filed_priority(&t.id).await?;
            let weight = rubric.origin_weight(t.spec.origin.as_deref());
            let age = (now - t.created).whole_days() * rubric.age_weight_per_day;
            ranks.insert(
                t.id.clone(),
                (
                    filed + weight + age,
                    format!("filed at {filed}, origin {weight:+}, age {age:+}"),
                ),
            );
        }
        propagate(&open, &claimed, &rubric, &mut ranks);
        let share = apply_floor(&open, &completed, &rubric, &mut ranks);
        let mut changed = Vec::new();
        for t in &open {
            let (to, why) = ranks
                .get(&t.id)
                .cloned()
                .unwrap_or((t.spec.priority, String::new()));
            if to != t.spec.priority {
                l.rescore(&t.id, to, &why).await?;
                changed.push(Change {
                    id: t.id.clone(),
                    title: t.spec.title.clone(),
                    from: t.spec.priority,
                    to,
                    why,
                });
            }
        }
        Ok(Rescore {
            domain,
            rubric: from,
            changed,
            maintenance_share: share,
            floor: rubric.maintenance_floor,
        })
    }

    /// How tasks flow in a domain, the paper's fabric telemetry: by family, how many
    /// are in each status; why tasks were invalidated; and the requirements of waiting
    /// tasks that no completed task ever carried, a fleet capability gap.
    ///
    /// # Errors
    ///
    /// An ungranted domain, or a ledger error.
    pub async fn task_stats(&self, domain: Option<&str>) -> Result<serde_json::Value, Error> {
        let domain = self.resolve_domain(domain)?;
        let l = self.ledger.lock().await;
        let mut by_family: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
        let mut reasons: BTreeMap<String, usize> = BTreeMap::new();
        let mut done_caps = std::collections::BTreeSet::new();
        let mut waiting = Vec::new();
        for status in [
            Status::Proposed,
            Status::Admitted,
            Status::Claimed,
            Status::Completed,
            Status::Invalidated,
        ] {
            for t in l.list(&domain, status).await? {
                *by_family
                    .entry(family(&t))
                    .or_default()
                    .entry(status.as_str().to_string())
                    .or_default() += 1;
                match status {
                    Status::Invalidated => {
                        *reasons
                            .entry(t.reason.clone().unwrap_or_default())
                            .or_default() += 1;
                    }
                    Status::Completed => done_caps.extend(t.spec.requirements.clone()),
                    Status::Admitted => waiting.push(t),
                    _ => {}
                }
            }
        }
        let unmatched: BTreeMap<String, usize> = waiting
            .iter()
            .flat_map(|t| t.spec.requirements.iter())
            .filter(|r| !done_caps.contains(*r))
            .fold(BTreeMap::new(), |mut m, r| {
                *m.entry(r.clone()).or_default() += 1;
                m
            });
        Ok(serde_json::json!({
            "domain": domain, "by_family": by_family, "invalidated_because": reasons,
            "unmatched_requirements": unmatched,
            "inflated_by_creator": inflated_by_creator(&self.root),
        }))
    }
}
