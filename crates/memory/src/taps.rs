//! `memory tap`: a connector installed under the root and run under its grant. The
//! component lives at `<root>/taps/<name>.wasm`, the grant in the grants file, and the
//! credential in the launcher's environment under the variable the grant names; the
//! tap gets the host's outbound hook and nothing else.

use dm_server::Memory;
use std::path::{Path, PathBuf};
use std::sync::Arc;

type Fallible<T> = Result<T, Box<dyn std::error::Error>>;

/// A tap's name becomes a file name under the root, so it is checked first: lower-case
/// letters, digits and hyphens, up to 64 characters.
pub fn valid_name(name: &str) -> bool {
    let ok_char = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-';
    !name.is_empty() && name.len() <= 64 && !name.starts_with('-') && name.chars().all(ok_char)
}

/// Where a tap's component lives under the root.
pub fn component_path(root: &Path, name: &str) -> Fallible<PathBuf> {
    if !valid_name(name) {
        return Err(
            format!("tap name {name:?}: lower-case letters, digits and hyphens only").into(),
        );
    }
    Ok(root.join("taps").join(format!("{name}.wasm")))
}

/// Copy a component under the root as `name`.
///
/// # Errors
///
/// A bad name, or a file that cannot be read or written.
pub fn install(root: &Path, name: &str, file: &Path) -> Fallible<PathBuf> {
    let dest = component_path(root, name)?;
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let bytes = std::fs::read(file).map_err(|e| format!("{}: {e}", file.display()))?;
    if !bytes.starts_with(b"\0asm") {
        return Err(format!("{} is not a WebAssembly binary", file.display()).into());
    }
    std::fs::write(&dest, bytes)?;
    Ok(dest)
}

/// Run a tap once, or every `every` seconds until stopped. Each run's outcome is printed
/// as JSON.
///
/// # Errors
///
/// No grant for the tap, no component installed, or a run that fails.
pub async fn run(
    root: &Path,
    name: &str,
    config: Option<&str>,
    every: Option<u64>,
) -> Fallible<()> {
    let path = component_path(root, name)?;
    let memory = Memory::open(root)?.with_author(format!("tap:{name}"));
    let grant =
        memory.grants().tap(name).cloned().ok_or_else(|| {
            format!("grants.toml has no [taps.{name}] on a domain the owner holds")
        })?;
    if !path.is_file() {
        return Err(format!(
            "no component installed as {name}; run: memory tap install {name} <file.wasm>"
        )
        .into());
    }
    let credential = grant
        .credential
        .as_deref()
        .and_then(|var| crate::present(std::env::var(var).ok()));
    if grant.credential.is_some() && credential.is_none() {
        eprintln!(
            "memory: {} is not set; the tap runs without a credential",
            grant.credential.as_deref().unwrap_or_default()
        );
    }
    let memory = Arc::new(memory);
    let config = config.unwrap_or("{}");
    loop {
        let run = dm_taps::run(
            memory.clone(),
            &path,
            &grant,
            credential.as_deref(),
            config,
            dm_taps::Limits::default(),
        )
        .await;
        match (run, every) {
            (Ok(outcome), _) => println!("{}", serde_json::to_string_pretty(&outcome)?),
            (Err(e), None) => return Err(e.into()),
            // on a schedule, one failed pull is reported and the next one still comes
            (Err(e), Some(_)) => eprintln!("memory: tap {name}: {e}"),
        }
        let Some(secs) = every else {
            return Ok(());
        };
        eprintln!("memory: tap {name} runs again in {secs}s");
        tokio::time::sleep(std::time::Duration::from_secs(secs.max(1))).await;
    }
}

/// The tool taps installed under the root, as the memory server calls them: each call
/// loads the tool's component and its grant, and runs it with the credential the grant
/// names, every request it makes decided by the policy. A tool whose grant names a
/// tool server is called there instead, through the egress; one whose grant says
/// `browser` is fetched by the owner's browser through the web launcher.
pub struct TapTools {
    root: std::path::PathBuf,
    /// The egress hosted calls leave through: the launcher's, or one made here on the
    /// first hosted call for a command that has none of its own.
    egress: std::sync::Mutex<Option<dm_egress::Egress>>,
}

impl TapTools {
    /// The tools under `root`. They run in the launcher's process, outside the sandbox,
    /// and every request they make is decided against the calling session's label.
    #[must_use]
    pub fn for_root(root: &Path) -> Arc<dyn dm_server::sources::Tools> {
        Arc::new(TapTools {
            root: root.to_path_buf(),
            egress: std::sync::Mutex::new(None),
        })
    }

    /// The tools under `root` for a launcher, whose hosted calls leave through its
    /// egress; see [`crate::hosted::tool_servers`] for what that egress carries.
    #[must_use]
    pub fn with_egress(
        root: &Path,
        egress: dm_egress::Egress,
    ) -> Arc<dyn dm_server::sources::Tools> {
        Arc::new(TapTools {
            root: root.to_path_buf(),
            egress: std::sync::Mutex::new(Some(egress)),
        })
    }

    /// The egress for a hosted call: the one given, else one over this root's tool
    /// servers and telemetry, made once.
    fn egress(&self, memory: &Arc<Memory>) -> Result<dm_egress::Egress, String> {
        let mut slot = self
            .egress
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(e) = slot.as_ref() {
            return Ok(e.clone());
        }
        let state = self.root.join("state");
        std::fs::create_dir_all(&state).map_err(|e| format!("{}: {e}", state.display()))?;
        let e = dm_egress::Egress::with_tool_servers(
            memory.clone(),
            std::collections::HashMap::new(),
            crate::hosted::tool_servers(memory.grants())?,
            &state.join("telemetry.db"),
        )
        .map_err(|e| e.to_string())?;
        *slot = Some(e.clone());
        Ok(e)
    }
}

/// Whether the arguments ask for the page rendered, which only the browser does.
fn wants_render(args: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(args).is_ok_and(|v| v["render"] == true)
}

/// How long one tool call may take, and the memory its component may grow to.
const TOOL_LIMITS: dm_taps::Limits = dm_taps::Limits {
    wall: std::time::Duration::from_secs(60),
    memory: 128 << 20,
};

impl dm_server::sources::Tools for TapTools {
    fn call(
        &self,
        memory: Arc<Memory>,
        tool: String,
        config: String,
        args: String,
    ) -> dm_server::sources::Answer<'static> {
        let root = self.root.clone();
        let grant = memory.grants().taps.get(&tool).cloned();
        // hosted elsewhere: the tool server runs it, this side decides what it reached
        let egress = match &grant {
            Some(g) if g.hosted.is_some() => Some(self.egress(&memory)),
            _ => None,
        };
        Box::pin(async move {
            let grant = grant.ok_or_else(|| format!("grants.toml has no [taps.{tool}]"))?;
            // the owner's browser: it fetches, this side decides before and after
            if grant.browser {
                return crate::browser::call(memory, &tool, &config, &args).await;
            }
            if wants_render(&args) {
                return Err(format!(
                    "render: only a fetch the owner's browser runs renders a page; [taps.{tool}] does not say browser = true"
                ));
            }
            if let Some(egress) = egress {
                return crate::hosted::call(&egress?, memory, &tool, &config, &args).await;
            }
            let credential = grant
                .credential
                .as_deref()
                .and_then(|var| crate::present(std::env::var(var).ok()));
            let path = component_path(&root, &tool).map_err(|e| e.to_string())?;
            if !path.is_file() {
                return Err(format!(
                    "no component installed as {tool}; the owner runs: amem tap install {tool} <file.wasm>"
                ));
            }
            let call = dm_taps::tool::Call {
                tool: &tool,
                grant: &grant,
                credential: credential.as_deref(),
                config: &config,
                args: &args,
            };
            dm_taps::tool::call(memory, &path, call, TOOL_LIMITS)
                .await
                .map_err(|e| e.to_string())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_checked_before_they_become_paths() {
        assert!(valid_name("github-issues"));
        assert!(valid_name("a1"));
        assert!(!valid_name(""));
        assert!(!valid_name("-x"));
        assert!(!valid_name("Bad Name"));
        assert!(!valid_name("../etc"));
        assert!(!valid_name(&"a".repeat(65)));
        let dir = tempfile::tempdir().unwrap();
        assert!(component_path(dir.path(), "a/b").is_err());
        assert_eq!(
            component_path(dir.path(), "x").unwrap(),
            dir.path().join("taps/x.wasm")
        );
        let not_wasm = dir.path().join("f.txt");
        std::fs::write(&not_wasm, "hello").unwrap();
        assert!(install(dir.path(), "x", &not_wasm)
            .unwrap_err()
            .to_string()
            .contains("not a WebAssembly"));
        let wasm = dir.path().join("f.wasm");
        std::fs::write(&wasm, b"\0asm\x01\0\0\0").unwrap();
        let dest = install(dir.path(), "x", &wasm).unwrap();
        assert!(dest.is_file());
    }
}
