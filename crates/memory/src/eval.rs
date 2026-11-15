//! `amem eval`: the evaluation arms, and the settings a launcher gives a session from
//! `--arm` and `--as-of`. The arms and the settings are the library's
//! (`dm_server::arms`); this module prints them and hands them over.

use crate::Fallible;
use clap::Subcommand;
use dm_server::arms::{self, Arm, Settings};
use dm_server::Memory;

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
}

/// Run `amem eval`.
///
/// # Errors
///
/// When the answer cannot be written.
pub fn cmd(op: &EvalOp) -> Fallible {
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
    }
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
