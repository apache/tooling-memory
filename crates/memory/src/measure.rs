//! `amem metrics`, `amem limits` and `amem landing`: the metric table of the paper, the
//! five limits, and the landing queue. Every rule is the library's. This file reads the
//! meter of the egress, which the library cannot read, and prints.

use crate::{cost, json, Command, Fallible};
use clap::Subcommand;
use dm_egress::Telemetry;
use dm_ledger::Status;
use dm_server::limits::{Limit, LimitRow, Metered};
use dm_server::measure::{Metric, Metrics, Target};
use dm_server::Memory;
use std::collections::{BTreeMap, BTreeSet};
use time::{Duration, OffsetDateTime};

/// What `amem limits` does beside the report.
#[derive(Subcommand)]
pub enum LimitsOp {
    /// Raise a limit for a time. It is your approval: only a person raises a limit. A
    /// change of the configured value in the grants ends the raise.
    Raise {
        /// `throughput`, `upkeep`, `outbound:<family>` or `landing:<queue>`.
        limit: String,
        /// The new value, in the unit of the limit.
        #[arg(long)]
        to: u64,
        #[arg(long)]
        domain: Option<String>,
        /// The principal whose share of throughput rises. The other limits are of the
        /// domain.
        #[arg(long = "for")]
        principal: Option<String>,
        /// How long the raise holds, in hours.
        #[arg(long, default_value_t = 24)]
        hours: i64,
    },
}

/// What `amem landing` does.
#[derive(Subcommand)]
pub enum LandingOp {
    /// A landing queue as it stands: what waits, in landing order, and what landed.
    Show { queue: String },
    /// Land the change at the head of a landing queue: its task closes with the handoff
    /// that its holder queued. Only a person lands a change.
    Land { queue: String },
}

/// The cost of a completed task of `domain`, from the report of `amem cost`.
async fn cost_per_task(
    m: &Memory,
    domain: Option<&str>,
) -> Result<Option<serde_json::Value>, Box<dyn std::error::Error>> {
    let domain = m.resolve_domain(domain)?;
    let done = m.tasks(Some(&domain), Status::Done).await?;
    let ids: BTreeSet<String> = done.into_iter().map(|t| t.id).collect();
    let report = cost::report(m.root()).await?;
    Ok(cost::per_task(&report, |task| ids.contains(task)))
}

/// What the meter of the egress holds for the last 24 hours, for each destination with
/// a spend limit: the tokens that a limit in tokens counts, and the metered tokens as
/// the price vector prices them.
fn metered(m: &Memory) -> Result<BTreeMap<String, Metered>, Box<dyn std::error::Error>> {
    let mut by = BTreeMap::new();
    let spend = &m.grants().spend;
    if spend.is_empty() {
        return Ok(by);
    }
    let meter = Telemetry::open(&m.root().join("state/telemetry.db"))?;
    let prices = m.prices()?;
    let since = Some(OffsetDateTime::now_utc() - Duration::days(1));
    for destination in spend.keys() {
        let mut unpriced = BTreeSet::new();
        let by_model = meter.spent_by_model(destination, None, since)?;
        let read = Metered {
            tokens: meter.spent(destination, None, since)?,
            cents: prices.cents(&by_model, &mut unpriced),
            unpriced: unpriced.into_iter().collect(),
        };
        by.insert(destination.clone(), read);
    }
    Ok(by)
}

/// One row of the metric table as a line: the name, the value or why there is none, and
/// the log that the value comes from.
fn metric_line(row: &Metric) -> String {
    let why = row.why_not.as_deref().unwrap_or("no log answers it");
    if !row.measured {
        return format!("  {}: not measured: {why}\n", row.name);
    }
    let missing = match &row.why_not {
        Some(why) => format!("; not measured: {why}"),
        None => String::new(),
    };
    format!("  {}: {} [{}]{missing}\n", row.name, row.value, row.from)
}

/// One published target as a line: the target, and what is measured against it.
fn target_line(t: &Target) -> String {
    let got = match (&t.measured, t.met) {
        (Some(v), Some(true)) => format!("measured {v}, met"),
        (Some(v), _) => format!("measured {v}, not met"),
        (None, _) => format!(
            "not measured: {}",
            t.why_not.as_deref().unwrap_or("no log answers it")
        ),
    };
    format!("  {}: target {}; {got}\n", t.name, t.target)
}

/// The metric table and the targets as text.
fn render_metrics(table: &Metrics, targets: &[Target]) -> String {
    let mut s = format!(
        "metrics of {}, the last {} days, price vector {}\n",
        table.domain, table.window_days, table.prices
    );
    s.extend(table.rows.iter().map(metric_line));
    s.push_str("targets\n");
    s.extend(targets.iter().map(target_line));
    s
}

/// A count or a limit: whole when it is whole.
fn number(n: Option<f64>) -> String {
    match n {
        Some(n) if n.fract() == 0.0 => format!("{n:.0}"),
        Some(n) => format!("{n:.1}"),
        None => "-".into(),
    }
}

/// One limit as a line: the count now, the limit in force, the unit, and whose it is.
fn limit_line(row: &LimitRow) -> String {
    let mut s = format!(
        "  {:<16} {} of {} {}",
        row.limit,
        number(row.now),
        number(row.cap),
        row.unit
    );
    if !row.scope.is_empty() {
        s.push_str(&format!(", {}", row.scope));
    }
    if let Some(by) = &row.raised_by {
        let from = number(row.configured);
        s.push_str(&format!("; raised from {from} by the approval {by}"));
    }
    if let Some(note) = &row.note {
        s.push_str(&format!("; {note}"));
    }
    s.push('\n');
    s
}

async fn metrics_cmd(m: &Memory, domain: Option<&str>, days: u32, as_json: bool) -> Fallible {
    let cost = cost_per_task(m, domain).await?;
    let table = m.metrics(domain, days, cost).await?;
    let targets = m.targets(days).await?;
    if as_json {
        let mut v = serde_json::to_value(&table)?;
        v["targets"] = serde_json::to_value(&targets)?;
        return json(&v);
    }
    print!("{}", render_metrics(&table, &targets));
    Ok(())
}

/// `amem metrics`, `amem limits` and `amem landing`.
pub async fn cmd(m: &Memory, command: Command) -> Fallible {
    match command {
        Command::Metrics {
            domain,
            days,
            json: as_json,
        } => metrics_cmd(m, domain.as_deref(), days, as_json).await,
        Command::Limits {
            op: None,
            domain,
            json: as_json,
        } => {
            let rows = m.limits_metered(domain.as_deref(), &metered(m)?).await?;
            if as_json {
                return json(&rows);
            }
            print!("{}", rows.iter().map(limit_line).collect::<String>());
            Ok(())
        }
        Command::Limits {
            op:
                Some(LimitsOp::Raise {
                    limit,
                    to,
                    domain,
                    principal,
                    hours,
                }),
            domain: of,
            ..
        } => {
            let limit: Limit = limit.parse()?;
            let domain = domain.or(of);
            let life = Duration::hours(hours);
            let raise = m.raise_limit(domain.as_deref(), &limit, to, principal.as_deref(), life);
            json(&raise.await?)
        }
        Command::Landing {
            op: LandingOp::Show { queue },
        } => json(&m.landing(&queue).await?),
        Command::Landing {
            op: LandingOp::Land { queue },
        } => json(&m.land(&queue).await?),
        _ => unreachable!("routed by maintain"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn metric(measured: bool, why_not: Option<&str>) -> Metric {
        Metric {
            name: "Induced work".into(),
            measured,
            value: if measured {
                json!({"open": 2})
            } else {
                json!(null)
            },
            from: if measured {
                "ledger".into()
            } else {
                String::new()
            },
            why_not: why_not.map(str::to_string),
        }
    }

    #[test]
    fn a_row_prints_its_value_and_its_log_or_why_it_is_not_measured() {
        let line = metric_line(&metric(true, None));
        assert_eq!(line, "  Induced work: {\"open\":2} [ledger]\n");
        let part = metric_line(&metric(true, Some("no meter")));
        assert_eq!(
            part,
            "  Induced work: {\"open\":2} [ledger]; not measured: no meter\n"
        );
        let none = metric_line(&metric(false, Some("it needs a model")));
        assert_eq!(none, "  Induced work: not measured: it needs a model\n");
    }

    #[test]
    fn a_target_prints_what_is_measured_against_it_or_why_nothing_is() {
        let mut t = Target {
            name: "Search latency, median".into(),
            target: "100 ms".into(),
            measured: Some("3 ms".into()),
            met: Some(true),
            why_not: None,
        };
        let met = "  Search latency, median: target 100 ms; measured 3 ms, met\n";
        assert_eq!(target_line(&t), met);
        t.met = Some(false);
        assert!(target_line(&t).ends_with("measured 3 ms, not met\n"));
        t.measured = None;
        t.met = None;
        t.why_not = Some("no search in the window".into());
        let line = target_line(&t);
        assert!(line.ends_with("target 100 ms; not measured: no search in the window\n"));
    }

    #[test]
    fn a_limit_prints_its_count_its_value_in_force_its_unit_and_its_raise() {
        let v = json!({
            "limit": "upkeep", "unit": "percent of completed tasks", "scope": "dom-me",
            "now": 33.333, "cap": 60.0, "configured": 50.0, "raised_by": "ap-1",
            "fraction": 0.55, "note": null,
        });
        let row = |v: &serde_json::Value| LimitRow {
            limit: v["limit"].as_str().unwrap().into(),
            unit: v["unit"].as_str().unwrap().into(),
            scope: v["scope"].as_str().unwrap().into(),
            now: v["now"].as_f64(),
            cap: v["cap"].as_f64(),
            configured: v["configured"].as_f64(),
            raised_by: v["raised_by"].as_str().map(str::to_string),
            fraction: v["fraction"].as_f64(),
            note: v["note"].as_str().map(str::to_string),
        };
        let line = limit_line(&row(&v));
        assert_eq!(
            line,
            "  upkeep           33.3 of 60 percent of completed tasks, dom-me; raised from 50 by the approval ap-1\n"
        );
        let unset = json!({"limit": "spend", "unit": "US cents", "scope": "", "note": "not set"});
        assert_eq!(
            limit_line(&row(&unset)),
            "  spend            - of - US cents; not set\n"
        );
    }
}
