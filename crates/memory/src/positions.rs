//! `amem position`, and how a launcher puts a session in a position.
//!
//! A position is a verified entry of type `position`; `crates/server` holds every rule
//! about it. The commands here print what the library answers. A session does not
//! choose its position: `amem serve --position` and `amem work --position` fill the one
//! the operator names, and `amem work --wake` fills one that waits for a session under
//! this session's agent definition.

use crate::Fallible;
use clap::Subcommand;
use dm_server::positions::{agent_definition, Filled, Wake};
use dm_server::{Error, Memory};

/// Why `amem work` refuses `--position` and `--wake` on the `wslc` backend.
pub const NOT_ON_WSLC: &str = "--position and --wake do not reach the `wslc` backend: the \
     launcher inside the egress container takes neither, so the session would fill no \
     position. Run this on a backend of the host";

/// What `amem position` does.
#[derive(Subcommand)]
pub enum PositionOp {
    /// Every entry of type `position` this session may read, newest first: the record
    /// in force of each name with what is pinned to it, what waits, who is live in
    /// it and whether it needs a session started; any other entry with why it is not
    /// in force.
    List {
        /// A granted domain; every granted domain when omitted.
        #[arg(long)]
        domain: Option<String>,
    },
    /// One position: its charter and authority envelope, its open commitments, its
    /// handoff cache, and the sessions that hold a task from it.
    Show {
        /// The position's name, `pos-<name>`.
        name: String,
        /// A granted domain; the one that holds the position when omitted.
        #[arg(long)]
        domain: Option<String>,
    },
    /// Retire a position, as the owner: its tasks lose their pin and return to
    /// matching, and its history stays under its name.
    Retire {
        /// The position's name, `pos-<name>`.
        name: String,
        /// Why, in words on one line. The retiring entry keeps them.
        #[arg(long)]
        why: String,
        /// A granted domain; the one that holds the position when omitted.
        #[arg(long)]
        domain: Option<String>,
    },
}

/// Run `amem position`.
///
/// # Errors
///
/// What the library refuses: a position that is not there for this session, a
/// retirement by anyone but the owner, or a store or ledger error.
pub async fn cmd(m: &Memory, op: PositionOp) -> Fallible {
    match op {
        PositionOp::List { domain } => crate::json(&m.positions(domain.as_deref()).await?),
        PositionOp::Show { name, domain } => {
            crate::json(&m.position(&name, domain.as_deref()).await?)
        }
        PositionOp::Retire { name, why, domain } => {
            crate::json(&m.retire_position(&name, &why, domain.as_deref()).await?)
        }
    }
}

/// What a session is told on standard error when it has filled a position.
fn said(filled: &Filled) -> String {
    if filled.seconded {
        format!(
            "amem: seconded to {}: its charter names {}",
            filled.position, filled.agent
        )
    } else {
        format!("amem: fills {} ({})", filled.position, filled.entry)
    }
}

/// Fill the position `name` for a session that starts, and say on standard error what
/// the session is to it.
///
/// # Errors
///
/// As [`Memory::fill_position`]: the caller ends, since a session that was to fill a
/// position does not run outside it.
pub async fn fill(m: &Memory, name: &str, domain: Option<&str>) -> Result<Filled, Error> {
    let filled = m.fill_position(name, domain).await?;
    eprintln!("{}", said(&filled));
    Ok(filled)
}

/// The positions of a wake list that wait for a session under `agent`, and the ones
/// that wait for another agent definition, each in the order of the list.
fn for_agent(list: Vec<Wake>, agent: Option<&str>) -> (Vec<Wake>, Vec<Wake>) {
    list.into_iter()
        .partition(|w| Some(w.agent.as_str()) == agent)
}

/// The positions that wait for a session under this session's agent definition, in the
/// order of the wake list. When none does, each position that waits for another agent
/// definition is named on standard error.
///
/// # Errors
///
/// As [`Memory::wake_list`].
pub async fn waiting(m: &Memory, domain: Option<&str>) -> Result<Vec<Wake>, Error> {
    let list = m.wake_list(domain).await?;
    let (mine, others) = for_agent(list, agent_definition(m.author()));
    if mine.is_empty() {
        for w in others {
            eprintln!("amem: {} waits for a session under {}", w.position, w.agent);
        }
    }
    Ok(mine)
}

/// Wake a position: fill it for this session, and say so on standard error with the
/// tasks that wait.
///
/// # Errors
///
/// As [`Memory::fill_position`].
pub async fn wake(m: &Memory, woken: &Wake) -> Result<(), Error> {
    m.fill_position(&woken.position, Some(&woken.domain))
        .await?;
    eprintln!(
        "amem: woke {} for {}",
        woken.position,
        woken.tasks.join(", ")
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn waits(position: &str, agent: &str) -> Wake {
        Wake {
            position: position.into(),
            domain: "dom-me".into(),
            entry: format!("mem-me-{position}"),
            agent: agent.into(),
            tasks: vec!["task-me-1".into()],
        }
    }

    #[test]
    fn a_wake_takes_the_positions_that_name_the_session_s_agent_definition() {
        let list = || {
            vec![
                waits("pos-a", "codex"),
                waits("pos-b", "claude"),
                waits("pos-c", "claude"),
            ]
        };
        let (mine, others) = for_agent(list(), Some("claude"));
        assert_eq!(
            mine,
            vec![waits("pos-b", "claude"), waits("pos-c", "claude")]
        );
        assert_eq!(others, vec![waits("pos-a", "codex")]);
        // a person has no agent definition, and no position waits for one
        let (mine, others) = for_agent(list(), None);
        assert_eq!(mine, vec![]);
        assert_eq!(others.len(), 3);
    }

    #[test]
    fn a_session_is_told_whether_it_fills_the_position_or_is_seconded_to_it() {
        let mut filled = Filled {
            position: "pos-a".into(),
            domain: "dom-me".into(),
            entry: "mem-me-1".into(),
            agent: "codex".into(),
            seconded: false,
        };
        assert_eq!(said(&filled), "amem: fills pos-a (mem-me-1)");
        filled.seconded = true;
        assert_eq!(
            said(&filled),
            "amem: seconded to pos-a: its charter names codex"
        );
    }
}
