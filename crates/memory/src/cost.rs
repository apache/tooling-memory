//! What runs cost: the egress's metering priced by a table the owner keeps, per session
//! and per completed task, the paper's primary efficiency number. Prices live in
//! `prices.toml` under the root, in US dollars per million tokens, per model as the
//! provider reports it, with the four classes of token priced apart, since a harness
//! like Claude Code spends most of its tokens reading its cache. A call on the
//! harness's own login is plan quota, not metered spend, and is shown as tokens beside
//! the money, never priced. A model the table does not name is shown unpriced rather
//! than guessed at.
//!
//! Tokens are one resource class of the paper's price vector (§12); the host's own
//! are the others: each run's wall clock, the CPU time and peak memory of the process
//! tree it confined, and the launcher's own CPU, the egress among it, as the launcher
//! recorded them, and the time spent waiting on model calls, as the egress timed them.
//! `[resources]` in the same table prices CPU time and memory held; unpriced, they are
//! shown in their own units.

use dm_egress::telemetry::{Run, SessionModel, Telemetry, BILLING_PLAN};
use dm_server::Memory;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

/// The template `amem init` writes: every rate left for the owner to fill in.
pub const TEMPLATE: &str = r#"# What a million tokens cost, in US dollars, per model as the provider reports it in
# `amem usage`. A key is matched exactly, else as the longest prefix of the model's
# name, so "claude-opus-5" prices "claude-opus-5-5". Four classes, priced apart:
# input (not from the cache), output, cache reads and cache writes. A model not named
# here is shown unpriced. Calls on a harness's own login are plan quota, never priced.
#
# [models."claude-opus-5-5"]
# input = 0.0
# output = 0.0
# cache_read = 0.0
# cache_write = 0.0
#
# The host's own resources, per run of a harness: CPU time of the process tree and the
# launcher, in US dollars per CPU-hour, and peak resident memory held for the run's wall
# clock, in US dollars per GiB-hour. Left out, they are shown unpriced.
#
# [resources]
# cpu_hour = 0.0
# memory_gib_hour = 0.0
"#;

/// The rates of one model, in US dollars per million tokens.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct Rate {
    #[serde(default)]
    pub input: f64,
    #[serde(default)]
    pub output: f64,
    #[serde(default)]
    pub cache_read: f64,
    #[serde(default)]
    pub cache_write: f64,
}

/// What the host's resources cost, in US dollars.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct ResourceRates {
    /// Per hour of CPU time, user and system, the run's tree and the launcher's.
    #[serde(default)]
    pub cpu_hour: f64,
    /// Per GiB of peak resident memory held for an hour of the run's wall clock.
    #[serde(default)]
    pub memory_gib_hour: f64,
}

/// The table.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Prices {
    #[serde(default)]
    pub models: BTreeMap<String, Rate>,
    /// The host's resources; absent, they are not priced.
    #[serde(default)]
    pub resources: Option<ResourceRates>,
}

impl Prices {
    /// The table under `root`, or an empty one.
    ///
    /// # Errors
    ///
    /// A file that is there and does not parse.
    pub fn load(root: &Path) -> Result<Prices, Box<dyn std::error::Error>> {
        match std::fs::read_to_string(root.join("prices.toml")) {
            Ok(text) => Ok(toml::from_str(&text)?),
            Err(_) => Ok(Prices::default()),
        }
    }

    /// A model's rate: its exact name, else the longest key it starts with.
    #[must_use]
    pub fn rate(&self, model: &str) -> Option<&Rate> {
        self.models.get(model).or_else(|| {
            self.models
                .iter()
                .filter(|(k, _)| model.starts_with(k.as_str()))
                .max_by_key(|(k, _)| k.len())
                .map(|(_, r)| r)
        })
    }

    /// What usage cost, when its model is priced.
    #[must_use]
    pub fn cost(&self, u: &SessionModel) -> Option<f64> {
        let r = self.rate(&u.model)?;
        #[allow(clippy::cast_precision_loss)] // token counts, priced
        let usd = (u.input_tokens as f64 * r.input
            + u.output_tokens as f64 * r.output
            + u.cache_read_tokens as f64 * r.cache_read
            + u.cache_write_tokens as f64 * r.cache_write)
            / 1_000_000.0;
        Some(usd)
    }

    /// What a run's host resources cost, when they are priced.
    #[must_use]
    pub fn run_cost(&self, r: &Run) -> Option<f64> {
        let rates = self.resources.as_ref()?;
        #[allow(clippy::cast_precision_loss)] // milliseconds and KiB, priced
        let usd = (r.user_ms + r.sys_ms + r.launcher_cpu_ms) as f64 / 3_600_000.0 * rates.cpu_hour
            + r.max_rss_kib as f64 / 1_048_576.0 * r.wall_ms as f64 / 3_600_000.0
                * rates.memory_gib_hour;
        Some(usd)
    }
}

/// One session's cost.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Session {
    pub session: String,
    pub first: String,
    /// Metered spend on priced models, in US dollars.
    pub usd: f64,
    /// Tokens on metered models the table does not price, by model.
    pub unpriced: BTreeMap<String, u64>,
    /// Tokens on the harness's own login, by model: plan quota.
    pub plan: BTreeMap<String, u64>,
    pub calls: u64,
    /// The tasks this session completed: it wrote their closing handoff.
    pub tasks: Vec<String>,
    /// Runs of a harness under this session.
    pub runs: u64,
    /// Their wall clock, summed, in milliseconds.
    pub wall_ms: u64,
    /// CPU time of their process trees, user and system, in milliseconds.
    pub cpu_ms: u64,
    /// The launcher's own CPU time over them, the egress among it, in milliseconds.
    pub launcher_cpu_ms: u64,
    /// The largest resident set one process reached in any of them, in KiB.
    pub max_rss_kib: u64,
    /// Whether every run's CPU and memory were counted, not only its wall clock.
    pub measured: bool,
    /// Time spent waiting on the model calls the egress saw, in milliseconds.
    pub call_ms: u64,
    /// The share of `usd` that is the host's resources.
    pub resource_usd: f64,
}

/// One completed task's cost: its session's, shared with the other tasks it completed.
#[derive(Clone, Debug, Serialize)]
pub struct TaskCost {
    pub task: String,
    pub title: String,
    pub session: String,
    pub usd: f64,
    /// Whether some of its session's tokens were unpriced or plan quota, so the money
    /// is not all of it.
    pub partial: bool,
    /// Its share of the session's wall clock, in milliseconds.
    pub wall_ms: u64,
    /// Its share of the session's CPU time, the trees' and the launcher's, in
    /// milliseconds.
    pub cpu_ms: u64,
}

/// The report.
#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub sessions: Vec<Session>,
    pub tasks: Vec<TaskCost>,
    pub usd: f64,
    pub priced_models: Vec<String>,
}

fn tokens(u: &SessionModel) -> u64 {
    u.input_tokens + u.output_tokens + u.cache_read_tokens + u.cache_write_tokens
}

/// Price the usage, per session, and attribute each completed task to the session that
/// wrote its closing handoff.
#[must_use]
pub fn price(
    rows: &[SessionModel],
    runs: &[Run],
    prices: &Prices,
    completed: &[(String, String, String)],
) -> Report {
    let mut by: BTreeMap<String, Session> = BTreeMap::new();
    for u in rows {
        let s = by.entry(u.session.clone()).or_insert_with(|| Session {
            session: u.session.clone(),
            first: u.first.clone(),
            measured: true,
            ..Session::default()
        });
        s.calls += u.calls;
        s.call_ms += u.call_ms;
        if u.billing == BILLING_PLAN {
            *s.plan.entry(u.model.clone()).or_default() += tokens(u);
        } else if let Some(usd) = prices.cost(u) {
            s.usd += usd;
        } else {
            *s.unpriced.entry(u.model.clone()).or_default() += tokens(u);
        }
    }
    for r in runs {
        let s = by.entry(r.session.clone()).or_insert_with(|| Session {
            session: r.session.clone(),
            first: r.at.clone(),
            measured: true,
            ..Session::default()
        });
        s.runs += 1;
        s.wall_ms += r.wall_ms;
        s.cpu_ms += r.user_ms + r.sys_ms;
        s.launcher_cpu_ms += r.launcher_cpu_ms;
        s.max_rss_kib = s.max_rss_kib.max(r.max_rss_kib);
        s.measured &= r.measured;
        if let Some(usd) = prices.run_cost(r) {
            s.resource_usd += usd;
            s.usd += usd;
        }
    }
    for (task, _, session) in completed {
        if let Some(s) = by.get_mut(session) {
            s.tasks.push(task.clone());
        }
    }
    let mut tasks = Vec::new();
    for (task, title, session) in completed {
        if let Some(s) = by.get(session) {
            #[allow(clippy::cast_precision_loss)] // a count of tasks
            let share = s.usd / s.tasks.len().max(1) as f64;
            let n = u64::try_from(s.tasks.len().max(1)).unwrap_or(1);
            tasks.push(TaskCost {
                task: task.clone(),
                title: title.clone(),
                session: session.clone(),
                usd: share,
                partial: !s.unpriced.is_empty() || !s.plan.is_empty(),
                wall_ms: s.wall_ms / n,
                cpu_ms: (s.cpu_ms + s.launcher_cpu_ms) / n,
            });
        }
    }
    let mut sessions: Vec<Session> = by.into_values().collect();
    sessions.sort_by(|a, b| a.first.cmp(&b.first));
    Report {
        usd: sessions.iter().map(|s| s.usd).sum(),
        sessions,
        tasks,
        priced_models: prices.models.keys().cloned().collect(),
    }
}

/// The completed tasks of every granted domain, each with the session that wrote its
/// closing handoff, from the ledger and the write log.
///
/// # Errors
///
/// A ledger error.
pub async fn completed(
    m: &Memory,
) -> Result<Vec<(String, String, String)>, Box<dyn std::error::Error>> {
    let writes = dm_server::reads::all_writes(m.root()).unwrap_or_default();
    let session_of = |id: &str| {
        writes
            .iter()
            .find(|w| w.id == id)
            .map(|w| w.session.clone())
    };
    let mut out = Vec::new();
    for domain in m.domains() {
        for t in m.tasks(Some(&domain), dm_ledger::Status::Completed).await? {
            if let Some(s) = t.handoff.as_deref().and_then(session_of) {
                out.push((t.id.clone(), t.spec.title.clone(), s));
            }
        }
    }
    Ok(out)
}

/// `amem cost`.
///
/// # Errors
///
/// When the telemetry, the price table or the ledger cannot be read.
pub async fn report(root: &Path) -> Result<Report, Box<dyn std::error::Error>> {
    let m = Memory::open(root)?.without_read_log();
    let t = Telemetry::open(&root.join("state/telemetry.db"))?;
    Ok(price(
        &t.by_session_model()?,
        &t.runs()?,
        &Prices::load(root)?,
        &completed(&m).await?,
    ))
}

/// Milliseconds as whole seconds, for a table.
fn secs(ms: u64) -> String {
    format!("{:.1}", Duration::from_millis(ms).as_secs_f64())
}

/// The sessions' host resources, for those that ran a harness.
fn resources(s: &mut String, r: &Report) {
    let ran: Vec<&Session> = r.sessions.iter().filter(|x| x.runs > 0).collect();
    if ran.is_empty() {
        return;
    }
    s.push_str(
        "\nhost resources   runs   wall s  cpu s  launcher s  peak MiB  on calls s      usd\n",
    );
    for x in ran {
        s.push_str(&format!(
            "{:<16} {:>4} {:>8} {:>6} {:>11} {:>9} {:>11} {:>8.4}{}\n",
            x.session,
            x.runs,
            secs(x.wall_ms),
            secs(x.cpu_ms),
            secs(x.launcher_cpu_ms),
            x.max_rss_kib / 1024,
            secs(x.call_ms),
            x.resource_usd,
            if x.measured {
                ""
            } else {
                "  (wall clock only)"
            }
        ));
    }
}

/// The report as text.
#[must_use]
pub fn render(r: &Report) -> String {
    let mut s = String::new();
    if r.priced_models.is_empty() {
        s.push_str(
            "No prices: fill in prices.toml under the root to see money; tokens follow.\n\n",
        );
    }
    s.push_str("session          calls      usd   tasks  unpriced or plan tokens\n");
    for x in &r.sessions {
        let mut other: Vec<String> = x
            .unpriced
            .iter()
            .map(|(m, n)| format!("{m} {n} unpriced"))
            .collect();
        other.extend(x.plan.iter().map(|(m, n)| format!("{m} {n} on a plan")));
        s.push_str(&format!(
            "{:<16} {:>5} {:>8.4} {:>7}  {}\n",
            x.session,
            x.calls,
            x.usd,
            x.tasks.len(),
            other.join(", ")
        ));
    }
    resources(&mut s, r);
    if !r.tasks.is_empty() {
        s.push_str("\ncompleted task      usd   wall s  cpu s  title\n");
        for t in &r.tasks {
            s.push_str(&format!(
                "{:<18} {:>6.4}{} {:>7} {:>6}  {}\n",
                t.task,
                t.usd,
                if t.partial { "+" } else { " " },
                secs(t.wall_ms),
                secs(t.cpu_ms),
                t.title
            ));
        }
        s.push_str("  + some of that session's tokens were unpriced or plan quota\n");
    }
    s.push_str(&format!(
        "\ntotal metered spend on priced models and resources: ${:.4}\n",
        r.usd
    ));
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(session: &str, model: &str, billing: &str, input: u64, cache_read: u64) -> SessionModel {
        SessionModel {
            call_ms: 0,
            session: session.into(),
            destination: "d".into(),
            model: model.into(),
            billing: billing.into(),
            source: "egress".into(),
            calls: 1,
            input_tokens: input,
            output_tokens: 1000,
            cache_read_tokens: cache_read,
            cache_write_tokens: 0,
            first: "2026-10-11T00:00:00Z".into(),
        }
    }

    #[test]
    fn usage_is_priced_by_class_and_a_prefix_matches_a_family() {
        let p: Prices = toml::from_str(
            "[models.\"claude-opus-5\"]\ninput = 10.0\noutput = 50.0\ncache_read = 1.0\n",
        )
        .unwrap();
        let u = row("s", "claude-opus-5-5", "metered", 1_000_000, 2_000_000);
        // 1M input at 10, 1000 output at 50, 2M cache reads at 1
        assert!((p.cost(&u).unwrap() - (10.0 + 0.05 + 2.0)).abs() < 1e-9);
        assert!(
            p.cost(&row("s", "qwen3.8-27b", "metered", 1, 0)).is_none(),
            "not named, not guessed"
        );
    }

    #[test]
    fn a_task_costs_its_sessions_spend_shared_and_plan_quota_is_never_priced() {
        let p: Prices = toml::from_str("[models.m]\ninput = 1.0\noutput = 0.0\n").unwrap();
        let rows = vec![
            row("s1", "m", "metered", 2_000_000, 0),
            row("s2", "m", "plan", 9_000_000, 0),
            row("s3", "other", "metered", 5, 0),
        ];
        let done = vec![
            ("t1".to_string(), "A".to_string(), "s1".to_string()),
            ("t2".to_string(), "B".to_string(), "s1".to_string()),
            ("t3".to_string(), "C".to_string(), "s2".to_string()),
        ];
        let r = price(&rows, &[], &p, &done);
        assert!(
            (r.usd - 2.0).abs() < 1e-9,
            "only the metered, priced session costs money"
        );
        let t1 = r.tasks.iter().find(|t| t.task == "t1").unwrap();
        assert!(
            (t1.usd - 1.0).abs() < 1e-9,
            "two tasks share their session's spend"
        );
        let t3 = r.tasks.iter().find(|t| t.task == "t3").unwrap();
        assert!(
            t3.usd.abs() < 1e-9 && t3.partial,
            "a plan session is tokens, not money"
        );
        let s3 = r.sessions.iter().find(|s| s.session == "s3").unwrap();
        assert_eq!(s3.unpriced.get("other"), Some(&1005));
        assert!(render(&r).contains("on a plan"));
    }

    fn run(session: &str, measured: bool) -> Run {
        Run {
            session: session.into(),
            at: "2026-10-12T00:00:00Z".into(),
            harness: "claude".into(),
            runner: "namespaces".into(),
            task: None,
            exit: 0,
            wall_ms: 3_600_000,
            user_ms: 2_000_000,
            sys_ms: 1_000_000,
            max_rss_kib: 1_048_576,
            measured,
            launcher_cpu_ms: 600_000,
        }
    }

    #[test]
    fn a_runs_host_resources_are_priced_and_shared_like_its_tokens() {
        let p: Prices =
            toml::from_str("[resources]\ncpu_hour = 0.5\nmemory_gib_hour = 0.25\n").unwrap();
        // an hour of CPU in all, and a GiB held for an hour of wall clock
        let rows = vec![row("s1", "m", "metered", 10, 0)];
        let done = vec![
            ("t1".to_string(), "A".to_string(), "s1".to_string()),
            ("t2".to_string(), "B".to_string(), "s1".to_string()),
        ];
        let r = price(&rows, &[run("s1", true), run("s2", false)], &p, &done);
        let s1 = r.sessions.iter().find(|s| s.session == "s1").unwrap();
        assert!((s1.resource_usd - 0.75).abs() < 1e-9, "{s1:?}");
        assert!(
            (s1.usd - 0.75).abs() < 1e-9,
            "the model is unpriced: {s1:?}"
        );
        assert_eq!(
            (s1.runs, s1.cpu_ms, s1.launcher_cpu_ms),
            (1, 3_000_000, 600_000)
        );
        let t1 = r.tasks.iter().find(|t| t.task == "t1").unwrap();
        assert_eq!((t1.wall_ms, t1.cpu_ms), (1_800_000, 1_800_000), "half each");
        assert!((t1.usd - 0.375).abs() < 1e-9);
        let s2 = r.sessions.iter().find(|s| s.session == "s2").unwrap();
        assert!(!s2.measured, "a run with only its wall clock");
        let text = render(&r);
        assert!(text.contains("host resources"), "{text}");
        assert!(text.contains("(wall clock only)"), "{text}");
        assert!(text.contains("1024"), "a GiB, in MiB: {text}");
        // unpriced, the resources are shown and cost nothing
        let free = price(&rows, &[run("s1", true)], &Prices::default(), &done);
        assert!(free.usd.abs() < 1e-9);
        assert_eq!(free.sessions[0].wall_ms, 3_600_000);
    }
}
