//! `amem log`, `amem lineage` and `amem withheld`: a domain's event log, the order of
//! record of what happened to its entries; the feed and the materializer that bring the
//! index up to it; the lineage read from it; and the entries an erasure withheld.

use crate::Fallible;
use clap::Subcommand;
use dm_server::lineage::Direction;
use dm_server::Memory;

/// What `amem log` does.
#[derive(Subcommand)]
pub enum LogOp {
    /// The domain's log, from a sequence number on: each write offered, each version
    /// put, each write refused, each erasure, each read, each send and each event of the
    /// ledger, by id and digest; never a body. The ledger's events are copied in first.
    /// For the owner and the security role; a quarantined write is shown only to a
    /// session that handles restricted.
    Show {
        /// The domain; the only granted one when omitted.
        #[arg(long)]
        domain: Option<String>,
        /// The first sequence number shown.
        #[arg(long, default_value_t = 0)]
        from: u64,
    },
    /// Feed the domain's index from its log: each version the log holds above the
    /// cursor of the index goes into the index as the record has it. A writer does this
    /// as part of its write; this is for what a stopped writer left.
    Feed {
        /// The domain; the only granted one when omitted.
        #[arg(long)]
        domain: Option<String>,
    },
    /// How far the domain's index is behind its log, against the published bound of 60
    /// seconds: the versions that wait for the feed, and the age of the oldest.
    Freshness {
        /// The domain; the only granted one when omitted.
        #[arg(long)]
        domain: Option<String>,
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

/// What `amem withheld` does.
#[derive(Subcommand)]
pub enum WithheldOp {
    /// The withheld entries of a domain that no entry replaced yet, each with the
    /// erased entry whose receipt names it.
    List {
        /// The domain; the only granted one when omitted.
        #[arg(long)]
        domain: Option<String>,
    },
    /// Release a withheld entry: it stands as it is, and retrieval serves it again.
    /// The owner's act.
    Release {
        /// The entry.
        id: String,
    },
}

/// Run `amem log`.
///
/// # Errors
///
/// An author outside the security role for the log, the feed and the freshness, anyone
/// but the owner for the materializer, an ungranted domain, or a store, index or ledger
/// error.
pub async fn cmd(m: &Memory, op: LogOp) -> Fallible {
    match op {
        LogOp::Show { domain, from } => crate::json(&m.log_synced(domain.as_deref(), from).await?),
        LogOp::Feed { domain } => crate::json(&m.feed(domain.as_deref()).await?),
        LogOp::Freshness { domain } => {
            let fresh = m.freshness(domain.as_deref())?;
            if !fresh.within_bound {
                eprintln!(
                    "amem: the index of {} is behind its log by more than {} seconds: {} unfed",
                    fresh.domain, fresh.bound_seconds, fresh.unfed
                );
            }
            crate::json(&fresh)
        }
        LogOp::Materialize { domain } => {
            let done = m.materialize(domain.as_deref()).await?;
            for id in &done.behind {
                eprintln!("amem: {id} is not at the version the log names");
            }
            crate::json(&done)
        }
    }
}

/// Run `amem lineage`: forward from an entry, or with `back` what an entry or a send
/// event named `<domain>@<seq>` may have been made from.
///
/// # Errors
///
/// An author outside the security role, a send asked about forward, or a ledger error.
pub async fn lineage(m: &Memory, of: &str, back: bool, depth: Option<u32>) -> Fallible {
    let direction = if back {
        Direction::Back
    } else {
        Direction::Forward
    };
    crate::json(&m.lineage(of, direction, depth).await?)
}

/// Run `amem withheld`.
///
/// # Errors
///
/// An author outside the security role for the list, anyone but the owner for a
/// release, an entry that is not withheld or was derived again, or a store error.
pub async fn withheld(m: &Memory, op: WithheldOp) -> Fallible {
    match op {
        WithheldOp::List { domain } => crate::json(&m.withheld(domain.as_deref()).await?),
        WithheldOp::Release { id } => {
            m.release_withheld(&id).await?;
            crate::json(&serde_json::json!({ "released": id }))
        }
    }
}
