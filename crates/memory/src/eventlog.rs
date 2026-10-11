//! `amem log`: a domain's event log, the order of record of what happened to its
//! entries, and the materializer that builds the index from it.

use crate::Fallible;
use clap::Subcommand;
use dm_server::Memory;

/// What `amem log` does.
#[derive(Subcommand)]
pub enum LogOp {
    /// The domain's log, from a sequence number on: each version put and each erasure,
    /// by id and digest; never a body.
    Show {
        /// The domain; the only granted one when omitted.
        #[arg(long)]
        domain: Option<String>,
        /// The first sequence number shown.
        #[arg(long, default_value_t = 0)]
        from: u64,
    },
    /// Build the domain's index from its log and the record, and replay the ledger's
    /// events into its projection; say what the record holds at another version than
    /// the log names.
    Materialize {
        /// The domain; the only granted one when omitted.
        #[arg(long)]
        domain: Option<String>,
    },
}

/// Run `amem log`.
///
/// # Errors
///
/// Anyone but the owner, an ungranted domain, or a store, index or ledger error.
pub async fn cmd(m: &Memory, op: LogOp) -> Fallible {
    match op {
        LogOp::Show { domain, from } => crate::json(&m.log(domain.as_deref(), from)?),
        LogOp::Materialize { domain } => {
            let done = m.materialize(domain.as_deref()).await?;
            for id in &done.behind {
                eprintln!("memory: {id} is not at the version the log names");
            }
            crate::json(&done)
        }
    }
}
