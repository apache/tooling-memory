//! `amem eval`: the evaluation arms, the plan of an evaluation and its scorecards, and
//! what a launcher gives a session from `--arm`, `--as-of` and the `--eval-*` flags.
//! The arms, the settings, the plan and the scorer are the library's
//! (`dm_server::arms`, `dm_server::evaluation`); this module prints them and hands them
//! over.

use crate::Fallible;
use clap::Subcommand;
use dm_server::arms::{self, Arm, Settings};
use dm_server::evaluation::{Launched, Outcome, Plan};
use dm_server::Memory;
use std::path::{Path, PathBuf};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// `--arm` and `--as-of` on the `wslc` backend: the settings are given to the memory
/// server in this process, and that backend starts its memory server in a container.
pub const NOT_ON_WSLC: &str = "--arm and --as-of do not reach the `wslc` backend: the \
    session's settings are given to the memory server on this host";

/// What `amem eval` does.
#[derive(Subcommand)]
pub enum EvalOp {
    /// The five arms of the comparison, in the paper's order: for each, what it stands
    /// for and the session settings it fixes.
    Arms,
    /// Read a plan: an evaluation set file with a `[plan]` table. Prints the plan and
    /// its digest, or why the plan is refused.
    Plan {
        /// The evaluation set file.
        file: PathBuf,
    },
    /// Score an evaluation, as the owner: the scorecard of the outcomes under the
    /// plan. Each outcome names the session of one run; its tape gives the task, the
    /// seed, the arm, the harness and the model.
    Score {
        /// The evaluation set file that the runs started under.
        plan: PathBuf,
        /// A JSON file: a list of `{session, success, cost, judge}`.
        outcomes: PathBuf,
        /// Also keep the scorecard as a decision entry in the platform's domain, and
        /// print the id of the entry.
        #[arg(long)]
        keep: bool,
    },
    /// The scorecards that the record keeps, newest first.
    Scorecards {
        /// The most scorecards to print.
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
}

fn plan_in(file: &Path) -> Result<Plan, Box<dyn std::error::Error>> {
    let text = std::fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
    Ok(Plan::parse(&text)?)
}

/// Run `amem eval`.
///
/// # Errors
///
/// A file that does not read, what the library refuses, or an answer that cannot be
/// written.
pub async fn cmd(root: &Path, op: EvalOp) -> Fallible {
    match op {
        EvalOp::Arms => {
            let arms: Vec<serde_json::Value> = Arm::ALL
                .into_iter()
                .map(|a| {
                    serde_json::json!({
                        "arm": a,
                        "stands_for": a.stands_for(),
                        "settings": a.settings(),
                    })
                })
                .collect();
            crate::json(&arms)
        }
        EvalOp::Plan { file } => {
            let plan = plan_in(&file)?;
            crate::json(&serde_json::json!({
                "digest": plan.digest(),
                "frozen_digest": plan.frozen_digest(),
                "plan": plan,
            }))
        }
        EvalOp::Score {
            plan,
            outcomes,
            keep,
        } => {
            let plan = plan_in(&plan)?;
            let text = std::fs::read_to_string(&outcomes)
                .map_err(|e| format!("{}: {e}", outcomes.display()))?;
            let outcomes: Vec<Outcome> = serde_json::from_str(&text)?;
            let m = Memory::open(root)?;
            let scorecard = m.score(&plan, &outcomes).await?;
            let entry = if keep {
                Some(m.keep_scorecard(&scorecard).await?.id)
            } else {
                None
            };
            crate::json(&serde_json::json!({ "scorecard": scorecard, "entry": entry }))
        }
        EvalOp::Scorecards { limit } => crate::json(&Memory::open(root)?.scorecards(limit).await?),
    }
}

/// One run of an evaluation, as `amem launch` names it with `--eval-plan`,
/// `--eval-task`, `--eval-seed`, `--arm` and `--as-of`.
#[derive(Clone, Debug)]
pub struct RunPlan {
    plan: Plan,
    task: String,
    seed: u32,
    arm: Arm,
    as_of: OffsetDateTime,
    prompt: String,
}

impl RunPlan {
    /// The prompt of the run: the plan's preamble, then the task's prompt.
    pub fn prompt(&self) -> &str {
        &self.prompt
    }
}

/// The flags of `amem launch` that make a session one run of an evaluation:
/// `--eval-plan`, `--eval-task`, `--eval-seed`, `--arm` and `--as-of`.
pub struct RunFlags {
    plan: Option<PathBuf>,
    task: Option<String>,
    seed: Option<u32>,
    arm: Option<String>,
    as_of: Option<String>,
}

impl RunFlags {
    /// The flags, in the order of the doc comment of the type.
    pub fn new(
        plan: Option<PathBuf>,
        task: Option<String>,
        seed: Option<u32>,
        arm: Option<String>,
        as_of: Option<String>,
    ) -> RunFlags {
        RunFlags {
            plan,
            task,
            seed,
            arm,
            as_of,
        }
    }
}

/// The run that the flags name; none when `--eval-plan` is not given.
///
/// # Errors
///
/// A plan file that does not read or is refused, a flag that is missing, a name that
/// is no arm, a snapshot that is not an RFC 3339 time, or a task, a seed or an arm
/// that is not the plan's.
pub fn run_of(flags: &RunFlags) -> Result<Option<RunPlan>, Box<dyn std::error::Error>> {
    let Some(file) = flags.plan.as_deref() else {
        if flags.task.is_some() || flags.seed.is_some() {
            return Err("--eval-task and --eval-seed need --eval-plan".into());
        }
        return Ok(None);
    };
    let plan = plan_in(file)?;
    let (Some(task), Some(seed), Some(arm), Some(as_of)) = (
        flags.task.as_deref(),
        flags.seed,
        flags.arm.as_deref(),
        flags.as_of.as_deref(),
    ) else {
        return Err("--eval-plan needs --eval-task, --eval-seed, --arm and --as-of".into());
    };
    let arm: Arm = arm.parse()?;
    let as_of = OffsetDateTime::parse(as_of, &Rfc3339)
        .map_err(|e| format!("the snapshot {as_of:?} is not an RFC 3339 time: {e}"))?;
    plan.holds(task, seed, arm)?;
    let Some(prompt) = plan.prompt_of(task) else {
        return Err(format!("the plan {} has no task {task}", plan.name).into());
    };
    Ok(Some(RunPlan {
        plan,
        task: task.to_string(),
        seed,
        arm,
        as_of,
        prompt,
    }))
}

/// What a launch asks for: the settings of the session, the run of an evaluation that
/// the session is, and the prompt.
pub type Asked = (Option<Settings>, Option<RunPlan>, Option<String>);

/// What `amem launch` asks for with its flags, `--prompt` and `--prompt-file`. A run of
/// an evaluation gets its settings from the library, with its arm, and its prompt from
/// the plan.
///
/// # Errors
///
/// What [`run_of`], [`settings`] and [`needs_prompt`] refuse, or a prompt file that
/// does not read.
pub fn asked(
    flags: &RunFlags,
    prompt: Option<String>,
    prompt_file: Option<PathBuf>,
) -> Result<Asked, Box<dyn std::error::Error>> {
    let evaluation = run_of(flags)?;
    let settings = match evaluation {
        Some(_) => None,
        None => settings(flags.arm.as_deref(), flags.as_of.as_deref())?,
    };
    needs_prompt(settings.as_ref(), prompt.is_some() || prompt_file.is_some())?;
    let prompt = match (&evaluation, prompt, prompt_file) {
        (Some(run), _, _) => Some(run.prompt().to_string()),
        (None, Some(p), _) => Some(p),
        (None, None, Some(f)) => Some(std::fs::read_to_string(&f)?),
        (None, None, None) => None,
    };
    Ok((settings, evaluation, prompt))
}

/// Make a session the run that the launch names: once, before it reads. The library
/// gives the session the settings of the arm at the snapshot.
///
/// # Errors
///
/// What [`Memory::start_evaluation`] refuses.
pub fn start(memory: &Memory, run: &RunPlan, harness: &str, model: Option<&str>) -> Fallible {
    let started = memory.start_evaluation(
        &run.plan,
        (&run.task, run.seed, run.arm),
        run.as_of,
        Launched { harness, model },
    )?;
    eprintln!(
        "amem: evaluation {}: task {}, seed {}, arm {} ({}), plan {}",
        run.plan.name,
        started.task,
        started.seed,
        started.arm,
        started.arm.stands_for(),
        started.plan
    );
    Ok(())
}

/// The settings `--arm` and `--as-of` name; none when neither is given.
///
/// # Errors
///
/// A name that is no arm, or a snapshot that is not an RFC 3339 time.
pub fn settings(
    arm: Option<&str>,
    as_of: Option<&str>,
) -> Result<Option<Settings>, Box<dyn std::error::Error>> {
    Ok(arms::from_flags(arm, as_of)?)
}

/// Refuse settings that replay transcripts where no prompt is given to put them in
/// front of: the session would run as the no-memory arm under another arm's name.
///
/// # Errors
///
/// When the settings replay transcripts and `prompted` is false.
pub fn needs_prompt(settings: Option<&Settings>, prompted: bool) -> Fallible {
    if settings.is_some_and(|s| s.transcripts) && !prompted {
        return Err(
            "the transcript-replay arm puts the transcripts in front of a prompt: use \
             `amem launch` with --prompt or --prompt-file"
                .into(),
        );
    }
    Ok(())
}

/// Give a session its settings, when the launch named any: once, before it reads.
///
/// # Errors
///
/// When the session has its settings already.
pub fn give(memory: &Memory, settings: Option<&Settings>) -> Fallible {
    if let Some(s) = settings {
        memory.set_settings(s.clone())?;
        match Arm::of(s) {
            Some(arm) => eprintln!("amem: arm {arm} ({})", arm.stands_for()),
            None => eprintln!("amem: session settings that are no arm's"),
        }
        if let Some(at) = s.not_after {
            let at = at
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_default();
            eprintln!("amem: the record as of {at}: later entries are not read");
        }
    }
    Ok(())
}
