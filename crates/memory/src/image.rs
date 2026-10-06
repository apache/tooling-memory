//! The harness image on this host: unpacked once into the record's state, then pivoted
//! into by the namespace backend, so a Linux run is the same environment as a run in the
//! image under WSL containers.
//!
//! Unpacking goes through whichever runtime holds the image: `wslc` from a WSL
//! distribution, otherwise `podman` or `docker`. The result is a plain directory, which is
//! all the sandbox needs.

use dm_sandbox::Harness;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

type Fallible<T> = Result<T, Box<dyn std::error::Error>>;

/// Where an unpacked image lives under the record root.
#[must_use]
pub fn rootfs_dir(root: &Path) -> PathBuf {
    root.join("state/images/harness/rootfs")
}

/// Whether `name` is a program in a directory on the PATH: a file, and on Unix one that
/// may be run.
pub(crate) fn on_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| runnable(&d.join(name))))
}

#[cfg(unix)]
fn runnable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn runnable(p: &Path) -> bool {
    p.is_file()
}

/// The runtime to export from, by preference: the one that built the image here.
fn runtime() -> Option<&'static str> {
    ["wslc.exe", "wslc", "podman", "docker"]
        .into_iter()
        .find(|r| on_path(r))
}

fn run(cmd: &str, args: &[&str]) -> Fallible<String> {
    let out = Command::new(cmd).args(args).stdin(Stdio::null()).output()?;
    if !out.status.success() {
        return Err(format!(
            "{cmd} {}: {}",
            args.first().copied().unwrap_or_default(),
            String::from_utf8_lossy(&out.stderr).trim()
        )
        .into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Unpack `image` into the record's state, replacing what was there. Returns the rootfs.
///
/// # Errors
///
/// When no runtime is present, the image is not in it, or the export cannot be read.
pub fn unpack(root: &Path, image: &str) -> Fallible<PathBuf> {
    let rt = runtime()
        .ok_or("no container runtime found (wslc, podman or docker) to export the image from")?;
    let rootfs = rootfs_dir(root);
    let fresh = rootfs.with_extension("new");
    if fresh.exists() {
        std::fs::remove_dir_all(&fresh)?;
    }
    std::fs::create_dir_all(&fresh)?;
    eprintln!("memory: exporting {image} from {rt}");
    let id = run(rt, &["create", image, "true"])?;
    let id = id.lines().last().unwrap_or_default().trim().to_string();
    let result = (|| -> Fallible<()> {
        // export to a tar stream and unpack as this user: no device nodes, no ownership
        let mut export = Command::new(rt)
            .args(["export", &id])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()?;
        let stdout = export.stdout.take().ok_or("export has no stdout")?;
        let status = Command::new("tar")
            .args(["-x", "--no-same-owner", "--exclude=dev/*", "-C"])
            .arg(&fresh)
            .stdin(stdout)
            .status()?;
        let export_status = export.wait()?;
        if !status.success() || !export_status.success() {
            return Err("export or unpack failed".into());
        }
        Ok(())
    })();
    run(rt, &["remove", "-f", &id]).ok();
    result?;
    if rootfs.exists() {
        std::fs::remove_dir_all(&rootfs)?;
    }
    std::fs::rename(&fresh, &rootfs)?;
    std::fs::write(rootfs.with_extension("source"), format!("{image}\n"))?;
    eprintln!("memory: unpacked into {}", rootfs.display());
    Ok(rootfs)
}

/// The unpacked rootfs, if a usable one is there.
#[must_use]
pub fn present(root: &Path) -> Option<PathBuf> {
    let r = rootfs_dir(root);
    (r.join("usr/local/bin").is_dir() && r.join("opt/memory/memory").is_file()).then_some(r)
}

/// Whether the unpacked rootfs carries the harness. A `command` runs whatever it names,
/// so the image serves it as it is. The entry is checked, not its target: npm's are
/// links into the image's own tree.
#[must_use]
pub fn carries(rootfs: &Path, harness: Harness) -> bool {
    harness == Harness::Command
        || rootfs
            .join("usr/local/bin")
            .join(harness.binary())
            .symlink_metadata()
            .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn present_needs_the_harnesses_and_the_binary() {
        let dir = tempfile::tempdir().unwrap();
        assert!(present(dir.path()).is_none());
        let r = rootfs_dir(dir.path());
        std::fs::create_dir_all(r.join("usr/local/bin")).unwrap();
        assert!(present(dir.path()).is_none(), "no binary yet");
        std::fs::create_dir_all(r.join("opt/memory")).unwrap();
        std::fs::write(r.join("opt/memory/memory"), "").unwrap();
        assert_eq!(present(dir.path()), Some(r));
    }
}
