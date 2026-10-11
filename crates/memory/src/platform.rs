//! `amem platform`: what the platform's domain holds for the launcher, and what of it is
//! loaded. An artifact loads only from an active entry a person verified; the listing
//! says why each of the rest does not.

use crate::Fallible;
use clap::Subcommand;
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
    }
}
