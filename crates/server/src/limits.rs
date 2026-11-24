//! The five limits, each in its own unit: spend, the share of throughput, the outbound
//! rate, the enforced rules, and the share of upkeep.
//!
//! Each limit is held in one place. The egress holds spend, and the policy holds the
//! enforced rules. This module holds the three shares: a claim is refused at the share
//! of throughput, a tap is handed no more than the outbound rate leaves, and work that
//! the fleet filed is not admitted at the share of upkeep. Each of those is a line of
//! the decision log with the action `limit`.
//!
//! The grants file holds the value of a share. A person raises one for a time with an
//! approval on the ledger: the target is `limit:<name>`, the action is `raise:<value>`,
//! and the version is the digest of the value that the grants hold. A change of that
//! value thus voids the raise. Every window ends at the ledger's clock.

use crate::{fabric, measure, Error, Memory};
use dm_ledger::{ApprovalRecord, ApprovalSpec, Ledger, Task};
use dm_policy::decide::Class;
use dm_policy::DomainLimits;
use serde::Serialize;
use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;
use time::Duration;

/// The action of a decision-log line that one of the shares writes.
pub const LIMIT_ACTION: &str = "limit";

/// The prefix of the target of an approval that raises a limit.
const TARGET: &str = "limit:";

/// The prefix of the action of an approval that raises a limit.
const RAISE: &str = "raise:";

/// A limit that a person raises with an approval.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Limit {
    /// The share of throughput: the claims of one agent in a domain in 60 minutes.
    Throughput,
    /// The share of upkeep: the percentage of the completed tasks that may be upkeep.
    Upkeep,
    /// The outbound rate of a family of origins: the handoffs posted back in 24 hours.
    Outbound(String),
    /// The fleet's share of a landing queue: the changes of agents landed in 24 hours.
    Landing(String),
}

impl Limit {
    /// The target of the approval that raises this limit: `limit:<name>`.
    #[must_use]
    pub fn target(&self) -> String {
        format!("{TARGET}{self}")
    }
}

impl fmt::Display for Limit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Limit::Throughput => f.write_str("throughput"),
            Limit::Upkeep => f.write_str("upkeep"),
            Limit::Outbound(family) => write!(f, "outbound:{family}"),
            Limit::Landing(queue) => write!(f, "landing:{queue}"),
        }
    }
}

impl FromStr for Limit {
    type Err = Error;

    fn from_str(s: &str) -> Result<Limit, Error> {
        let named = |name: &str| (!name.is_empty()).then(|| name.to_string());
        let limit = match s.split_once(':') {
            None if s == "throughput" => Some(Limit::Throughput),
            None if s == "upkeep" => Some(Limit::Upkeep),
            Some(("outbound", family)) => named(family).map(Limit::Outbound),
            Some(("landing", queue)) => named(queue).map(Limit::Landing),
            _ => None,
        };
        limit.ok_or_else(|| {
            Error::Denied(format!(
                "{s} is no limit that an approval raises: throughput, upkeep, outbound:<family> or landing:<queue>"
            ))
        })
    }
}

/// A limit as it holds now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Cap {
    /// The value that the grants hold.
    pub configured: u64,
    /// The value in force: the configured one, or the highest raise that holds.
    pub cap: u64,
    /// The approval that raised it.
    pub raised_by: Option<String>,
}

/// One row of the report on the limits.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LimitRow {
    /// The limit: `spend`, `throughput`, `outbound`, `rules` or `upkeep`. A row of one
    /// outbound family is `outbound:<family>`, and a row of a landing queue is
    /// `landing:<queue>`.
    pub limit: String,
    /// The unit that the limit is counted in.
    pub unit: String,
    /// Whose limit it is: a destination, a principal or a domain.
    pub scope: String,
    /// The count now, where the ledger or the record holds it.
    pub now: Option<f64>,
    /// The limit in force; `None` when the grants set none.
    pub cap: Option<f64>,
    /// The value that the grants hold.
    pub configured: Option<f64>,
    /// The approval that raised the limit above the configured value.
    pub raised_by: Option<String>,
    /// The count now as a fraction of the limit in force.
    pub fraction: Option<f64>,
    /// What else a reader must know to read the row.
    pub note: Option<String>,
}

#[allow(clippy::cast_precision_loss)] // counts of tasks, rules, tokens and cents
fn number(n: u64) -> f64 {
    n as f64
}

impl LimitRow {
    fn new(limit: &str, unit: &str, scope: &str) -> LimitRow {
        LimitRow {
            limit: limit.to_string(),
            unit: unit.to_string(),
            scope: scope.to_string(),
            now: None,
            cap: None,
            configured: None,
            raised_by: None,
            fraction: None,
            note: None,
        }
    }

    /// The row of a limit that the grants do not set.
    fn not_set(limit: &str, unit: &str, scope: &str) -> LimitRow {
        LimitRow::new(limit, unit, scope).noted("not set")
    }

    fn noted(mut self, note: &str) -> LimitRow {
        self.note = Some(note.to_string());
        self
    }

    fn counted(mut self, now: f64) -> LimitRow {
        self.now = Some(now);
        self.fraction = self.cap.filter(|cap| *cap > 0.0).map(|cap| now / cap);
        self
    }

    fn capped(mut self, cap: Option<Cap>) -> LimitRow {
        match cap {
            Some(c) => {
                self.cap = Some(number(c.cap));
                self.configured = Some(number(c.configured));
                self.raised_by = c.raised_by;
                self
            }
            None => self.noted("not set"),
        }
    }
}

/// The version of an approval that raises `target` from `configured`.
fn version_of(target: &str, configured: u64) -> String {
    dm_core::digest(&format!("{target}={configured}"))
}

/// The claims that `who` made in `domain` in the 60 minutes before the ledger's clock.
async fn claims_in_hour(l: &Ledger, domain: &str, who: &str) -> Result<u64, Error> {
    let since = l.now() - Duration::minutes(60);
    let claims = l.moments(domain).await?;
    let made = claims
        .iter()
        .filter(|m| m.kind == "claimed" && m.holder.as_deref() == Some(who) && m.at > since);
    Ok(made.count() as u64)
}

/// The handoffs posted back in the 24 hours before the ledger's clock, by the family of
/// the task's origin.
pub(crate) async fn posted_in_day(
    l: &Ledger,
    domain: &str,
) -> Result<BTreeMap<String, u64>, Error> {
    let since = l.now() - Duration::hours(24);
    let mut posted = BTreeMap::new();
    for m in l.moments(domain).await? {
        if m.kind != "posted" || m.at <= since {
            continue;
        }
        if let Ok(task) = l.get(&m.task).await {
            *posted.entry(fabric::family(&task)).or_insert(0) += 1;
        }
    }
    Ok(posted)
}

/// What the outbound rates of a domain leave for one answer to a tap.
#[derive(Debug, Default)]
pub(crate) struct Outbound {
    /// By family: the rate in force, the posts of the last 24 hours added to what this
    /// answer hands out, and the tasks that wait.
    families: BTreeMap<String, (u64, u64, u64)>,
}

impl Outbound {
    /// Whether the rate of the task's family leaves room for it. A family with no rate
    /// always has room. A task with no room is counted as one that waits.
    pub(crate) fn has_room(&mut self, task: &Task) -> bool {
        match self.families.get_mut(&fabric::family(task)) {
            Some((cap, used, waits)) if *used >= *cap => {
                *waits += 1;
                false
            }
            _ => true,
        }
    }

    /// Count a task that the answer hands out.
    pub(crate) fn count(&mut self, task: &Task) {
        if let Some((_, used, _)) = self.families.get_mut(&fabric::family(task)) {
            *used += 1;
        }
    }

    /// One sentence for each family of which tasks wait.
    pub(crate) fn held(&self, domain: &str) -> Vec<String> {
        self.families
            .iter()
            .filter(|(_, (_, _, waits))| *waits > 0)
            .map(|(family, (cap, _, waits))| {
                format!(
                    "{waits} handoffs of {family} wait in {domain}: its outbound rate is {cap} posts in 24 hours. A person raises it: amem limits raise outbound:{family}"
                )
            })
            .collect()
    }
}

impl Memory {
    /// The value that the grants hold for a limit of a domain.
    fn configured(&self, domain: &str, limit: &Limit) -> Option<u64> {
        let shares = self.grants.limits.get(domain);
        match limit {
            Limit::Throughput => shares?.claims_per_hour.map(u64::from),
            Limit::Upkeep => shares?.upkeep_percent.map(u64::from),
            Limit::Outbound(family) => shares?.outbound.get(family).copied().map(u64::from),
            Limit::Landing(queue) => {
                let q = self
                    .grants
                    .landing
                    .get(queue)
                    .filter(|q| q.domain == domain);
                q?.per_day.map(u64::from)
            }
        }
    }

    /// Raise a limit of a domain to `to` for `life`, as an approval on the ledger. Only
    /// a person in person raises a limit. The approval is of the value that the grants
    /// hold now, so a change of the grants voids it. The share of throughput is raised
    /// for one principal, `scope`; the other limits are raised for the domain. A landing
    /// queue names its domain, so `domain` may be absent for it.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for a session that is no person in person, for a limit that the
    /// grants do not set, for a share of throughput with no principal, and for a share
    /// of upkeep above 100; an ungranted domain; a ledger error.
    pub async fn raise_limit(
        &self,
        domain: Option<&str>,
        limit: &Limit,
        to: u64,
        scope: Option<&str>,
        life: Duration,
    ) -> Result<ApprovalRecord, Error> {
        self.in_person("raises a limit")?;
        let of_queue = match limit {
            Limit::Landing(queue) => self.grants.landing.get(queue).map(|q| q.domain.as_str()),
            _ => None,
        };
        let domain = self.resolve_domain(domain.or(of_queue))?;
        let Some(configured) = self.configured(&domain, limit) else {
            return Err(Error::Denied(format!(
                "the grants set no limit {limit} for {domain}: there is nothing to raise"
            )));
        };
        if *limit == Limit::Upkeep && to > 100 {
            return Err(Error::Denied(
                "the share of upkeep is a percentage: 100 at most".into(),
            ));
        }
        let scope = match (limit, scope) {
            (Limit::Throughput, Some(principal)) => principal.to_string(),
            (Limit::Throughput, None) => {
                return Err(Error::Denied(
                    "name the principal whose share of throughput rises".into(),
                ))
            }
            _ => domain.clone(),
        };
        let target = limit.target();
        let mut l = self.ledger.lock().await;
        let spec = ApprovalSpec {
            principal: self.author.clone(),
            action: format!("{RAISE}{to}"),
            version: version_of(&target, configured),
            target,
            scope,
            expires: l.now() + life,
            domain,
        };
        Ok(l.record_approval(&spec).await?)
    }

    /// The limit of a domain as it holds now for `scope`: the value that the grants
    /// hold, or the highest raise that holds. A raise holds when a person gave it, it is
    /// not expired on the ledger's clock, it is of the value that the grants hold now,
    /// and it is for `scope`. A raise never lowers a limit. `None` when the grants set
    /// no such limit.
    pub(crate) async fn cap(
        &self,
        l: &Ledger,
        domain: &str,
        limit: &Limit,
        scope: &str,
    ) -> Result<Option<Cap>, Error> {
        let Some(configured) = self.configured(domain, limit) else {
            return Ok(None);
        };
        let target = limit.target();
        let version = version_of(&target, configured);
        let now = l.now();
        let owner = self.grants.owner.as_str();
        let mut cap = Cap {
            configured,
            cap: configured,
            raised_by: None,
        };
        for a in l.approvals(domain, Some(&target)).await? {
            let holds = !a.expired(now)
                && a.spec.version == version
                && a.spec.scope == scope
                && measure::is_person(&a.spec.principal, owner);
            let to = a.spec.action.strip_prefix(RAISE);
            let to = to.and_then(|n| n.parse::<u64>().ok());
            if let Some(to) = to.filter(|to| holds && *to > cap.cap) {
                cap.cap = to;
                cap.raised_by = Some(a.id);
            }
        }
        Ok(Some(cap))
    }

    /// Refuse a claim of an agent whose claims in the domain in the last 60 minutes are
    /// at its share of throughput. People are not limited.
    pub(crate) async fn within_throughput(&self, l: &Ledger, domain: &str) -> Result<(), Error> {
        if Class::of(&self.author, &self.grants.owner) != Class::Agent {
            return Ok(());
        }
        let Some(cap) = self
            .cap(l, domain, &Limit::Throughput, &self.author)
            .await?
        else {
            return Ok(());
        };
        let made = claims_in_hour(l, domain, &self.author).await?;
        if made < cap.cap {
            return Ok(());
        }
        let who = &self.author;
        Err(self.refusal(
            LIMIT_ACTION,
            domain,
            format!(
                "{who} made {made} claims in {domain} in the last hour; its share of throughput is {}. A person raises it: amem limits raise throughput --for {who}",
                cap.cap
            ),
        ))
    }

    /// What the outbound rates of a domain leave now, for one answer to a tap.
    pub(crate) async fn outbound(&self, l: &Ledger, domain: &str) -> Result<Outbound, Error> {
        let mut rate = Outbound::default();
        let Some(shares) = self.grants.limits.get(domain) else {
            return Ok(rate);
        };
        if shares.outbound.is_empty() {
            return Ok(rate);
        }
        let posted = posted_in_day(l, domain).await?;
        for family in shares.outbound.keys() {
            let limit = Limit::Outbound(family.clone());
            if let Some(cap) = self.cap(l, domain, &limit, domain).await? {
                let used = posted.get(family).copied().unwrap_or(0);
                rate.families.insert(family.clone(), (cap.cap, used, 0));
            }
        }
        Ok(rate)
    }

    /// Why the share of upkeep holds a task back from admission, when it does. It holds
    /// back work that the fleet filed: a task whose root is curation, or whose root an
    /// agent or the system filed. It does so when the upkeep among the tasks completed
    /// in the window is at the share. Curation keeps the floor of the rubric: a curation
    /// task is not held back while curation is under that floor. With nothing completed
    /// in the window there is no share, and nothing is held back.
    pub(crate) async fn upkeep_hold(
        &self,
        l: &Ledger,
        task: &Task,
    ) -> Result<Option<String>, Error> {
        let domain = task.spec.domain.as_str();
        let Some(cap) = self.cap(l, domain, &Limit::Upkeep, domain).await? else {
            return Ok(None);
        };
        let root = measure::root_of(l, task).await;
        let curation = fabric::maintenance(&root);
        if !curation && !measure::fleet_filed(&root, &self.grants.owner) {
            return Ok(None);
        }
        let days = self
            .grants
            .limits
            .get(domain)
            .map_or(DomainLimits::WINDOW_DAYS, DomainLimits::window);
        let flow = self.flow_all(l, domain, days).await?;
        if flow.done == 0 || (flow.upkeep as u64) * 100 < cap.cap * flow.done as u64 {
            return Ok(None);
        }
        let floor = self.rubric().0.maintenance_floor;
        if curation
            && number((flow.upkeep - flow.self_filed) as u64) < floor * number(flow.done as u64)
        {
            return Ok(None);
        }
        Ok(Some(format!(
            "{} is not admitted: {} of the {} tasks completed in {domain} in {days} days are upkeep, and the share of upkeep is {} percent. A person admits it, or raises the share: amem limits raise upkeep",
            task.id, flow.upkeep, flow.done, cap.cap
        )))
    }

    /// The five limits of a domain as they stand, in the paper's order: spend, the share
    /// of throughput, the outbound rate, the enforced rules, and the share of upkeep.
    /// Every limit has a row. A limit that the grants do not set has no cap and the note
    /// `not set`. The counts are the ones that the limits are held against: they are of
    /// the whole domain. The session reads no task for them, except for the row of a
    /// landing queue, which reads as [`Memory::landing`] does.
    ///
    /// # Errors
    ///
    /// An ungranted domain, or a ledger error.
    pub async fn limits(&self, domain: Option<&str>) -> Result<Vec<LimitRow>, Error> {
        let domain = self.resolve_domain(domain)?;
        let l = self.ledger.lock().await;
        let mut rows = self.spend_rows();
        rows.push(self.throughput_row(&l, &domain).await?);
        rows.extend(self.outbound_rows(&l, &domain).await?);
        rows.push(self.rules_row(&domain));
        rows.push(self.upkeep_row(&l, &domain).await?);
        Ok(rows)
    }

    /// The rows of spend: one for each value that a destination has. The egress counts
    /// spend, so no row has a count.
    fn spend_rows(&self) -> Vec<LimitRow> {
        const METER: &str = "the egress meters spend; amem cost and the refusal of a call show it";
        let mut rows = Vec::new();
        for (dest, s) in &self.grants.spend {
            let values = [
                ("tokens", "session", s.session_tokens),
                ("tokens", "day", s.day_tokens),
                ("US cents", "session", s.session_usd_cents),
                ("US cents", "day", s.day_usd_cents),
            ];
            for (unit, window, value) in values {
                if let Some(value) = value {
                    let mut row = LimitRow::new("spend", unit, &format!("{dest}, one {window}"));
                    row.cap = Some(number(value));
                    row.configured = row.cap;
                    rows.push(row.noted(METER));
                }
            }
        }
        if rows.is_empty() {
            rows.push(LimitRow::not_set("spend", "US cents", ""));
        }
        rows
    }

    async fn throughput_row(&self, l: &Ledger, domain: &str) -> Result<LimitRow, Error> {
        let who = self.author.as_str();
        let cap = self.cap(l, domain, &Limit::Throughput, who).await?;
        let row = LimitRow::new("throughput", "claims per hour", who).capped(cap);
        let row = row.counted(number(claims_in_hour(l, domain, who).await?));
        let agent = Class::of(who, &self.grants.owner) == Class::Agent;
        if agent || row.cap.is_none() {
            Ok(row)
        } else {
            Ok(row.noted("the limit is on agents; a person is not limited"))
        }
    }

    /// The rows of the outbound rate: one for each family with a rate, and one for each
    /// landing queue of the domain.
    async fn outbound_rows(&self, l: &Ledger, domain: &str) -> Result<Vec<LimitRow>, Error> {
        let mut rows = Vec::new();
        let posted = posted_in_day(l, domain).await?;
        let families = self.grants.limits.get(domain).map(|s| &s.outbound);
        for family in families.into_iter().flat_map(BTreeMap::keys) {
            let limit = Limit::Outbound(family.clone());
            let cap = self.cap(l, domain, &limit, domain).await?;
            let row = LimitRow::new(&limit.to_string(), "posts per day", domain).capped(cap);
            rows.push(row.counted(number(posted.get(family).copied().unwrap_or(0))));
        }
        if rows.is_empty() {
            rows.push(LimitRow::not_set("outbound", "posts per day", domain));
        }
        for (queue, q) in &self.grants.landing {
            if q.domain != domain {
                continue;
            }
            let limit = Limit::Landing(queue.clone());
            let cap = self.cap(l, domain, &limit, domain).await?;
            let landed = self.landing_in(l, queue).await?.fleet_landed_last_day;
            let row = LimitRow::new(&limit.to_string(), "changes per day", domain).capped(cap);
            rows.push(row.counted(number(landed as u64)));
        }
        Ok(rows)
    }

    /// The rules of a domain that count against the ceiling and are enforced now.
    pub(crate) fn enforced_rules(&self, domain: &str) -> usize {
        let p = self.policy();
        let owner = self.grants.owner.as_str();
        let signed = |by: &Option<String>| {
            by.as_deref()
                .is_some_and(|by| measure::is_person(by, owner))
        };
        let enforced = p.entries().iter().filter(|e| {
            e.domain == domain
                && !e.meta.is_exception()
                && !e.meta.is_core()
                && !signed(&e.meta.signed_off_by)
                && p.state_of(e) == dm_policy::record::Status::Enforced
        });
        enforced.count()
    }

    /// The row of the enforced rules: the rules of the domain that count against the
    /// ceiling and are enforced now. The policy holds this limit.
    fn rules_row(&self, domain: &str) -> LimitRow {
        let mut row = LimitRow::new("rules", "rules", domain);
        match self.grants.gates.max_enforced_rules {
            Some(ceiling) => {
                row.cap = Some(number(ceiling as u64));
                row.configured = row.cap;
            }
            None => row = row.noted("not set"),
        }
        row.counted(number(self.enforced_rules(domain) as u64))
    }

    async fn upkeep_row(&self, l: &Ledger, domain: &str) -> Result<LimitRow, Error> {
        let cap = self.cap(l, domain, &Limit::Upkeep, domain).await?;
        let days = self
            .grants
            .limits
            .get(domain)
            .map_or(DomainLimits::WINDOW_DAYS, DomainLimits::window);
        let flow = self.flow_all(l, domain, days).await?;
        let row = LimitRow::new("upkeep", "percent of completed tasks", domain).capped(cap);
        Ok(match flow.upkeep_share {
            Some(share) => row.counted(share * 100.0),
            None => row,
        })
    }
}

#[cfg(test)]
mod tests;
