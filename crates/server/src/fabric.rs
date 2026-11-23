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
use std::collections::{BTreeMap, BTreeSet, HashMap};
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

/// Whether a task is curation: the survey's, the auditor's, the telemetry's.
#[must_use]
pub fn maintenance(t: &Task) -> bool {
    t.spec.origin.as_deref().is_some_and(|o| {
        o.starts_with("curator:") || o.starts_with("auditor:") || o.starts_with("telemetry:")
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

/// A rank the rubric gave a task: the rank, why, and every task it came from, however
/// far back, so a report can leave out a rank lent by a task its reader may not see.
#[derive(Clone, Debug)]
struct Rank {
    rank: i64,
    why: String,
    from: Vec<String>,
}

type Ranks = HashMap<String, Rank>;

/// A task's current rank and the tasks it came from: its rank this cycle if it has one,
/// else, for a claimed task, the priority it holds.
fn current(ranks: &Ranks, claimed: &[Task], id: &str) -> Option<(i64, Vec<String>)> {
    ranks.get(id).map(|r| (r.rank, r.from.clone())).or_else(|| {
        claimed
            .iter()
            .find(|c| c.id == id)
            .map(|c| (c.spec.priority, Vec::new()))
    })
}

/// The best rank `t` may take from what it blocks and from its parent, if better than
/// what it has: the rank, why, and where it came from.
fn lent(t: &Task, open: &[Task], claimed: &[Task], rubric: &Rubric, ranks: &Ranks) -> Option<Rank> {
    let mut best = ranks.get(&t.id).map_or(0, |r| r.rank);
    let mut lent = None;
    for d in open
        .iter()
        .chain(claimed)
        .filter(|d| d.spec.depends_on.contains(&t.id))
    {
        if let Some((dr, from)) = current(ranks, claimed, &d.id) {
            if dr + rubric.unblock_weight > best {
                best = dr + rubric.unblock_weight;
                lent = Some((format!("unblocks {}", d.id), d.id.clone(), from));
            }
        }
    }
    let parent = t
        .spec
        .origin
        .as_deref()
        .and_then(|o| o.strip_prefix("task:"));
    if let Some((pr, from)) = parent.and_then(|p| current(ranks, claimed, p)) {
        if pr > best {
            best = pr;
            let p = parent.unwrap_or_default().to_string();
            lent = Some((format!("a child of {p}"), p, from));
        }
    }
    lent.map(|(why, by, mut from)| {
        from.insert(0, by);
        Rank {
            rank: best,
            why,
            from,
        }
    })
}

/// Raise blockers above what they block and children to their parents, until nothing
/// moves; bounded by the number of tasks, since each pass settles at least one edge.
fn propagate(open: &[Task], claimed: &[Task], rubric: &Rubric, ranks: &mut Ranks) {
    for _ in 0..=open.len() {
        let mut moved = false;
        for t in open {
            if let Some(r) = lent(t, open, claimed, rubric, ranks) {
                ranks.insert(t.id.clone(), r);
                moved = true;
            }
        }
        if !moved {
            break;
        }
    }
}

/// The share of completed work that is curation; with nothing completed, all of it is.
fn maintenance_share<'a>(completed: impl Iterator<Item = &'a Task>) -> f64 {
    let (curation, all) = completed.fold((0_usize, 0_usize), |(c, n), t| {
        (c + usize::from(maintenance(t)), n + 1)
    });
    #[allow(clippy::cast_precision_loss)] // counts of tasks
    if all == 0 {
        1.0
    } else {
        curation as f64 / all as f64
    }
}

/// Curation's floor: when its share of the completed work is under the floor, the best
/// curation task waiting goes to the head of the queue.
fn apply_floor(open: &[Task], completed: &[Task], rubric: &Rubric, ranks: &mut Ranks) {
    let share = maintenance_share(completed.iter());
    if share >= rubric.maintenance_floor {
        return;
    }
    let rank_of = |t: &Task, ranks: &Ranks| ranks.get(&t.id).map_or(t.spec.priority, |r| r.rank);
    let top_delivery = open
        .iter()
        .filter(|t| !maintenance(t))
        .max_by_key(|t| rank_of(t, ranks));
    // the floor is for curation an agent can do; an alarm for the owner waits for a person
    let best_curation = open
        .iter()
        .filter(|t| maintenance(t) && !t.spec.requirements.iter().any(|r| r == "human"))
        .max_by_key(|t| rank_of(t, ranks));
    if let (Some(top), Some(c)) = (top_delivery, best_curation) {
        let top_rank = rank_of(top, ranks);
        if rank_of(c, ranks) <= top_rank {
            let mut from = ranks
                .get(&top.id)
                .map(|r| r.from.clone())
                .unwrap_or_default();
            from.insert(0, top.id.clone());
            ranks.insert(
                c.id.clone(),
                Rank {
                    rank: top_rank + 1,
                    why: format!(
                        "curation's floor: {:.0}% of the completed work, under {:.0}%",
                        share * 100.0,
                        rubric.maintenance_floor * 100.0
                    ),
                    from,
                },
            );
        }
    }
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

/// The origin of the task that a probing alarm files. The task names the agent that it
/// is about, so only a person in person sees a task of this origin.
pub const PROBING_ORIGIN: &str = "telemetry:probing";

/// How many refusals of one agent in one clock hour are reported as probing.
pub const PROBING_DENIALS: usize = 5;

/// One agent that the policy refused many times in one clock hour.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Probing {
    /// The principal that was refused.
    pub who: String,
    /// The clock hour, as the first 13 characters of a time in the log:
    /// `2026-11-15T14`.
    pub hour: String,
    /// How many of its requests were refused in that hour.
    pub denials: usize,
    /// The actions that were refused, each one time, in the order of their names.
    pub actions: Vec<String>,
}

/// The agents that probe, from the decision log: the paper's repeated denials from one
/// agent. Each refused decision whose principal is an agent counts for that principal
/// and the clock hour of the line. A principal with [`PROBING_DENIALS`] or more in one
/// hour is in the result, in the order of principal and hour. The hour comes from the
/// line, so no clock is read.
#[must_use]
pub fn probing(root: &std::path::Path) -> Vec<Probing> {
    let text = std::fs::read_to_string(root.join("state/decisions.jsonl")).unwrap_or_default();
    let mut per_hour: BTreeMap<(String, String), (usize, BTreeSet<String>)> = BTreeMap::new();
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let refused = v["allow"] == false && v["class"] == "agent";
        let hour = v["at"].as_str().and_then(|at| at.get(..13));
        if let (true, Some(who), Some(hour)) = (refused, v["principal"].as_str(), hour) {
            let slot = per_hour
                .entry((who.to_string(), hour.to_string()))
                .or_default();
            slot.0 += 1;
            slot.1.extend(v["action"].as_str().map(str::to_string));
        }
    }
    per_hour
        .into_iter()
        .filter(|(_, (denials, _))| *denials >= PROBING_DENIALS)
        .map(|((who, hour), (denials, actions))| Probing {
            who,
            hour,
            denials,
            actions: actions.into_iter().collect(),
        })
        .collect()
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

    /// Rank a domain's open tasks by the rubric, once: the paper's cycle. Every open task
    /// is ranked, whatever the session's handling; the changes reported are those within
    /// it, read, and only those whose rank came from nothing above it. The report hides
    /// the reason, not the effect: the rank a hidden task lends is stored, and a listing
    /// shows the priority it set, from which a hidden task's rank can be read.
    ///
    /// # Errors
    ///
    /// An ungranted domain, or a ledger error.
    pub async fn rescore(&self, domain: Option<&str>) -> Result<Rescore, Error> {
        let domain = self.resolve_domain(domain)?;
        let (rubric, from) = self.rubric();
        let mut l = self.ledger.lock().await;
        let mut open = l.list(&domain, Status::Open).await?;
        // a blocked task is ranked too: it lends its rank to what blocks it
        open.extend(l.list(&domain, Status::Blocked).await?);
        open.extend(l.list(&domain, Status::Proposed).await?);
        let claimed = l.list(&domain, Status::Claimed).await?;
        let completed = l.list(&domain, Status::Done).await?;
        let now = OffsetDateTime::now_utc();
        let mut ranks: Ranks = HashMap::new();
        for t in &open {
            let filed = l.filed_priority(&t.id).await?;
            let weight = rubric.origin_weight(t.spec.origin.as_deref());
            let age = (now - t.created).whole_days() * rubric.age_weight_per_day;
            ranks.insert(
                t.id.clone(),
                Rank {
                    rank: filed + weight + age,
                    why: format!("filed at {filed}, origin {weight:+}, age {age:+}"),
                    from: Vec::new(),
                },
            );
        }
        propagate(&open, &claimed, &rubric, &mut ranks);
        apply_floor(&open, &completed, &rubric, &mut ranks);
        let mut changed = Vec::new();
        for t in &open {
            let Some(r) = ranks.get(&t.id).filter(|r| r.rank != t.spec.priority) else {
                continue;
            };
            l.rescore(&t.id, r.rank, &r.why).await?;
            changed.push((
                Change {
                    id: t.id.clone(),
                    title: t.spec.title.clone(),
                    from: t.spec.priority,
                    to: r.rank,
                    why: r.why.clone(),
                },
                r.from.clone(),
            ));
        }
        // the queue is ranked whole, since rank lent by a task this session may not see
        // is rank all the same; the session is told only of a change to a task it may
        // see whose rank came from nothing it may not see, however far back, since the
        // reason names the lender and the new rank gives its rank away. This hides the
        // reason, not the effect: the stored priority is the same rank, and tasks() shows
        // it, so a hidden task's rank is still readable from what it lifted
        let handling = self.handling()?;
        let visible: HashMap<&str, &Task> = open
            .iter()
            .chain(&claimed)
            .filter(|t| self.sees(t, handling))
            .map(|t| (t.id.as_str(), t))
            .collect();
        let changed: Vec<Change> = changed
            .into_iter()
            .filter(|(c, from)| {
                visible.contains_key(c.id.as_str())
                    && from.iter().all(|f| visible.contains_key(f.as_str()))
            })
            .map(|(c, _)| c)
            .collect();
        for c in &changed {
            if let Some(t) = visible.get(c.id.as_str()) {
                self.observe_task(t);
            }
        }
        Ok(Rescore {
            domain,
            rubric: from,
            changed,
            // the floor weighs all completed work; the report counts only what it may see
            maintenance_share: maintenance_share(
                completed.iter().filter(|t| self.sees(t, handling)),
            ),
            floor: rubric.maintenance_floor,
        })
    }

    /// How tasks flow in a domain, the paper's fabric telemetry: by family, how many
    /// are in each status; why tasks were invalidated; and the requirements of waiting
    /// tasks that no completed task ever carried, a fleet capability gap. Only tasks
    /// within the session's handling are counted, and each counted is read.
    ///
    /// # Errors
    ///
    /// An ungranted domain, or a ledger error.
    pub async fn task_stats(&self, domain: Option<&str>) -> Result<serde_json::Value, Error> {
        let domain = self.resolve_domain(domain)?;
        let l = self.ledger.lock().await;
        let mut by_family: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
        let mut reasons: BTreeMap<String, usize> = BTreeMap::new();
        let mut done_caps = BTreeSet::new();
        let mut waiting = Vec::new();
        for status in [
            Status::Proposed,
            Status::Open,
            Status::Blocked,
            Status::Claimed,
            Status::Done,
            Status::Invalidated,
        ] {
            for t in self.read_tasks(l.list(&domain, status).await?)? {
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
                    Status::Done => done_caps.extend(t.spec.requirements.clone()),
                    Status::Open | Status::Blocked => waiting.push(t),
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
            "probing": probing(&self.root),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// One line of the decision log, as [`Memory::log_decision`] writes it.
    fn decision(at: &str, principal: &str, class: &str, action: &str, allow: bool) -> String {
        serde_json::json!({
            "at": at, "session": "s-1", "action": action, "principal": principal,
            "class": class, "domain": "dom-me", "allow": allow, "admit": false,
            "review": "none", "reason": "a reason",
        })
        .to_string()
    }

    /// `count` refusals of `principal` in the clock hour `hour`, one for each minute.
    fn refusals(hour: &str, principal: &str, class: &str, count: usize) -> Vec<String> {
        (0..count)
            .map(|minute| {
                let at = format!("{hour}:{minute:02}:07.5Z");
                decision(&at, principal, class, "write", false)
            })
            .collect()
    }

    /// A root whose decision log holds `lines`.
    fn logged(dir: &Path, lines: &[String]) {
        std::fs::create_dir_all(dir.join("state")).unwrap();
        std::fs::write(dir.join("state/decisions.jsonl"), lines.join("\n") + "\n").unwrap();
    }

    fn found(lines: &[String]) -> Vec<Probing> {
        let dir = tempfile::tempdir().unwrap();
        logged(dir.path(), lines);
        probing(dir.path())
    }

    /// Rule 10: where the count starts, and what is in a report.
    #[test]
    fn five_refusals_of_one_agent_in_one_clock_hour_are_probing() {
        let hour = "2026-11-15T14";
        let mut lines = refusals(hour, "agent:a", "agent", PROBING_DENIALS - 1);
        assert_eq!(found(&lines), []);
        lines.push(decision(
            "2026-11-15T14:59:59Z",
            "agent:a",
            "agent",
            "promote",
            false,
        ));
        lines.push("not a line of the log".into());
        let report = found(&lines);
        assert_eq!(
            report,
            [Probing {
                who: "agent:a".into(),
                hour: hour.into(),
                denials: PROBING_DENIALS,
                actions: vec!["promote".into(), "write".into()],
            }]
        );
        assert_eq!(PROBING_DENIALS, 5);
    }

    /// Rule 10: what does not count.
    #[test]
    fn refusals_in_other_hours_of_other_principals_and_of_persons_are_not_added_up() {
        // five across two clock hours
        let mut lines = refusals("2026-11-15T14", "agent:a", "agent", 3);
        lines.extend(refusals("2026-11-15T15", "agent:a", "agent", 2));
        assert_eq!(found(&lines), []);
        // three each of two agents
        let mut lines = refusals("2026-11-15T14", "agent:a", "agent", 3);
        lines.extend(refusals("2026-11-15T14", "agent:b", "agent", 3));
        assert_eq!(found(&lines), []);
        // the owner, another person, a tap and the system
        for (who, class) in [
            ("human:test:me", "owner"),
            ("human:test:reviewer", "human"),
            ("tap:checks", "tap"),
            ("curation", "system"),
        ] {
            assert_eq!(
                found(&refusals("2026-11-15T14", who, class, 9)),
                [],
                "{who}"
            );
        }
        // decisions that allowed the request
        let allowed: Vec<String> = (0..9)
            .map(|_| decision("2026-11-15T14:00:00Z", "agent:a", "agent", "write", true))
            .collect();
        assert_eq!(found(&allowed), []);
        // each agent and each hour is a report of its own
        let mut lines = refusals("2026-11-15T15", "agent:b", "agent", 6);
        lines.extend(refusals("2026-11-15T14", "agent:a", "agent", 5));
        lines.extend(refusals("2026-11-15T16", "agent:a", "agent", 4));
        let report = found(&lines);
        let who: Vec<_> = report
            .iter()
            .map(|p| (p.who.as_str(), p.hour.as_str(), p.denials))
            .collect();
        assert_eq!(
            who,
            [
                ("agent:a", "2026-11-15T14", 5),
                ("agent:b", "2026-11-15T15", 6)
            ]
        );
    }

    /// The task of a probing alarm names the agent that it is about. That agent does not
    /// see the task, or the task would show the agent where the count starts.
    #[tokio::test]
    async fn the_task_of_a_probing_alarm_is_seen_only_by_a_person_in_person() {
        let dir = tempfile::tempdir().unwrap();
        let owner = crate::testing::root(dir.path());
        let spec = |origin: &str| dm_ledger::TaskSpec {
            domain: "dom-me".into(),
            title: "Curation alarm, probing: agent:a".into(),
            body: "the policy refused agent:a at least 5 times".into(),
            requirements: vec!["human".into()],
            origin: Some(origin.into()),
            ..dm_ledger::TaskSpec::default()
        };
        let alarm = owner.file_own(spec(PROBING_ORIGIN)).await.unwrap().task;
        let other = owner.file_own(spec("telemetry:inflation")).await.unwrap();
        let other = other.task;
        let agent = crate::testing::as_author(dir.path(), "agent:a");
        let served = Memory::open(dir.path()).unwrap();
        served.mark_served();
        for session in [&agent, &served] {
            let asked = session.get_task(&alarm.id).await;
            assert!(matches!(asked, Err(Error::Ledger(_))), "{asked:?}");
            let open = session.tasks(Some("dom-me"), dm_ledger::Status::Open);
            let open: Vec<String> = open.await.unwrap().into_iter().map(|t| t.id).collect();
            assert_eq!(open, [other.id.as_str()], "{}", session.author());
        }
        assert_eq!(owner.get_task(&alarm.id).await.unwrap().id, alarm.id);
        let open = owner.tasks(Some("dom-me"), dm_ledger::Status::Open);
        assert_eq!(open.await.unwrap().len(), 2);
    }

    /// Rule 11.
    #[tokio::test]
    async fn the_statistics_of_the_tasks_carry_the_probing_list() {
        let dir = tempfile::tempdir().unwrap();
        let owner = crate::testing::root(dir.path());
        let quiet = owner.task_stats(Some("dom-me")).await.unwrap();
        assert_eq!(quiet["probing"], serde_json::json!([]));
        let path = dir.path().join("state/decisions.jsonl");
        let mut log = std::fs::read_to_string(&path).unwrap_or_default();
        for line in refusals("2026-11-15T14", "agent:a", "agent", 5) {
            log.push_str(&line);
            log.push('\n');
        }
        std::fs::write(&path, log).unwrap();
        let stats = owner.task_stats(Some("dom-me")).await.unwrap();
        assert_eq!(
            stats["probing"],
            serde_json::json!([{
                "who": "agent:a", "hour": "2026-11-15T14", "denials": 5, "actions": ["write"],
            }])
        );
    }
}
