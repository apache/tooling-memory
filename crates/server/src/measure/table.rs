//! The metric table of the paper (section 9), answered from the logs.
//!
//! The table always has the 21 rows of [`PAPER_METRICS`], in the paper's order. A row has
//! a value from the ledger, the read log, the decision log or the grants, or it says why
//! it is not measured. No row asks an agent, and no row calls a model. Every window ends
//! at the ledger's clock. A row counts only the tasks that the session may read.

use super::{landing_event, median, Flow, Moments, LANDING_EVENT};
use crate::approvals::SIGN_OFF_ORIGIN;
use crate::autonomy::VERDICT_EVENT;
use crate::limits::Limit;
use crate::read_metrics::{self, Counts};
use crate::reads::{self, Read};
use crate::{fabric, io, Error, Memory, PROPOSAL_ORIGIN};
use dm_ledger::{Ledger, Status, Task};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};

/// The rows of the paper's metric table, in the paper's order and words, shortened.
pub const PAPER_METRICS: [&str; 21] = [
    "Cost per completed task",
    "Fetch-through rate",
    "Wasted reads",
    "Retrieval depth per task",
    "Zero-result and re-query rate",
    "Policy citations and overrides",
    "Denials by domain",
    "Unmatched or invalidated tasks",
    "Approval backlog and decision latency",
    "Decision-relevant omissions in summaries",
    "Found but misleading",
    "Reviewer agreement",
    "Dependency cycles, priority reversals, missed commitments",
    "Verification cost and latency per task definition",
    "Claim-to-landed time",
    "Enforced rules per domain, and denials per task definition",
    "Outbound rate per destination, against its budget",
    "Induced work",
    "Agent-initiated stops and escalations per task definition",
    "Share of throughput spent on upkeep",
    "Pinned tasks, and queue depth per position",
];

/// The function of each row, in the order of [`PAPER_METRICS`].
const ROWS: [fn(&Logs) -> Metric; 21] = [
    cost,
    fetch_through,
    wasted,
    depth,
    requery,
    citations,
    denials,
    unmatched,
    approvals,
    omissions,
    misleading,
    agreement,
    coordination,
    verification,
    landed,
    rules,
    outbound,
    induced,
    stops,
    upkeep,
    positions,
];

/// The tools that pull from the memory: a re-query is a search after a search among them.
const PULLS: [&str; 4] = ["search", "list", "get", "recall"];

/// One row of the metric table.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Metric {
    /// The paper's row: one of [`PAPER_METRICS`].
    pub name: String,
    /// Whether the logs answer the row, in whole or in part.
    pub measured: bool,
    /// The numbers; null when the row is not measured.
    pub value: Value,
    /// Where the value comes from: `ledger`, `read log`, `decision log`, `grants`,
    /// `caller`, or empty when the row is not measured.
    pub from: String,
    /// Why the row is not measured, or which part of a measured row is missing.
    pub why_not: Option<String>,
}

/// The metric table of one domain over a window.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Metrics {
    /// The domain.
    pub domain: String,
    /// The length of the window, in days.
    pub window_days: u32,
    /// The version of the price vector in force.
    pub prices: String,
    /// The 21 rows, in the paper's order.
    pub rows: Vec<Metric>,
}

impl Metric {
    /// A row that the logs answer. The table gives the row its name.
    fn of(from: &str, value: Value) -> Metric {
        Metric {
            name: String::new(),
            measured: true,
            value,
            from: from.to_string(),
            why_not: None,
        }
    }

    /// A measured row of which one part is missing.
    fn less(mut self, missing: &str) -> Metric {
        self.why_not = Some(missing.to_string());
        self
    }

    /// A row that the logs do not answer.
    fn not(why: &str) -> Metric {
        Metric {
            name: String::new(),
            measured: false,
            value: Value::Null,
            from: String::new(),
            why_not: Some(why.to_string()),
        }
    }
}

/// One line of the decision log, as far as the table reads it.
#[derive(Debug, Deserialize)]
struct Decided {
    at: String,
    #[serde(default)]
    domain: String,
    allow: bool,
    #[serde(default)]
    cites: Vec<String>,
    #[serde(default)]
    held_task: Option<String>,
}

/// The time of a log line.
pub(super) fn when(at: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(at, &Rfc3339).ok()
}

/// The lines of the decision log under `root` from `since` on. A line that does not
/// parse is left out. No log is no line.
fn decisions(root: &Path, since: OffsetDateTime) -> Result<Vec<Decided>, Error> {
    let path = root.join("state/decisions.jsonl");
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(&path).map_err(io(&path))?;
    let lines = text.lines().filter_map(|l| serde_json::from_str(l).ok());
    Ok(lines
        .filter(|d: &Decided| when(&d.at).is_some_and(|at| at >= since))
        .collect())
}

#[allow(clippy::cast_precision_loss)] // counts of reads and tasks
fn share(part: usize, whole: usize) -> Option<f64> {
    (whole > 0).then(|| part as f64 / whole as f64)
}

fn add(counts: &mut BTreeMap<String, usize>, key: &str) {
    *counts.entry(key.to_string()).or_default() += 1;
}

/// What the rows are computed from.
struct Logs {
    domain: String,
    /// The domains that the session is granted.
    granted: Vec<String>,
    since: OffsetDateTime,
    now: OffsetDateTime,
    /// What the caller says a completed task costs.
    cost: Option<Value>,
    /// The retrieval metrics of the window.
    retrieval: Counts,
    /// The read log, oldest first.
    reads: Vec<Read>,
    /// The decision log of the window, every domain.
    decisions: Vec<Decided>,
    /// The tasks of the domain that the session reads, by id.
    tasks: BTreeMap<String, Task>,
    moments: Moments,
    flow: Flow,
    /// The enforced rules of the domain.
    rules: usize,
    /// The outbound rate in force, by family.
    rates: BTreeMap<String, u64>,
    /// The share of upkeep in force, in percent.
    ceiling: Option<u64>,
}

impl Logs {
    fn within(&self, at: OffsetDateTime) -> bool {
        at >= self.since
    }

    /// The refused decisions of the domain in the window.
    fn refused(&self) -> impl Iterator<Item = &Decided> {
        let here = |d: &&Decided| !d.allow && d.domain == self.domain;
        self.decisions.iter().filter(here)
    }

    /// The tasks that are neither done nor invalidated, by id.
    fn open(&self) -> BTreeMap<&str, &Task> {
        let open = self.tasks.values().filter(|t| !t.status.is_terminal());
        open.map(|t| (t.id.as_str(), t)).collect()
    }

    /// The tasks that ask a person for an approval: sign-offs and proposals.
    fn approval_tasks(&self) -> impl Iterator<Item = &Task> {
        self.tasks.values().filter(|t| {
            let origin = t.spec.origin.as_deref().unwrap_or_default();
            origin.starts_with(SIGN_OFF_ORIGIN) || origin.starts_with(PROPOSAL_ORIGIN)
        })
    }

    /// When a task closed, if it closed in the window: its completion or invalidation.
    fn closed(&self, t: &Task) -> Option<OffsetDateTime> {
        let kind = match t.status {
            Status::Done => "completed",
            Status::Invalidated => "invalidated",
            _ => return None,
        };
        let at = self.moments.kind(&t.id, kind).next_back()?.at;
        self.within(at).then_some(at)
    }

    /// The approvals closed in the window: whether each was given, and the seconds from
    /// its filing to its closing.
    fn decided(&self) -> Vec<(bool, i64)> {
        let closed = self.approval_tasks().filter_map(|t| {
            let secs = (self.closed(t)? - t.created).whole_seconds();
            Some((t.status == Status::Done, secs))
        });
        closed.collect()
    }

    /// The tasks that have a task definition, each with its definition.
    fn defined(&self) -> impl Iterator<Item = (&Task, &str)> {
        let tasks = self.tasks.values();
        tasks.filter_map(|t| Some((t, t.spec.definition.as_deref()?)))
    }
}

fn cost(x: &Logs) -> Metric {
    match &x.cost {
        Some(v) if !v.is_null() => Metric::of("caller", v.clone()),
        _ => {
            Metric::not("the egress meters spend: amem cost gives this number at the command line")
        }
    }
}

fn fetch_through(x: &Logs) -> Metric {
    let c = &x.retrieval;
    match c.fetch_through_rate {
        Some(rate) => Metric::of(
            "read log",
            json!({"rate": rate, "search_hits": c.search_hits, "fetched_through": c.fetched_through}),
        ),
        None => Metric::not("no search served an entry in the window"),
    }
}

fn wasted(x: &Logs) -> Metric {
    let c = &x.retrieval;
    match c.wasted_read_ratio {
        Some(ratio) => Metric::of(
            "read log",
            json!({"ratio": ratio, "opened": c.opened, "opened_uncited": c.opened_uncited}),
        )
        .less("used means cited by id in the handoff of the session; no judge sampled the reads"),
        None => Metric::not("no session that left a handoff opened an entry in the window"),
    }
}

fn depth(x: &Logs) -> Metric {
    let c = &x.retrieval;
    match c.retrieval_depth {
        Some(depth) => Metric::of(
            "read log",
            json!({"pulls_per_task": depth, "pulls": c.pulls, "tasks_completed": c.tasks_completed}),
        ),
        None => Metric::not("no session completed a task in the window"),
    }
}

fn requery(x: &Logs) -> Metric {
    let mut last: BTreeMap<&str, &str> = BTreeMap::new();
    let (mut searches, mut empty, mut again) = (0, 0, 0);
    for r in &x.reads {
        if !PULLS.contains(&r.tool.as_str()) {
            continue;
        }
        let before = last.insert(r.session.as_str(), r.tool.as_str());
        if r.tool != "search" || !when(&r.at).is_some_and(|at| x.within(at)) {
            continue;
        }
        searches += 1;
        empty += usize::from(r.hits == 0);
        again += usize::from(before == Some("search"));
    }
    if searches == 0 {
        return Metric::not("no search in the window");
    }
    Metric::of(
        "read log",
        json!({
            "searches": searches,
            "zero_result_rate": share(empty, searches),
            "re_query_rate": share(again, searches),
        }),
    )
    .less("the queries are not clustered into entries to write")
}

fn citations(x: &Logs) -> Metric {
    let (mut cited, mut overridden) = (BTreeMap::new(), BTreeMap::new());
    for d in x.decisions.iter().filter(|d| d.domain == x.domain) {
        let counts = if d.allow { &mut overridden } else { &mut cited };
        for policy in &d.cites {
            add(counts, policy);
        }
    }
    Metric::of(
        "decision log",
        json!({"citations": cited, "overrides": overridden}),
    )
}

fn denials(x: &Logs) -> Metric {
    let mut by_domain: BTreeMap<&str, usize> = BTreeMap::new();
    for domain in &x.granted {
        let refused = x
            .decisions
            .iter()
            .filter(|d| !d.allow && d.domain == *domain);
        by_domain.insert(domain, refused.count());
    }
    Metric::of("decision log", json!(by_domain))
}

fn unmatched(x: &Logs) -> Metric {
    let gaps = fabric::Gaps::of(x.tasks.values());
    Metric::of(
        "ledger",
        json!({
            "unmatched_requirements": gaps.unmatched,
            "invalidated_because": gaps.reasons,
        }),
    )
}

fn approvals(x: &Logs) -> Metric {
    let backlog = x.approval_tasks().filter(|t| !t.status.is_terminal());
    let decided = x.decided();
    let latency = median(decided.iter().map(|(_, secs)| *secs).collect());
    let row = Metric::of(
        "ledger",
        json!({
            "backlog": backlog.count(),
            "closed": decided.len(),
            "median_decision_secs": latency,
        }),
    );
    if decided.is_empty() {
        row.less("no approval closed in the window, so there is no latency")
    } else {
        row
    }
}

fn omissions(_: &Logs) -> Metric {
    Metric::not("needs a judge over summaries; this table calls no model")
}

fn misleading(_: &Logs) -> Metric {
    Metric::not("needs paired replays; this table runs none")
}

fn agreement(x: &Logs) -> Metric {
    let decided = x.decided();
    let given = decided.iter().filter(|(given, _)| *given).count();
    match share(given, decided.len()) {
        Some(with_proposer) => Metric::of(
            "ledger",
            json!({"with_proposer": with_proposer, "given": given, "closed": decided.len()}),
        )
        .less(
            "the agreement between reviewers is not measured: one reviewer decides each approval",
        ),
        None => Metric::not("no approval closed in the window"),
    }
}

/// Whether the dependencies of `from` lead to `to` through open tasks.
fn reaches(open: &BTreeMap<&str, &Task>, from: &str, to: &str) -> bool {
    let mut seen = BTreeSet::new();
    let mut next = vec![from];
    while let Some(id) = next.pop() {
        let deps = open.get(id).map_or(&[][..], |t| &t.spec.depends_on[..]);
        for dep in deps {
            if dep == to {
                return true;
            }
            if seen.insert(dep.as_str()) {
                next.push(dep);
            }
        }
    }
    false
}

/// The open tasks that are on a cycle of dependencies, and the open tasks that depend on
/// an open task of lower priority.
fn tangles(open: &BTreeMap<&str, &Task>) -> (usize, usize) {
    let on_cycle = open.keys().filter(|id| reaches(open, id, id));
    let lower = |t: &Task, dep: &String| {
        let dep = open.get(dep.as_str());
        dep.is_some_and(|d| d.spec.priority < t.spec.priority)
    };
    let reversed = open
        .values()
        .filter(|t| t.spec.depends_on.iter().any(|dep| lower(t, dep)));
    (on_cycle.count(), reversed.count())
}

fn coordination(x: &Logs) -> Metric {
    let (on_cycle, reversed) = tangles(&x.open());
    Metric::of(
        "ledger",
        json!({"tasks_on_a_cycle": on_cycle, "priority_reversals": reversed}),
    )
    .less("missed commitments are not counted: this table reads no position")
}

/// The medians of the durations, by key.
fn medians(secs: BTreeMap<String, Vec<i64>>) -> BTreeMap<String, Option<i64>> {
    secs.into_iter().map(|(k, v)| (k, median(v))).collect()
}

fn verification(x: &Logs) -> Metric {
    let mut secs: BTreeMap<String, Vec<i64>> = BTreeMap::new();
    for (t, definition) in x.defined() {
        let event = format!("{VERDICT_EVENT}{}", t.id);
        let all = x.moments.0.get(&t.id).map_or(&[][..], Vec::as_slice);
        for (i, asked) in all.iter().enumerate() {
            if asked.kind != "waiting" || asked.on.as_deref() != Some(event.as_str()) {
                continue;
            }
            let came = all[i + 1..]
                .iter()
                .find(|m| m.kind == "arrived" || m.kind == "completed");
            if let Some(came) = came.filter(|m| x.within(m.at)) {
                let waited = (came.at - asked.at).whole_seconds();
                secs.entry(definition.to_string()).or_default().push(waited);
            }
        }
    }
    if secs.is_empty() {
        return Metric::not(
            "no verdict came in the window; the cost of verification is not measured",
        );
    }
    Metric::of("ledger", json!({"median_latency_secs": medians(secs)}))
        .less("the cost of verification is not measured: this table reads no meter")
}

fn landed(x: &Logs) -> Metric {
    let mut secs: BTreeMap<String, Vec<i64>> = BTreeMap::new();
    for t in x.tasks.values().filter(|t| t.status == Status::Done) {
        let times = x.moments.times(t);
        let Some(queue) = times.queue else {
            continue;
        };
        let at = x.moments.landed_on(&t.id, &landing_event(&queue));
        let claimed = at.filter(|at| x.within(*at)).zip(times.claimed);
        if let Some((at, claimed)) = claimed {
            let waited = (at - claimed).whole_seconds();
            secs.entry(queue).or_default().push(waited);
        }
    }
    if secs.is_empty() {
        return Metric::not("no change landed through a queue in the window");
    }
    let count: BTreeMap<&str, usize> = secs.iter().map(|(q, v)| (q.as_str(), v.len())).collect();
    Metric::of(
        "ledger",
        json!({"landed": count, "median_secs": medians(secs)}),
    )
}

fn rules(x: &Logs) -> Metric {
    let mut by_definition = BTreeMap::new();
    for d in x.refused() {
        let held = d.held_task.as_deref().and_then(|id| x.tasks.get(id));
        if let Some(definition) = held.and_then(|t| t.spec.definition.as_deref()) {
            add(&mut by_definition, definition);
        }
    }
    Metric::of(
        "decision log",
        json!({"enforced_rules": x.rules, "denials": by_definition}),
    )
}

fn outbound(x: &Logs) -> Metric {
    let since = x.now - Duration::hours(24);
    let mut posted = BTreeMap::new();
    for (id, t) in &x.tasks {
        let posts = x.moments.kind(id, "posted").filter(|m| m.at > since);
        match posts.count() {
            0 => {}
            n => *posted.entry(fabric::family(t)).or_insert(0) += n,
        }
    }
    Metric::of(
        "ledger",
        json!({"posted_last_day": posted, "per_day": x.rates}),
    )
}

fn induced(x: &Logs) -> Metric {
    Metric::of(
        "ledger",
        json!({"done": x.flow.induced, "open": x.flow.induced_open}),
    )
}

fn stops(x: &Logs) -> Metric {
    let (mut stopped, mut escalated) = (BTreeMap::new(), BTreeMap::new());
    for (t, definition) in x.defined() {
        let all = x.moments.0.get(&t.id).map_or(&[][..], Vec::as_slice);
        for m in all.iter().filter(|m| x.within(m.at)) {
            let on = m.on.as_deref().unwrap_or_default();
            let checked = on.starts_with(VERDICT_EVENT) || on.starts_with(LANDING_EVENT);
            match m.kind.as_str() {
                "released" => add(&mut stopped, definition),
                "waiting" if !checked => add(&mut escalated, definition),
                _ => {}
            }
        }
    }
    Metric::of(
        "ledger",
        json!({"stops": stopped, "escalations": escalated}),
    )
}

fn upkeep(x: &Logs) -> Metric {
    let Some(share) = x.flow.upkeep_share else {
        return Metric::not("no task was completed in the window");
    };
    let row = Metric::of(
        "ledger",
        json!({
            "share": share,
            "upkeep": x.flow.upkeep,
            "done": x.flow.done,
            "ceiling_percent": x.ceiling,
        }),
    );
    match x.ceiling {
        Some(_) => row,
        None => row.less("the grants set no ceiling for the share of upkeep"),
    }
}

fn positions(_: &Logs) -> Metric {
    Metric::not("this table reads no position")
}

impl Memory {
    /// The retrieval metrics from the read log and the write log: per session and in
    /// total, with the text of each handoff by id. Nothing here is a read of the
    /// session: no line goes to the read log.
    ///
    /// # Errors
    ///
    /// A log that is there and cannot be read.
    pub async fn read_metrics(
        &self,
    ) -> Result<(read_metrics::Metrics, BTreeMap<String, String>), Error> {
        self.read_metrics_since(None).await
    }

    /// [`Memory::read_metrics`] over the lines of the logs from `since` on.
    async fn read_metrics_since(
        &self,
        since: Option<OffsetDateTime>,
    ) -> Result<(read_metrics::Metrics, BTreeMap<String, String>), Error> {
        let keep = |at: &str| match since {
            Some(since) => when(at).is_some_and(|at| at >= since),
            None => true,
        };
        let log = reads::path(&self.root);
        let mut all = reads::all(&self.root).map_err(io(&log))?;
        all.retain(|r| keep(&r.at));
        let written = self.root.join("state/writes.jsonl");
        let mut writes = reads::all_writes(&self.root).map_err(io(&written))?;
        writes.retain(|w| keep(&w.at));
        let mut handoffs = BTreeMap::new();
        for w in &writes {
            if handoffs.contains_key(&w.id) {
                continue;
            }
            // from the archive: it holds the active entries too, and it logs no read
            let Ok(entry) = self.archived(&w.id).await else {
                continue;
            };
            if entry.front.entry_type == dm_core::EntryType::Handoff {
                let text = format!("{}\n{}", entry.body, entry.front.cites.join("\n"));
                handoffs.insert(w.id.clone(), text);
            }
        }
        let mut completed = BTreeSet::new();
        for d in self.domains() {
            let done = self.tasks(Some(&d), Status::Done).await.unwrap_or_default();
            completed.extend(done.into_iter().filter_map(|t| t.handoff));
        }
        let m = read_metrics::compute(&read_metrics::Inputs {
            reads: &all,
            writes: &writes,
            handoffs: &handoffs,
            completed: &completed,
        });
        Ok((m, handoffs))
    }

    /// The outbound rate in force of each family that has one.
    async fn rates(&self, l: &Ledger, domain: &str) -> Result<BTreeMap<String, u64>, Error> {
        let mut rates = BTreeMap::new();
        let families = self.grants.limits.get(domain).map(|s| &s.outbound);
        for family in families.into_iter().flat_map(BTreeMap::keys) {
            let limit = Limit::Outbound(family.clone());
            if let Some(cap) = self.cap(l, domain, &limit, domain).await? {
                rates.insert(family.clone(), cap.cap);
            }
        }
        Ok(rates)
    }

    /// What the rows of the table are computed from, for a caller that holds the ledger.
    async fn logs(
        &self,
        l: &Ledger,
        domain: &str,
        days: u32,
        retrieval: Counts,
    ) -> Result<Logs, Error> {
        let now = l.now();
        let since = now - Duration::days(i64::from(days));
        let granted = self.domains().into_iter();
        let tasks = self.domain_tasks(l, domain).await?;
        let ceiling = self.cap(l, domain, &Limit::Upkeep, domain).await?;
        Ok(Logs {
            domain: domain.to_string(),
            granted: granted
                .filter(|d| self.resolve_domain(Some(d)).is_ok())
                .collect(),
            since,
            now,
            cost: None,
            retrieval,
            reads: reads::all(&self.root).map_err(io(&reads::path(&self.root)))?,
            decisions: decisions(&self.root, since)?,
            tasks: tasks.into_iter().map(|t| (t.id.clone(), t)).collect(),
            moments: Moments::of(l, domain).await?,
            flow: self.flow_in(l, domain, days).await?,
            rules: self.enforced_rules(domain),
            rates: self.rates(l, domain).await?,
            ceiling: ceiling.map(|c| c.cap),
        })
    }

    /// The metric table of the paper for a domain, over the last `days` days: the 21
    /// rows of [`PAPER_METRICS`], in the paper's order. A row has a value from the logs,
    /// or it says why it is not measured. Only the tasks that the session may read are
    /// counted, and each counted is read. The library cannot read the meter of the
    /// egress, so the caller passes the cost of a completed task.
    ///
    /// # Errors
    ///
    /// An ungranted domain, a log or a price vector that cannot be read, or a ledger
    /// error.
    pub async fn metrics(
        &self,
        domain: Option<&str>,
        days: u32,
        cost_per_task: Option<Value>,
    ) -> Result<Metrics, Error> {
        let domain = self.resolve_domain(domain)?;
        let prices = self.prices()?.version;
        let now = self.ledger.lock().await.now();
        let since = now - Duration::days(i64::from(days));
        // before the ledger is held: the retrieval metrics list the completed tasks
        let (retrieval, _) = self.read_metrics_since(Some(since)).await?;
        let l = self.ledger.lock().await;
        let mut logs = self.logs(&l, &domain, days, retrieval.total).await?;
        logs.cost = cost_per_task;
        let named = PAPER_METRICS.iter().zip(ROWS);
        let rows = named.map(|(name, row)| Metric {
            name: (*name).to_string(),
            ..row(&logs)
        });
        Ok(Metrics {
            domain,
            window_days: days,
            prices,
            rows: rows.collect(),
        })
    }
}

#[cfg(test)]
mod tests;
