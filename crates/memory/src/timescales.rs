//! `amem registry`, `amem proposal`, `amem recover`, `amem outage` and `amem horizon`:
//! the ownership registry and its check, the proposals to a personal domain, the
//! recovery bundle with its restore and its restore drill, the declared outage, and the
//! evidence tasks for the horizon scanner. Each command is one call into the library,
//! and the memory server calls the same functions.

use crate::curation::Cycle;
use crate::Fallible;
use clap::Subcommand;
use dm_server::recovery::{self, Authority};
use dm_server::timescales::{day, proposal_lines};
use dm_server::Memory;
use serde_json::json;
use std::path::{Path, PathBuf};
use time::{Date, OffsetDateTime};

/// What `amem registry` does.
#[derive(Subcommand)]
pub enum RegistryOp {
    /// Check `registry.toml` against the grants: each granted domain has a unit with an
    /// owner who answers today, the person of each personal domain holds the grants,
    /// and each unit names a horizon scanner. A root with no registry file has nothing
    /// to flag.
    Check {
        /// File one task for each flag. A flag that has a task gets no second task.
        #[arg(long)]
        file: bool,
        /// The day to check for, `YYYY-MM-DD`. Today when omitted.
        #[arg(long)]
        today: Option<String>,
    },
    /// Read the identifier of a person, and follow the assertions of the registry to
    /// the identifier that names the same person now.
    Resolve {
        /// `human:<authority>,<YYYY-MM-DD>:<user-id>` or `human:<system>:<id>`.
        id: String,
    },
}

/// What `amem proposal` does.
#[derive(Subcommand)]
pub enum ProposalOp {
    /// The proposals to personal domains that wait for an answer. The owner's act.
    List,
    /// Admit a proposal: it goes into the record as it was written, with its author.
    Admit {
        /// The proposal.
        id: String,
    },
    /// Refuse a proposal: it is removed, and the same proposal is not held again.
    Refuse {
        /// The proposal.
        id: String,
    },
    /// Remove each proposal that waited longer than the period of its domain. Nothing
    /// is kept of an expired proposal.
    Expire {
        /// The day to count to, `YYYY-MM-DD`. Today when omitted.
        #[arg(long)]
        today: Option<String>,
    },
}

/// What `amem recover` does.
#[derive(Subcommand)]
pub enum RecoverOp {
    /// Make a recovery bundle of this root in a directory that is empty or not there:
    /// the record, the logs with their cursors, the events of the ledger, the writes
    /// held in quarantine and the configuration. The owner's act.
    Bundle {
        /// The directory for the bundle.
        dest: PathBuf,
    },
    /// Copy the erasure journal of this root, the receipt of each erasure, to a
    /// directory, with the refusal of each proposal to a personal domain. Keep it apart
    /// from the bundles: a restore takes it.
    Journal {
        /// The directory for the journal.
        dest: PathBuf,
    },
    /// Restore a bundle into a scratch root and report each of five cases as recovered,
    /// absent or failed. The record and the ledger of this root are not written: only
    /// its claim epoch file moves.
    Drill {
        /// The directory of the bundle.
        bundle: PathBuf,
        /// The scratch root: a directory that is empty or not there. A new directory
        /// under `state/recovery/` when omitted.
        #[arg(long)]
        scratch: Option<PathBuf>,
    },
    /// Restore a bundle into the root that `--root` names: a directory that is empty or
    /// not there. The grants, the registry, the erasure journal and the claim epoch are
    /// those in force now, and the bundle does not roll them back.
    Restore {
        /// The directory of the bundle.
        bundle: PathBuf,
        /// The grants file in force now. It becomes the grants of the restored root.
        #[arg(long)]
        grants: PathBuf,
        /// The registry file in force now, when there is one.
        #[arg(long)]
        registry: Option<PathBuf>,
        /// The erasure journal that `amem recover journal` kept.
        #[arg(long)]
        journal: Option<PathBuf>,
        /// The claim epoch file. The restore takes the next number from it.
        #[arg(long)]
        epoch_authority: PathBuf,
    },
}

/// What `amem outage` does.
#[derive(Subcommand)]
pub enum OutageOp {
    /// The outage that is declared now, if there is one.
    Status,
    /// Declare an outage: each read and each change waits until it ends. A handoff and
    /// a heartbeat are queued. The owner's act.
    Declare {
        /// Why, in a few words.
        #[arg(long)]
        reason: String,
    },
    /// End the outage, and replay the queued handoffs and heartbeats in order. The
    /// owner's act.
    End,
}

/// What `amem horizon` does.
#[derive(Subcommand)]
pub enum HorizonOp {
    /// File the evidence task and the review task of each unit that names a horizon
    /// scanner, for the period that holds today. The tasks of one period are filed once.
    Schedule {
        /// The day to schedule for, `YYYY-MM-DD`. Today when omitted.
        #[arg(long)]
        today: Option<String>,
    },
}

/// The day a command names, or today.
fn day_or_today(text: Option<&str>) -> Result<Date, dm_server::Error> {
    text.map_or_else(|| Ok(OffsetDateTime::now_utc().date()), day)
}

/// Run `amem registry`.
///
/// # Errors
///
/// A date that is not `YYYY-MM-DD`, an identifier that names no person, or what the
/// library returns.
pub async fn registry(m: &Memory, op: RegistryOp) -> Fallible {
    match op {
        RegistryOp::Check { file, today } => {
            let today = day_or_today(today.as_deref())?;
            let check = m.registry_check(today).await?;
            let (filed, already) = if file {
                m.file_registry_flags(today).await?
            } else {
                (Vec::new(), 0)
            };
            crate::json(&json!({
                "registry": check.registry, "flags": check.flags,
                "filed": filed, "already_filed": already,
            }))
        }
        RegistryOp::Resolve { id } => crate::json(&m.resolve_identity(&id)?),
    }
}

/// Run `amem proposal`.
///
/// # Errors
///
/// An author who is not the owner, a proposal that is not there, or what the library
/// returns.
pub async fn proposal(m: &Memory, op: ProposalOp) -> Fallible {
    let answer = |id: String, admit: bool| async move {
        let written = m.answer_proposal(&id, admit).await?;
        crate::json(&json!({ "id": id, "admitted": admit, "written": written }))
    };
    match op {
        ProposalOp::List => crate::json(&json!({ "proposals": proposal_lines(&m.proposals()?) })),
        ProposalOp::Admit { id } => answer(id, true).await,
        ProposalOp::Refuse { id } => answer(id, false).await,
        ProposalOp::Expire { today } => {
            let expired = m.expire_proposals(day_or_today(today.as_deref())?).await?;
            crate::json(&json!({ "expired": expired }))
        }
    }
}

/// `amem recover restore`: the bundle restored into `root`, with the grants, the
/// registry, the erasure journal and the claim epoch file that the operator names.
async fn restore(
    root: &Path,
    bundle: &Path,
    grants: &Path,
    registry: Option<&Path>,
    journal: Option<&Path>,
    epoch_authority: &Path,
) -> Fallible {
    let read =
        |path: &Path| std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()));
    let grants = read(grants)?;
    let registry = registry.map(read).transpose()?;
    let journal_dir = journal;
    let (journal, refusals) = match journal {
        Some(dir) => (recovery::read_journal(dir)?, recovery::read_refusals(dir)?),
        None => (Vec::new(), Vec::new()),
    };
    let (transcripts, windows) = match journal_dir {
        Some(dir) => (
            recovery::read_transcript_journal(dir)?,
            recovery::read_windows(dir)?,
        ),
        None => (Vec::new(), None),
    };
    let authority = Authority {
        grants: &grants,
        registry: registry.as_deref(),
        journal: &journal,
        refusals: &refusals,
        transcripts: &transcripts,
        drift_windows: windows.as_deref(),
        epoch_file: epoch_authority,
        file_ledger: false,
    };
    crate::json(&recovery::restore(bundle, root, &authority).await?)
}

/// Run `amem recover`. The restore runs on a root that is not there yet; each other
/// command opens the root.
///
/// # Errors
///
/// An author who is not the owner, a bundle that is not whole, a directory that holds
/// something, or what the library returns.
pub async fn recover(root: &Path, op: RecoverOp) -> Fallible {
    match op {
        RecoverOp::Bundle { dest } => {
            crate::json(&Memory::open(root)?.recovery_bundle(&dest).await?)
        }
        RecoverOp::Journal { dest } => {
            let m = Memory::open(root)?;
            let receipts = m.erasure_journal().await?;
            let transcripts = m.transcript_journal().await?;
            let refusals = m.refusal_journal()?;
            let windows = m.drift_journal()?;
            recovery::write_journal(&dest, &receipts)?;
            recovery::write_transcript_journal(&dest, &transcripts)?;
            recovery::write_refusals(&dest, &refusals)?;
            recovery::write_windows(&dest, windows.as_deref())?;
            crate::json(&json!({
                "journal": dest,
                "receipts": receipts.len(),
                "transcripts": transcripts.len(),
                "refusals": refusals.len(),
                "drift_windows": windows.is_some(),
            }))
        }
        RecoverOp::Drill { bundle, scratch } => {
            let m = Memory::open(root)?;
            let scratch = scratch.unwrap_or_else(|| m.drill_scratch());
            let mut drill = serde_json::to_value(m.drill(&bundle, &scratch).await?)?;
            drill["scratch"] = json!(scratch);
            crate::json(&drill)
        }
        RecoverOp::Restore {
            bundle,
            grants,
            registry,
            journal,
            epoch_authority,
        } => {
            restore(
                root,
                &bundle,
                &grants,
                registry.as_deref(),
                journal.as_deref(),
                &epoch_authority,
            )
            .await
        }
    }
}

/// Run `amem outage`.
///
/// # Errors
///
/// An author who is not the owner for a declaration or an end, or what the library
/// returns.
pub async fn outage(m: &Memory, op: OutageOp) -> Fallible {
    match op {
        OutageOp::Status => crate::json(&m.outage_status()),
        OutageOp::Declare { reason } => crate::json(&m.declare_outage(&reason)?),
        OutageOp::End => crate::json(&m.end_outage().await?),
    }
}

/// Run `amem horizon`.
///
/// # Errors
///
/// A session that is not the session of a person, a date that is not `YYYY-MM-DD`, or
/// what the library returns.
pub async fn horizon(m: &Memory, op: HorizonOp) -> Fallible {
    match op {
        HorizonOp::Schedule { today } => {
            let scheduled = m.schedule_horizon(day_or_today(today.as_deref())?).await?;
            crate::json(&json!({ "scheduled": scheduled }))
        }
    }
}

/// The part of a curation cycle for the long timescales: a task for each flag of the
/// registry, and the tasks of each horizon scanner for the period of today. A root with
/// no registry, or a registry that names no unit and no domain, gets nothing. The
/// session of a cycle is the session of an agent, and only a person schedules the
/// evidence for a horizon scanner: so this opens the root as its owner.
///
/// # Errors
///
/// What [`Memory::open`], [`Memory::file_registry_flags`] and
/// [`Memory::schedule_horizon`] return.
pub async fn file_cycle(root: &Path, cycle: &mut Cycle) -> Result<(), dm_server::Error> {
    let owner = Memory::open(root)?;
    let named = |r: &dm_policy::registry::Registry| !(r.units.is_empty() && r.domains.is_empty());
    if !owner.registry().is_some_and(named) {
        return Ok(());
    }
    let today = OffsetDateTime::now_utc().date();
    let (filed, already) = owner.file_registry_flags(today).await?;
    cycle.registry_flags = filed.len() + already;
    let scheduled = owner.schedule_horizon(today).await?;
    cycle.horizon_filed = scheduled.iter().filter(|s| s.filed).count();
    Ok(())
}
