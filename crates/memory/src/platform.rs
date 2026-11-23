//! `amem platform`: what the platform's domain holds for the launcher, and what of it is
//! loaded. An artifact loads only from an active entry a person verified; the listing
//! says why each of the rest does not.

use crate::Fallible;
use clap::Subcommand;
use dm_ledger::Autonomy;
use dm_server::evaluation::Answer;
use dm_server::Memory;

/// What `amem platform` does.
#[derive(Subcommand)]
pub enum PlatformOp {
    /// Every artifact in the platform's domain, newest first, loaded or not, and why not.
    List,
    /// The artifact the launcher loads for a kind and name.
    Show {
        /// `agent`, `template`, `eval` or `harness`.
        kind: String,
        /// A harness's binary, `default`, `work`, or a suite's name.
        name: String,
    },
    /// The autonomy level of a task definition's version in force, the highest level
    /// that its text lets it earn, and its grades, raises and work outcomes.
    Level {
        /// The name of the task definition.
        definition: String,
    },
    /// Grade handoffs against the held-out suite of a task definition. Only the
    /// evaluator grades, or the owner at this command line.
    Grade {
        /// The name of the task definition.
        definition: String,
        /// The level that the run is for: `person`, `review` or `checks`.
        #[arg(long)]
        level: Autonomy,
        /// One answer, as `<scenario>=<handoff entry>`; repeat for more.
        #[arg(long = "answer")]
        answers: Vec<String>,
    },
    /// Raise the level of a task definition one step, on its newest passing run. The
    /// step to `checks` is your sign-off.
    Raise {
        /// The name of the task definition, or `<name>@<entry>` for its version in force.
        definition: String,
    },
    /// The gates that an entry of the platform needs before it is verified, and
    /// whether each is met.
    Gates {
        /// The entry id of the candidate.
        entry: String,
    },
}

/// The `--answer <scenario>=<handoff>` of `platform grade`. The handoff is an entry id,
/// which holds no `=`, so the name of the scenario may hold one.
fn answer_of(text: &str) -> Result<Answer, Box<dyn std::error::Error>> {
    match text.rsplit_once('=') {
        Some((scenario, handoff)) if !scenario.is_empty() && !handoff.is_empty() => Ok(Answer {
            scenario: scenario.to_string(),
            handoff: handoff.to_string(),
        }),
        _ => Err(format!("{text:?} is not <scenario>=<handoff entry>").into()),
    }
}

/// `amem platform level`, `grade`, `raise` and `gates`: what a version of a task
/// definition has earned, and what holds a candidate back.
async fn earned(m: &Memory, op: PlatformOp) -> Fallible {
    match op {
        PlatformOp::Level { definition } => crate::json(&m.autonomy(&definition).await?),
        PlatformOp::Grade {
            definition,
            level,
            answers,
        } => {
            let answers: Vec<Answer> = answers
                .iter()
                .map(|a| answer_of(a))
                .collect::<Result<_, _>>()?;
            crate::json(&m.grade(&definition, level, &answers).await?)
        }
        PlatformOp::Raise { definition } => crate::json(&m.raise(&definition).await?),
        PlatformOp::Gates { entry } => crate::json(&m.candidate_gates(&entry).await?),
        PlatformOp::List | PlatformOp::Show { .. } => unreachable!("routed by cmd"),
    }
}

/// Run `amem platform`.
///
/// # Errors
///
/// Anyone but the owner, no platform domain in the grants, or a store error.
pub async fn cmd(m: &Memory, op: PlatformOp) -> Fallible {
    if m.platform_domain().is_none() {
        return Err("grants.toml names no platform domain: add platform = \"<domain>\"".into());
    }
    match op {
        PlatformOp::List => crate::json(&m.platform().await?),
        PlatformOp::Show { kind, name } => match m.platform_artifact(&kind, &name).await? {
            Some(a) => crate::json(&a),
            None => Err(format!("no verified {kind} artifact named {name} is loaded").into()),
        },
        op => earned(m, op).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_answer_is_a_scenario_and_a_handoff_with_the_last_equals_sign_between_them() {
        let a = answer_of("lease = five minutes?=handoff-dom-me-1").unwrap();
        assert_eq!(a.scenario, "lease = five minutes?");
        assert_eq!(a.handoff, "handoff-dom-me-1");
        for bad in ["lease question", "=handoff-dom-me-1", "lease question="] {
            let why = answer_of(bad).map(|_| ()).unwrap_err().to_string();
            assert!(why.contains("is not <scenario>=<handoff entry>"), "{why}");
        }
    }
}
