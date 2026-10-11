//! `amem tool` and `amem source`: a tool tap called as the owner, and the sources
//! tools brought in, listed, shown and swept.

use crate::Fallible;
use clap::Subcommand;
use dm_server::Memory;
use std::path::Path;
use std::sync::Arc;

/// What `amem source` does.
#[derive(Subcommand, Debug)]
pub enum SourceOp {
    /// What tools brought in and the record keeps, newest first: the tool, what it was
    /// asked, when it runs out, and whether it is held for the owner.
    List,
    /// One source in full, held or not: the owner's view.
    Show {
        /// Its id, as a citation names it: `source:<id>`.
        id: String,
    },
    /// Drop the sources whose clock ran out and that no live entry cites.
    Sweep,
    /// Release a held answer once you have read it (`amem source show`): the same call
    /// is served from it from then on, with what the scan found.
    Release {
        /// Its id: `source:<id>` or `<id>`.
        id: String,
    },
}

/// Call a tool as the owner: `args` is the tool's JSON.
///
/// # Errors
///
/// Arguments that are not JSON, or the tool's refusal or failure.
pub async fn call(root: &Path, tool: &str, args: &str) -> Fallible {
    let args: serde_json::Value =
        serde_json::from_str(args).map_err(|e| format!("args are not JSON: {e}"))?;
    let m = Arc::new(Memory::open(root)?);
    m.set_tools(crate::taps::TapTools::for_root(root));
    let answer = m.use_tool(tool, args).await?;
    println!("{}", serde_json::to_string_pretty(&answer)?);
    Ok(())
}

/// `amem source`.
///
/// # Errors
///
/// Anyone but the owner, an unknown id, or a store error.
pub async fn cmd(m: &Memory, op: SourceOp) -> Fallible {
    match op {
        SourceOp::List => {
            let rows: Vec<serde_json::Value> = m
                .sources()
                .await?
                .iter()
                .map(|s| {
                    serde_json::json!({
                        "id": s.id, "tool": s.tool, "args": s.args, "domain": s.domain,
                        "classification": s.classification.to_string(),
                        "kept_at": s.kept_at.to_string(), "retain_until": s.retain_until.to_string(),
                        "held": s.held, "findings": s.findings, "session": s.session,
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&rows)?);
        }
        SourceOp::Show { id } => {
            let id = id.strip_prefix(dm_server::sources::CITATION).unwrap_or(&id);
            println!(
                "{}",
                serde_json::to_string_pretty(&m.source_for_owner(id).await?)?
            );
        }
        SourceOp::Release { id } => {
            let id = id.strip_prefix(dm_server::sources::CITATION).unwrap_or(&id);
            let s = m.release_source(id).await?;
            eprintln!(
                "memory: {} released; it carries {}",
                s.id,
                if s.findings.is_empty() {
                    "nothing the scan found".to_string()
                } else {
                    s.findings.join(", ")
                }
            );
            println!("{}", serde_json::to_string_pretty(&s)?);
        }
        SourceOp::Sweep => {
            let dropped = m.sweep_sources(time::OffsetDateTime::now_utc()).await?;
            println!("{}", serde_json::to_string_pretty(&dropped)?);
        }
    }
    Ok(())
}
