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
        let outcome =
            dm_taps::run(memory.clone(), &path, &grant, credential.as_deref(), config).await?;
        println!("{}", serde_json::to_string_pretty(&outcome)?);
        let Some(secs) = every else {
            return Ok(());
        };
        eprintln!("memory: tap {name} runs again in {secs}s");
        tokio::time::sleep(std::time::Duration::from_secs(secs.max(1))).await;
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
