//! The evaluation: a plan fixed before the runs, arms paired across seeds, the replay
//! traps, and a scorecard kept in the platform's domain (the paper's section 11).
//!
//! A [`Plan`] is an evaluation set file with a `[plan]` table: the tasks, the arms, the
//! baseline arm, the number of seeds, the margin and the judge. The plan has a digest.
//! A launcher starts each run with [`Memory::start_evaluation`], which gives the session
//! the settings of its arm at one snapshot. The head of the session's tape then names
//! the digest of the plan, the task, the seed and the arm ([`RunOf`]). The memory server
//! writes that head, so the margin is shown to be fixed before the run, and a run that
//! no session made has no tape.
//!
//! [`Memory::score`] reads the facts of each run from its tape: the task, the seed, the
//! arm, the harness, the model, the start time and the snapshot. It takes three things
//! from the caller and trusts them: whether the run succeeded, what the run cost in
//! tokens, and the name of the judge that decided the success. The runner is the party
//! that has the answer of the harness, so nothing in the record can check them.
//!
//! A pair is one task and one seed with a run in an arm and a run in the baseline arm.
//! [`compare`] takes the difference of each pair, arm minus baseline, and gives the mean
//! with an interval from the normal approximation. The margin and the accepted cost of
//! the plan turn the two intervals into a [`Finding`]. A finding is not a verdict. The
//! five replay traps are checked on every scorecard, and an open trap makes each verdict
//! [`Verdict::Inconclusive`]. A drift window of the runs' model that overlaps the runs
//! makes each verdict [`Verdict::Quarantined`].
//!
//! A scorecard is kept as a `decision` entry in the platform's domain, with the
//! scorecard as JSON in a fenced block `platform scorecard <plan>`. The launcher loads
//! no scorecard: it is a record. [`Memory::scorecards`] serves the kept scorecards under
//! the read filter of the session, as `list` and `get` serve entries.

use crate::arms::Arm;
use crate::drift_windows::Window;
use crate::platform::artifacts_in;
use crate::tape::canonical;
use crate::{Error, Memory, WriteRequest, Written};
use dm_core::EntryType;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// The kind of the fenced block that holds a scorecard.
pub const KIND: &str = "scorecard";

/// How the title of a scorecard's entry starts. [`Memory::scorecards`] reads the body of
/// an entry only when its title starts so.
pub const TITLE: &str = "Scorecard ";

/// The most decisions [`Memory::scorecards`] lists to find the scorecards.
const LISTED: usize = 1000;

/// The names of the five replay traps, in the paper's order.
pub const TRAPS: [&str; 5] = [
    "survivorship",
    "leakage",
    "noise",
    "judge-drift",
    "distribution-shift",
];

/// Whether a task is one that production passed or one that it failed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stratum {
    /// A task with no recorded failure.
    #[default]
    Passed,
    /// A task that failed in production: the survivorship trap needs one.
    Failed,
}

/// Which of the two task sets a task is in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskSet {
    /// The set that follows production.
    #[default]
    Rolling,
    /// The set that does not change: the distribution-shift trap needs one of each.
    Frozen,
}

/// One task of a plan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanTask {
    /// Its name in the plan.
    pub id: String,
    /// What the harness is asked, after the preamble.
    pub prompt: String,
    /// The regular expression that the answer must match.
    pub expect: String,
    /// Whether production passed it or failed it.
    #[serde(default)]
    pub stratum: Stratum,
    /// The task set it is in.
    #[serde(default)]
    pub set: TaskSet,
}

/// The plan of an evaluation: everything that is fixed before the first run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    /// The plan's name: one word, the name of its scorecards.
    pub name: String,
    /// The domain the runs read, and where their tapes are kept.
    pub domain: String,
    /// The arm every other arm is compared with.
    pub baseline: Arm,
    /// The arms, the baseline among them.
    pub arms: Vec<Arm>,
    /// How many times each task runs in each arm. A seed is a number below this.
    pub seeds: u32,
    /// The margin: how much lower the success rate of an arm may be, as a fraction, and
    /// the arm is still no worse than the baseline.
    pub margin: f64,
    /// The accepted cost: the most an arm that is better may cost, as a factor of the
    /// cost of the baseline.
    pub accepted_cost: f64,
    /// The name of the judge that decides the success of a run.
    pub judge: String,
    /// The harness every run uses, when the plan fixes one.
    pub harness: Option<String>,
    /// The model every run uses, when the plan fixes one.
    pub model: Option<String>,
    /// The text in front of the prompt of every task.
    pub preamble: String,
    /// The tasks.
    pub tasks: Vec<PlanTask>,
}

/// The evaluation set file, as TOML holds it. A key that is not named here is ignored.
#[derive(Deserialize)]
struct File {
    #[serde(default)]
    preamble: String,
    plan: Option<Table>,
    #[serde(default)]
    task: Vec<PlanTask>,
}

/// The `[plan]` table.
#[derive(Deserialize)]
struct Table {
    name: String,
    domain: String,
    baseline: String,
    arms: Vec<String>,
    seeds: u32,
    margin: Option<f64>,
    accepted_cost: Option<f64>,
    judge: String,
    harness: Option<String>,
    model: Option<String>,
}

fn refuse(why: &str) -> Error {
    Error::Denied(format!("the plan is refused: {why}"))
}

fn arm_of(name: &str) -> Result<Arm, Error> {
    name.parse::<Arm>().map_err(|e| refuse(&e))
}

/// The arms of a plan, each named once.
fn arms_of(names: &[String]) -> Result<Vec<Arm>, Error> {
    let mut arms = Vec::new();
    for name in names {
        let arm = arm_of(name)?;
        if arms.contains(&arm) {
            return Err(refuse(&format!("it names the arm {arm} twice")));
        }
        arms.push(arm);
    }
    Ok(arms)
}

/// The tasks of a plan: at least one, each id once.
fn tasks_of(tasks: Vec<PlanTask>) -> Result<Vec<PlanTask>, Error> {
    if tasks.is_empty() {
        return Err(refuse("it has no task"));
    }
    let mut ids = BTreeSet::new();
    for t in &tasks {
        if !ids.insert(t.id.as_str()) {
            return Err(refuse(&format!("it names the task {} twice", t.id)));
        }
    }
    Ok(tasks)
}

/// The numbers of a `[plan]` table: the margin and the accepted cost.
fn numbers_of(table: &Table) -> Result<(f64, f64), Error> {
    let Some(margin) = table.margin else {
        return Err(refuse("it names no margin"));
    };
    if !(0.0..=1.0).contains(&margin) {
        return Err(refuse("the margin is a fraction from 0 to 1"));
    }
    let accepted_cost = table.accepted_cost.unwrap_or(1.0);
    if !accepted_cost.is_finite() || accepted_cost <= 0.0 {
        return Err(refuse("the accepted cost is a factor above 0"));
    }
    if table.seeds < 2 {
        return Err(refuse(
            "it needs two seeds or more: one run of a task says nothing about noise",
        ));
    }
    Ok((margin, accepted_cost))
}

impl Plan {
    /// Read a plan from the text of an evaluation set file.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`], with the reason, for text that is not TOML, that has no
    /// `[plan]` table, no margin or a margin outside 0 to 1, fewer than two seeds, an
    /// arm that is no arm, an arm named twice, a baseline that is not among the arms,
    /// no task, a task id named twice, or a judge that is the model.
    pub fn parse(text: &str) -> Result<Plan, Error> {
        let file: File =
            toml::from_str(text).map_err(|e| refuse(&format!("it does not read: {e}")))?;
        let Some(table) = file.plan else {
            return Err(refuse("it has no [plan] table"));
        };
        let (margin, accepted_cost) = numbers_of(&table)?;
        let arms = arms_of(&table.arms)?;
        let baseline = arm_of(&table.baseline)?;
        if !arms.contains(&baseline) {
            return Err(refuse(&format!(
                "the baseline {baseline} is not among its arms"
            )));
        }
        if table.model.as_deref() == Some(table.judge.as_str()) {
            return Err(refuse(
                "the judge is the model of the runs: a model does not judge its own answers",
            ));
        }
        Ok(Plan {
            name: table.name,
            domain: table.domain,
            baseline,
            arms,
            seeds: table.seeds,
            margin,
            accepted_cost,
            judge: table.judge,
            harness: table.harness,
            model: table.model,
            preamble: file.preamble,
            tasks: tasks_of(file.task)?,
        })
    }

    /// The digest of the plan: of the plan as canonical JSON, keys sorted. A comment,
    /// the order of the keys and an ignored key do not change it.
    #[must_use]
    pub fn digest(&self) -> String {
        let plan = serde_json::to_value(self).unwrap_or(Value::Null);
        dm_core::digest(&canonical(&plan))
    }

    /// The task named `id`.
    #[must_use]
    pub fn task(&self, id: &str) -> Option<&PlanTask> {
        self.tasks.iter().find(|t| t.id == id)
    }

    /// The prompt of a run of the task `id`: the preamble, then the task's prompt.
    #[must_use]
    pub fn prompt_of(&self, id: &str) -> Option<String> {
        self.task(id)
            .map(|t| format!("{}{}", self.preamble, t.prompt))
    }

    /// Whether a run of the task `task` at the seed `seed` in the arm `arm` is a run
    /// of this plan.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for a task the plan does not hold, an arm the plan does not
    /// name, and a seed at or above the plan's seeds.
    pub fn holds(&self, task: &str, seed: u32, arm: Arm) -> Result<(), Error> {
        if self.task(task).is_none() {
            return Err(Error::Denied(format!(
                "the plan {} has no task {task}",
                self.name
            )));
        }
        if !self.arms.contains(&arm) {
            return Err(Error::Denied(format!(
                "the plan {} does not name the arm {arm}",
                self.name
            )));
        }
        if seed >= self.seeds {
            return Err(Error::Denied(format!(
                "the plan {} has {} seeds, from 0: {seed} is not one",
                self.name, self.seeds
            )));
        }
        Ok(())
    }

    /// The digest of the frozen task set: of the frozen tasks, in the plan's order.
    #[must_use]
    pub fn frozen_digest(&self) -> String {
        let frozen: Vec<&PlanTask> = self
            .tasks
            .iter()
            .filter(|t| t.set == TaskSet::Frozen)
            .collect();
        dm_core::digest(&canonical(&json!(frozen)))
    }
}

/// Which run of which plan a session is. The head of the session's tape holds it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunOf {
    /// The digest of the plan.
    pub plan: String,
    /// The task.
    pub task: String,
    /// The seed: the number of the repeated run.
    pub seed: u32,
    /// The arm.
    pub arm: Arm,
}

/// What the runner says of one run. The scorer trusts these three values.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outcome {
    /// The session of the run: its tape gives every other fact.
    pub session: String,
    /// Whether the judge accepted the answer.
    pub success: bool,
    /// What the run cost, in tokens, as the harness counted them.
    pub cost: u64,
    /// The name of the judge.
    pub judge: String,
}

/// One run, as [`compare`] reads it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Run {
    /// The task.
    pub task: String,
    /// The seed.
    pub seed: u32,
    /// The arm.
    pub arm: Arm,
    /// Whether the run succeeded.
    pub success: bool,
    /// What it cost, in tokens.
    pub cost: u64,
}

/// A mean of paired differences with its interval.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Interval {
    /// The mean.
    pub mean: f64,
    /// The low end.
    pub low: f64,
    /// The high end.
    pub high: f64,
}

/// What the numbers alone say of an arm against the baseline.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Finding {
    /// No worse than the baseline and cheaper, or better at an accepted cost.
    Pays,
    /// Worse than the baseline, or no better and dearer.
    Fails,
    /// The intervals do not decide.
    Unclear,
}

/// What a scorecard says of an arm.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Verdict {
    /// The arm pays.
    Pays,
    /// The arm fails.
    Fails,
    /// The intervals do not decide.
    Unclear,
    /// A replay trap is open, so the finding is not a verdict.
    Inconclusive {
        /// The traps that are open.
        traps: Vec<String>,
    },
    /// The runs spanned a drift window of their model.
    Quarantined {
        /// The destination of the window.
        destination: String,
        /// The model of the window.
        model: String,
    },
}

/// One replay trap on a scorecard.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Trap {
    /// Its name.
    pub name: String,
    /// Whether the evaluation is safe from it.
    pub held: bool,
    /// Why, in one sentence.
    pub why: String,
}

/// The score of one arm.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ArmScore {
    /// The arm.
    pub arm: Arm,
    /// Whether it is the baseline.
    pub baseline: bool,
    /// How many runs it has.
    pub runs: usize,
    /// How many of them succeeded.
    pub passed: usize,
    /// The mean cost of its runs, in tokens.
    pub mean_cost: f64,
    /// How many of its runs have a partner in the baseline.
    pub pairs: usize,
    /// How many seeds its pairs come from; for the baseline, its runs.
    pub seeds: usize,
    /// The difference in success, arm minus baseline, over the pairs.
    pub success: Interval,
    /// The difference in cost, arm minus baseline, over the pairs.
    pub cost: Interval,
    /// What the numbers alone say; none for the baseline.
    pub finding: Option<Finding>,
    /// What the scorecard says; none for the baseline.
    pub verdict: Option<Verdict>,
}

/// The result of an evaluation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Scorecard {
    /// The plan's name.
    pub plan: String,
    /// The digest of the plan that every run started under.
    pub plan_digest: String,
    /// The digest of the plan's frozen task set.
    pub frozen_digest: String,
    /// The domain the runs read.
    pub domain: String,
    /// The harness of the runs.
    pub harness: String,
    /// The model of the runs, when the launches named one.
    pub model: Option<String>,
    /// The judge of the plan.
    pub judge: String,
    /// The baseline arm.
    pub baseline: Arm,
    /// The margin of the plan.
    pub margin: f64,
    /// The accepted cost of the plan.
    pub accepted_cost: f64,
    /// When the first run started.
    pub started: String,
    /// When the last run made its last tool call.
    pub ended: String,
    /// The snapshot of the runs, when every run has the same one.
    pub snapshot: Option<String>,
    /// The arms, in the plan's order.
    pub arms: Vec<ArmScore>,
    /// The five replay traps.
    pub traps: Vec<Trap>,
    /// How many runs outside the baseline have no partner in it.
    pub unpaired: usize,
    /// The drift window the runs spanned, when they spanned one.
    pub quarantined: Option<Window>,
}

/// A scorecard that the record keeps.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KeptScorecard {
    /// The entry that holds it.
    pub entry: String,
    /// When that entry was written.
    pub created: String,
    /// Who wrote that entry.
    pub author: String,
    /// The scorecard.
    pub scorecard: Scorecard,
}

/// A value at six decimals: such a value is read back from JSON as it was written.
fn rounded(x: f64) -> f64 {
    (x * 1e6).round() / 1e6
}

fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.iter().sum::<f64>() / values.len() as f64
}

/// The mean of `differences` with `mean ± 1.96 * sd / sqrt(n)`, the sample standard
/// deviation. With fewer than two differences the interval is the mean alone.
fn interval(differences: &[f64]) -> Interval {
    let m = mean(differences);
    if differences.len() < 2 {
        return Interval {
            mean: rounded(m),
            low: rounded(m),
            high: rounded(m),
        };
    }
    let n = differences.len() as f64;
    let variance = differences.iter().map(|d| (d - m).powi(2)).sum::<f64>() / (n - 1.0);
    let half = 1.96 * variance.sqrt() / n.sqrt();
    Interval {
        mean: rounded(m),
        low: rounded(m - half),
        high: rounded(m + half),
    }
}

/// What the two intervals say under the plan's margin. `accepted` is whether the arm
/// costs no more than the accepted cost allows.
fn finding_of(plan: &Plan, success: &Interval, cost: &Interval, accepted: bool) -> Finding {
    let worse = success.high < -plan.margin;
    let no_worse = success.low >= -plan.margin;
    let better = success.low > 0.0;
    let cheaper = cost.high < 0.0;
    let dearer = cost.low > 0.0;
    if worse {
        Finding::Fails
    } else if (no_worse && cheaper) || (better && accepted) {
        Finding::Pays
    } else if no_worse && !better && dearer {
        Finding::Fails
    } else {
        Finding::Unclear
    }
}

fn cost_of(run: &Run) -> f64 {
    run.cost as f64
}

fn seeds_of<'a>(runs: impl Iterator<Item = &'a Run>) -> usize {
    runs.map(|r| r.seed).collect::<BTreeSet<u32>>().len()
}

/// The totals of an arm over its own runs, before any pair.
fn totals(arm: Arm, own: &[&Run]) -> ArmScore {
    let costs: Vec<f64> = own.iter().map(|r| cost_of(r)).collect();
    ArmScore {
        arm,
        baseline: false,
        runs: own.len(),
        passed: own.iter().filter(|r| r.success).count(),
        mean_cost: rounded(mean(&costs)),
        pairs: 0,
        seeds: 0,
        success: Interval::default(),
        cost: Interval::default(),
        finding: None,
        verdict: None,
    }
}

/// `score` with the paired differences of the arm against the baseline.
fn paired(plan: &Plan, mut score: ArmScore, pairs: &[(&Run, &Run)]) -> ArmScore {
    let success: Vec<f64> = pairs
        .iter()
        .map(|(a, b)| f64::from(u8::from(a.success)) - f64::from(u8::from(b.success)))
        .collect();
    let cost: Vec<f64> = pairs.iter().map(|(a, b)| cost_of(a) - cost_of(b)).collect();
    let of_arm: Vec<f64> = pairs.iter().map(|(a, _)| cost_of(a)).collect();
    let of_baseline: Vec<f64> = pairs.iter().map(|(_, b)| cost_of(b)).collect();
    let accepted = mean(&of_arm) <= plan.accepted_cost * mean(&of_baseline);
    score.pairs = pairs.len();
    score.seeds = seeds_of(pairs.iter().map(|(a, _)| *a));
    score.success = interval(&success);
    score.cost = interval(&cost);
    score.finding = Some(if pairs.len() < 2 {
        Finding::Unclear
    } else {
        finding_of(plan, &score.success, &score.cost, accepted)
    });
    score
}

/// Compare each arm of the plan with the baseline arm over `runs`, in the plan's
/// order. A pair is a task and a seed with a run in the arm and a run in the baseline;
/// a run with no partner is in the totals of its arm and in no difference. The
/// baseline gets no finding, and no arm gets a verdict here: a verdict also needs the
/// traps and the drift windows.
#[must_use]
pub fn compare(plan: &Plan, runs: &[Run]) -> Vec<ArmScore> {
    let baseline: BTreeMap<(&str, u32), &Run> = runs
        .iter()
        .filter(|r| r.arm == plan.baseline)
        .map(|r| ((r.task.as_str(), r.seed), r))
        .collect();
    plan.arms
        .iter()
        .map(|&arm| {
            let own: Vec<&Run> = runs.iter().filter(|r| r.arm == arm).collect();
            let mut score = totals(arm, &own);
            if arm == plan.baseline {
                score.baseline = true;
                score.seeds = seeds_of(own.iter().copied());
                return score;
            }
            let pairs: Vec<(&Run, &Run)> = own
                .iter()
                .filter_map(|r| baseline.get(&(r.task.as_str(), r.seed)).map(|b| (*r, *b)))
                .collect();
            paired(plan, score, &pairs)
        })
        .collect()
}

/// One run with the facts its tape gives and the judge its outcome names.
#[derive(Clone, Debug)]
struct Fact {
    run: Run,
    harness: String,
    model: Option<String>,
    judge: String,
    started: OffsetDateTime,
    ended: OffsetDateTime,
    snapshot: Option<OffsetDateTime>,
}

fn at(text: &str) -> Result<OffsetDateTime, Error> {
    OffsetDateTime::parse(text, &Rfc3339)
        .map_err(|e| Error::Transcript(format!("tape: the time {text:?} does not read: {e}")))
}

fn rfc3339(t: OffsetDateTime) -> String {
    t.format(&Rfc3339).unwrap_or_default()
}

fn trap(name: &str, held: bool, safe: &str, open: &str) -> Trap {
    Trap {
        name: name.to_string(),
        held,
        why: if held { safe } else { open }.to_string(),
    }
}

/// The snapshot every run has, when every run has the same one.
fn one_snapshot(facts: &[Fact]) -> Option<OffsetDateTime> {
    let first = facts.first()?.snapshot?;
    facts
        .iter()
        .all(|f| f.snapshot == Some(first))
        .then_some(first)
}

/// The five replay traps of an evaluation, in the paper's order.
fn traps_of(plan: &Plan, facts: &[Fact], arms: &[ArmScore], started: OffsetDateTime) -> Vec<Trap> {
    let failed = plan.tasks.iter().any(|t| t.stratum == Stratum::Failed);
    let before = one_snapshot(facts).is_some_and(|s| s <= started);
    let seeds = arms.iter().all(|a| a.baseline || a.seeds >= 2);
    let judge = facts.iter().all(|f| f.judge == plan.judge);
    let frozen = plan.tasks.iter().any(|t| t.set == TaskSet::Frozen);
    let rolling = plan.tasks.iter().any(|t| t.set == TaskSet::Rolling);
    vec![
        trap(
            TRAPS[0],
            failed,
            "The plan has a task that failed in production.",
            "The plan has no task with the stratum `failed`: tasks that passed say nothing about the tasks that failed.",
        ),
        trap(
            TRAPS[1],
            before,
            "Every run read the record at one snapshot that is not later than the first run.",
            "The runs do not have one snapshot that is not later than the first run: a run can read what a later session wrote.",
        ),
        trap(
            TRAPS[2],
            seeds,
            "Every arm has pairs from two seeds or more.",
            "An arm has pairs from fewer than two seeds: one run of a task does not show the noise.",
        ),
        trap(
            TRAPS[3],
            judge,
            "The judge of the plan decided every outcome.",
            "An outcome names a judge that is not the judge of the plan.",
        ),
        trap(
            TRAPS[4],
            frozen && rolling,
            "The plan has a frozen task and a rolling task.",
            "The plan needs a frozen task and a rolling task: one set alone does not show a shift of the tasks.",
        ),
    ]
}

/// Refuse runs that are not one evaluation: no run, two runs of one task, seed and
/// arm, two harnesses or models, or a harness or model that is not the plan's.
fn one_evaluation(plan: &Plan, facts: &[Fact]) -> Result<(), Error> {
    let Some(first) = facts.first() else {
        return Err(Error::Denied(
            "an evaluation is scored from one outcome or more".into(),
        ));
    };
    let mut seen = BTreeSet::new();
    for f in facts {
        let r = &f.run;
        if !seen.insert((r.task.as_str(), r.seed, r.arm.name())) {
            return Err(Error::Denied(format!(
                "two outcomes are the run of task {}, seed {}, arm {}",
                r.task, r.seed, r.arm
            )));
        }
        if f.harness != first.harness || f.model != first.model {
            return Err(Error::Denied(
                "the runs differ in harness or model: a scorecard compares arms and nothing else"
                    .into(),
            ));
        }
    }
    let harness = plan.harness.as_ref().is_none_or(|h| *h == first.harness);
    let model = plan.model.is_none() || plan.model == first.model;
    if !harness || !model {
        return Err(Error::Denied(
            "the runs are on a harness or a model that is not the plan's".into(),
        ));
    }
    Ok(())
}

/// Give each arm its verdict: quarantined when the runs spanned a drift window, else
/// inconclusive when a trap is open, else its finding.
fn give_verdicts(arms: &mut [ArmScore], traps: &[Trap], window: Option<&Window>) {
    let open: Vec<String> = traps
        .iter()
        .filter(|t| !t.held)
        .map(|t| t.name.clone())
        .collect();
    for arm in arms {
        let Some(finding) = arm.finding else {
            continue;
        };
        arm.verdict = Some(match (window, open.is_empty(), finding) {
            (Some(w), _, _) => Verdict::Quarantined {
                destination: w.destination.clone(),
                model: w.model.clone(),
            },
            (None, false, _) => Verdict::Inconclusive {
                traps: open.clone(),
            },
            (None, true, Finding::Pays) => Verdict::Pays,
            (None, true, Finding::Fails) => Verdict::Fails,
            (None, true, Finding::Unclear) => Verdict::Unclear,
        });
    }
}

/// The scorecard of `facts` under `plan`, with `windows` as the drift windows.
fn assemble(plan: &Plan, facts: &[Fact], windows: &[Window]) -> Result<Scorecard, Error> {
    one_evaluation(plan, facts)?;
    let first = &facts[0];
    let runs: Vec<Run> = facts.iter().map(|f| f.run.clone()).collect();
    let mut arms = compare(plan, &runs);
    let started = facts
        .iter()
        .map(|f| f.started)
        .min()
        .unwrap_or(first.started);
    let ended = facts.iter().map(|f| f.ended).max().unwrap_or(first.ended);
    let traps = traps_of(plan, facts, &arms, started);
    let quarantined = first.model.as_ref().and_then(|model| {
        windows
            .iter()
            .find(|w| w.model == *model && w.overlaps(started, ended))
            .cloned()
    });
    give_verdicts(&mut arms, &traps, quarantined.as_ref());
    let outside = runs.iter().filter(|r| r.arm != plan.baseline).count();
    let pairs: usize = arms.iter().map(|a| a.pairs).sum();
    Ok(Scorecard {
        plan: plan.name.clone(),
        plan_digest: plan.digest(),
        frozen_digest: plan.frozen_digest(),
        domain: plan.domain.clone(),
        harness: first.harness.clone(),
        model: first.model.clone(),
        judge: plan.judge.clone(),
        baseline: plan.baseline,
        margin: plan.margin,
        accepted_cost: plan.accepted_cost,
        started: rfc3339(started),
        ended: rfc3339(ended),
        snapshot: one_snapshot(facts).map(rfc3339),
        arms,
        traps,
        unpaired: outside - pairs,
        quarantined,
    })
}

fn verdict_words(verdict: Option<&Verdict>) -> String {
    match verdict {
        None => "the baseline".into(),
        Some(Verdict::Pays) => "pays".into(),
        Some(Verdict::Fails) => "fails".into(),
        Some(Verdict::Unclear) => "unclear".into(),
        Some(Verdict::Inconclusive { traps }) => {
            format!("inconclusive ({})", traps.join(", "))
        }
        Some(Verdict::Quarantined { destination, model }) => {
            format!("quarantined (drift window of {destination}/{model})")
        }
    }
}

fn finding_words(finding: Option<Finding>) -> &'static str {
    match finding {
        None => "",
        Some(Finding::Pays) => "pays",
        Some(Finding::Fails) => "fails",
        Some(Finding::Unclear) => "unclear",
    }
}

fn interval_words(i: &Interval) -> String {
    format!("{:.3} ({:.3} to {:.3})", i.mean, i.low, i.high)
}

/// The verdicts of a scorecard on one line.
fn summary_of(card: &Scorecard) -> String {
    let verdicts: Vec<String> = card
        .arms
        .iter()
        .filter(|a| !a.baseline)
        .map(|a| format!("{} {}", a.arm, verdict_words(a.verdict.as_ref())))
        .collect();
    format!(
        "Against {}, margin {}: {}.",
        card.baseline,
        card.margin,
        verdicts.join("; ")
    )
}

/// The body of a scorecard's entry: the table, the traps, and the scorecard as JSON.
fn body_of(card: &Scorecard, title: &str) -> Result<String, Error> {
    let mut out = format!("# {title}\n\n{}\n\n", summary_of(card));
    out.push_str(
        "| Arm | Runs | Passed | Mean cost | Pairs | Success, arm minus baseline | Cost, arm minus baseline | Numbers alone | Verdict |\n|---|---|---|---|---|---|---|---|---|\n",
    );
    for a in &card.arms {
        let (success, cost) = if a.baseline {
            (String::new(), String::new())
        } else {
            (interval_words(&a.success), interval_words(&a.cost))
        };
        out.push_str(&format!(
            "| {} | {} | {} | {:.0} | {} | {success} | {cost} | {} | {} |\n",
            a.arm,
            a.runs,
            a.passed,
            a.mean_cost,
            a.pairs,
            finding_words(a.finding),
            verdict_words(a.verdict.as_ref()),
        ));
    }
    out.push_str("\n## Replay traps\n\n");
    for t in &card.traps {
        let state = if t.held { "held" } else { "open" };
        out.push_str(&format!("- `{}`, {state}: {}\n", t.name, t.why));
    }
    let json = serde_json::to_string_pretty(card)
        .map_err(|e| Error::Transcript(format!("scorecard: {e}")))?;
    out.push_str(&format!(
        "\n```platform {KIND} {}\n{json}\n```\n",
        card.plan
    ));
    Ok(out)
}

/// How a launcher starts a run: on which harness and model.
#[derive(Clone, Copy, Debug)]
pub struct Launched<'a> {
    /// The harness, by its binary name.
    pub harness: &'a str,
    /// The model, when the launch names one.
    pub model: Option<&'a str>,
}

impl Memory {
    /// Make this session one run of `plan`: the task `task`, the seed `seed`, the arm
    /// `arm`, with the record read as of `as_of`. The session gets the settings of the
    /// arm with that snapshot. Its tape is kept in the plan's domain, and the head of
    /// the tape names the run. A launcher calls this once, where it would give the
    /// session its settings; nothing a session can call does.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for a task the plan does not hold, an arm the plan does not
    /// name, a seed at or above the plan's seeds, a domain that is not granted, a
    /// harness or a model that is not the plan's when the plan names one, and a
    /// session that is a run or has its settings already.
    pub fn start_evaluation(
        &self,
        plan: &Plan,
        (task, seed, arm): (&str, u32, Arm),
        as_of: OffsetDateTime,
        launched: Launched<'_>,
    ) -> Result<RunOf, Error> {
        if self.evaluation.get().is_some() {
            return Err(Error::Denied(
                "this session is a run of an evaluation already".into(),
            ));
        }
        plan.holds(task, seed, arm)?;
        if !self.grants.domains.contains_key(&plan.domain) {
            return Err(Error::Denied(format!(
                "the domain {} of the plan {} is not granted",
                plan.domain, plan.name
            )));
        }
        let harness = plan
            .harness
            .as_deref()
            .is_none_or(|h| h == launched.harness);
        let model = plan.model.is_none() || plan.model.as_deref() == launched.model;
        if !harness || !model {
            return Err(Error::Denied(format!(
                "the plan {} fixes the harness or the model, and this launch is on another",
                plan.name
            )));
        }
        let mut settings = arm.settings();
        settings.not_after = Some(as_of);
        self.set_settings(settings)?;
        let run = RunOf {
            plan: plan.digest(),
            task: task.to_string(),
            seed,
            arm,
        };
        let _ = self.evaluation.set((run.clone(), plan.domain.clone()));
        Ok(run)
    }

    /// The run this session is, when a launcher made it one.
    #[must_use]
    pub fn evaluation(&self) -> Option<RunOf> {
        self.evaluation.get().map(|(run, _)| run.clone())
    }

    /// The domain this session's tape goes to as a run of an evaluation.
    pub(crate) fn evaluation_domain(&self) -> Option<String> {
        self.evaluation.get().map(|(_, domain)| domain.clone())
    }

    /// The facts of one outcome, from the tape of its session.
    async fn fact_of(&self, plan: &Plan, digest: &str, outcome: &Outcome) -> Result<Fact, Error> {
        let session = &outcome.session;
        let tape = self.tape(session).await?;
        let start = &tape.header.start;
        let Some(run) = &start.evaluation else {
            return Err(Error::Denied(format!(
                "the tape of {session} names no evaluation: the session was not started as a run"
            )));
        };
        if run.plan != digest {
            return Err(Error::Denied(format!(
                "the session {session} started under another plan ({}): this plan is {digest}",
                run.plan
            )));
        }
        let prompt = plan.prompt_of(&run.task);
        if prompt.is_none() || start.prompt != prompt {
            return Err(Error::Denied(format!(
                "the prompt of {session} is not the prompt of the task {} in the plan",
                run.task
            )));
        }
        let started = at(&tape.header.started_at)?;
        let last = match tape.calls.last() {
            Some(call) => at(&call.at)?,
            None => started,
        };
        Ok(Fact {
            run: Run {
                task: run.task.clone(),
                seed: run.seed,
                arm: run.arm,
                success: outcome.success,
                cost: outcome.cost,
            },
            harness: start.harness.clone(),
            model: start.model.clone(),
            judge: outcome.judge.clone(),
            started,
            ended: last.max(started),
            snapshot: start.settings.not_after,
        })
    }

    /// Score an evaluation: the scorecard of `outcomes` under `plan`. The task, the
    /// seed, the arm, the harness, the model, the start time and the snapshot of each
    /// run come from the tape of its session, never from the caller. The success, the
    /// cost and the judge of each outcome are the caller's, and are trusted. The
    /// owner's alone, because it reads tapes.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for anyone but the owner; for no outcome; for an outcome whose
    /// tape names no evaluation, names another plan, or has a prompt that is not the
    /// task's; for two outcomes of one task, seed and arm; and for runs that differ in
    /// harness or model, or are not on the plan's. [`Error::NotFound`] for a session
    /// with no tape. A store error.
    pub async fn score(&self, plan: &Plan, outcomes: &[Outcome]) -> Result<Scorecard, Error> {
        self.owner_only("tapes")?;
        let digest = plan.digest();
        let mut facts = Vec::new();
        for outcome in outcomes {
            facts.push(self.fact_of(plan, &digest, outcome).await?);
        }
        assemble(plan, &facts, &self.drift_windows()?)
    }

    /// Keep a scorecard: one `decision` entry in the platform's domain. Its title names
    /// the plan, the model and the date of the first run, its summary gives the
    /// verdicts, and its body holds a table, the traps, and the scorecard as JSON in a
    /// fenced block. The owner's alone, as the scoring is.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for anyone but the owner, and when the grants name no
    /// platform domain. What a write refuses.
    pub async fn keep_scorecard(&self, card: &Scorecard) -> Result<Written, Error> {
        if self.author != self.grants.owner {
            return Err(Error::Denied(format!(
                "the owner keeps a scorecard: {}",
                self.grants.owner
            )));
        }
        let Some(domain) = self.platform_domain() else {
            return Err(Error::Denied(
                "a scorecard is kept in the platform's domain, and the grants name none (`platform = \"<domain>\"`)"
                    .into(),
            ));
        };
        let on = card.model.as_deref().unwrap_or(&card.harness);
        let date: String = card.started.chars().take(10).collect();
        let title = format!("{TITLE}{}: {on}, {date}", card.plan);
        self.write(WriteRequest {
            domain: Some(domain),
            entry_type: EntryType::Decision,
            body: body_of(card, &title)?,
            title: Some(title),
            summary: Some(summary_of(card)),
            ..WriteRequest::default()
        })
        .await
    }

    /// The scorecards the record keeps, newest first, at most `limit`. They are served
    /// under this session's read filter, as `list` and `get` serve entries: the label
    /// of the session rises, and a session that does not read the entry gets nothing
    /// of it. Empty when the grants name no platform domain.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] when the platform's domain is outside what this session
    /// reads. An index or a store error.
    pub async fn scorecards(&self, limit: usize) -> Result<Vec<KeptScorecard>, Error> {
        let Some(domain) = self.platform_domain() else {
            return Ok(Vec::new());
        };
        let listed = self
            .list(Some(&domain), Some(EntryType::Decision), LISTED)
            .await?;
        let mut out = Vec::new();
        for hit in listed.iter().filter(|h| h.title.starts_with(TITLE)) {
            if out.len() >= limit {
                break;
            }
            let Ok(entry) = self.get(&hit.id).await else {
                continue;
            };
            let kept = artifacts_in(&entry)
                .into_iter()
                .filter(|a| a.kind == KIND)
                .find_map(|a| {
                    let scorecard = serde_json::from_str::<Scorecard>(&a.text).ok()?;
                    Some(KeptScorecard {
                        entry: a.entry,
                        created: a.created,
                        author: entry.front.author.clone(),
                        scorecard,
                    })
                });
            out.extend(kept);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests;
