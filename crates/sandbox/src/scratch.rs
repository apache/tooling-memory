//! The directory a Linux sandbox builds its root in: `memory-sandbox-<pid>` in the
//! temporary directory, named for the launcher and locked by it for as long as the run
//! lasts. The launcher removes its own when the run ends; one a killed launcher left is
//! removed by the next launcher to start a run, once nothing holds it.

use crate::Error;
use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};
use nix::sys::signal::kill;
use nix::unistd::{getuid, Pid};
use std::fs::File;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

/// What every scratch directory's name starts with; the launcher's pid follows.
const PREFIX: &str = "memory-sandbox-";

/// This launcher's scratch directory, locked while it is held and removed when it is
/// dropped.
pub(crate) struct Scratch {
    path: PathBuf,
    _lock: Flock<File>,
}

impl Scratch {
    /// Sweep what killed launchers left in `tmp`, then make and lock this launcher's own.
    ///
    /// # Errors
    ///
    /// When the directory cannot be made or opened, or another process holds its lock.
    pub(crate) fn make(tmp: &Path) -> Result<Scratch, Error> {
        sweep(tmp);
        let path = dir(tmp);
        let failed = |e: &dyn std::fmt::Display| Error::Confine(format!("{}: {e}", path.display()));
        std::fs::create_dir_all(&path).map_err(|e| failed(&e))?;
        let dir = open_dir(&path).map_err(|e| failed(&e))?;
        let lock = Flock::lock(dir, FlockArg::LockExclusiveNonblock)
            .map_err(|(_, e)| failed(&format!("held by another process: {e}")))?;
        if !still_at(&path, &lock) {
            return Err(failed(&"removed or replaced while it was being locked"));
        }
        Ok(Scratch { path, _lock: lock })
    }

    /// Where the root is built.
    #[cfg(test)]
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.path).ok();
    }
}

/// This launcher's scratch directory in `tmp`.
pub(crate) fn dir(tmp: &Path) -> PathBuf {
    tmp.join(format!("{PREFIX}{}", std::process::id()))
}

/// The directory at `path` itself, never what a link there points to.
fn open_dir(path: &Path) -> std::io::Result<File> {
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
}

/// Whether the directory at `path` is still the one `dir` has open: a sweep may remove
/// it, and another make it again, between the open and the lock.
fn still_at(path: &Path, dir: &File) -> bool {
    match (std::fs::symlink_metadata(path), dir.metadata()) {
        (Ok(at), Ok(held)) => at.dev() == held.dev() && at.ino() == held.ino(),
        _ => false,
    }
}

/// The pid a scratch directory's name carries, when the name is one this module makes.
fn pid_of(name: &str) -> Option<i32> {
    let digits = name.strip_prefix(PREFIX)?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok().filter(|p| *p > 0)
}

/// Whether a process with this pid is running. One that is, whoever's, or one that
/// cannot be told, counts as running: a live launcher's directory is never removed.
fn running(pid: i32) -> bool {
    !matches!(kill(Pid::from_raw(pid), None), Err(Errno::ESRCH))
}

/// Remove the scratch directories killed launchers left in `tmp`. One goes only when its
/// name is one this module makes, it is a directory of this user's and not a link, the
/// launcher its name carries is not running, nothing holds its lock, and it is empty, as
/// the host sees a sandbox's root once its mount namespace is gone. Anything else in
/// `tmp` is left alone. Returns the directories removed.
pub(crate) fn sweep(tmp: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(tmp) else {
        return Vec::new();
    };
    let own = i32::try_from(std::process::id()).unwrap_or(0);
    let me = getuid().as_raw();
    entries
        .flatten()
        .filter_map(|e| {
            let pid = pid_of(e.file_name().to_str()?)?;
            if pid == own || running(pid) {
                return None;
            }
            let path = e.path();
            let dir = open_dir(&path).ok()?;
            if dir.metadata().ok()?.uid() != me {
                return None;
            }
            let held = Flock::lock(dir, FlockArg::LockExclusiveNonblock).ok()?;
            if !still_at(&path, &held) {
                return None;
            }
            std::fs::remove_dir(&path).ok()?;
            Some(path)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A process that runs until it is killed. By absolute path: other tests change PATH.
    fn waiting() -> std::process::Command {
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.args(["-c", "read x"])
            .stdin(std::process::Stdio::piped());
        cmd
    }

    /// A pid no process has: one that has been reaped.
    fn dead_pid() -> i32 {
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", ":"])
            .spawn()
            .unwrap();
        let pid = i32::try_from(child.id()).unwrap();
        child.wait().unwrap();
        assert!(!running(pid));
        pid
    }

    #[test]
    fn a_name_is_a_scratch_directory_s_only_when_this_module_makes_it() {
        assert_eq!(pid_of("memory-sandbox-4242"), Some(4242));
        for name in [
            "memory-sandbox-",
            "memory-sandbox-0",
            "memory-sandbox-+42",
            "memory-sandbox-42x",
            "memory-sandbox-42/..",
            "memory-sandbox-99999999999",
            "other-4242",
            "xmemory-sandbox-4242",
        ] {
            assert_eq!(pid_of(name), None, "{name}");
        }
    }

    #[test]
    fn a_killed_launcher_s_directory_is_swept_and_nothing_a_live_one_holds() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let dead = dead_pid();
        let stale = tmp.path().join(format!("{PREFIX}{dead}"));
        std::fs::create_dir(&stale).unwrap();
        // a live launcher: its pid runs
        let mut live_child = waiting().spawn().unwrap();
        let live = tmp.path().join(format!("{PREFIX}{}", live_child.id()));
        std::fs::create_dir(&live).unwrap();
        // a dead pid's name, but its lock is held: a launcher in another pid namespace
        let held_pid = dead_pid();
        let held = tmp.path().join(format!("{PREFIX}{held_pid}"));
        std::fs::create_dir(&held).unwrap();
        let _lock = Flock::lock(open_dir(&held).unwrap(), FlockArg::LockExclusiveNonblock)
            .map_err(|(_, e)| e)
            .unwrap();
        // a dead pid's name on a link to a directory elsewhere, and on a directory with
        // something in it
        let linked_pid = dead_pid();
        let linked = tmp.path().join(format!("{PREFIX}{linked_pid}"));
        std::os::unix::fs::symlink(outside.path(), &linked).unwrap();
        let full_pid = dead_pid();
        let full = tmp.path().join(format!("{PREFIX}{full_pid}"));
        std::fs::create_dir(&full).unwrap();
        std::fs::write(full.join("kept"), "x").unwrap();
        // a name of another shape
        let other = tmp.path().join(format!("memory-sandbox-{dead}-x"));
        std::fs::create_dir(&other).unwrap();

        let swept = sweep(tmp.path());

        live_child.kill().unwrap();
        live_child.wait().unwrap();
        assert_eq!(swept, vec![stale.clone()]);
        assert!(!stale.exists());
        for kept in [&live, &held, &full, &other] {
            assert!(kept.is_dir(), "{}", kept.display());
        }
        assert!(linked.is_symlink());
        assert!(outside.path().is_dir());
        assert!(full.join("kept").is_file());
    }

    #[test]
    fn a_directory_made_again_after_it_was_opened_is_not_the_one_held() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(format!("{PREFIX}1"));
        std::fs::create_dir(&path).unwrap();
        let held = open_dir(&path).unwrap();
        assert!(still_at(&path, &held));
        // made while the first still exists, so it cannot be given the same inode
        let other = tmp.path().join("other");
        std::fs::create_dir(&other).unwrap();
        std::fs::remove_dir(&path).unwrap();
        std::fs::rename(&other, &path).unwrap();
        assert!(
            !still_at(&path, &held),
            "a sweep and another launcher came between"
        );
    }

    #[test]
    fn a_launcher_s_own_directory_is_locked_while_held_and_gone_when_dropped() {
        let tmp = tempfile::tempdir().unwrap();
        let scratch = Scratch::make(tmp.path()).unwrap();
        let path = scratch.path().to_path_buf();
        assert_eq!(
            path.file_name().unwrap().to_str().unwrap(),
            format!("{PREFIX}{}", std::process::id())
        );
        assert!(Flock::lock(open_dir(&path).unwrap(), FlockArg::LockExclusiveNonblock).is_err());
        drop(scratch);
        assert!(!path.exists());
    }
}
