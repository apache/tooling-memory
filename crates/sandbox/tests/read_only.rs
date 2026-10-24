//! A read-only grant of the namespaces backend is read-only all the way down: a mount
//! the host has under the granted directory is read-only inside as well.

// An integration test is test code; a panic here is the failure being reported.
#![allow(clippy::unwrap_used)]
#![cfg(target_os = "linux")]

mod common;

use common::say;
use dm_sandbox::Mount;
use std::collections::BTreeMap;
use std::ffi::{CStr, CString};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The host directory granted read-only, in the probes' environment.
const GRANT: &str = "MEMORY_TEST_GRANT";
/// Where the grant appears inside.
const INSIDE: &str = "/grant";

/// Run inside the sandbox: try to write at the top of the grant and in the mount below it.
#[test]
#[ignore = "the program the read-only test runs inside the sandbox"]
fn probe_writes() {
    for (name, dir) in [("top", INSIDE), ("submount", "/grant/sub")] {
        let file = Path::new(dir).join("written");
        say(name, std::fs::write(file, b"x"));
    }
}

/// Run by the test below, in a mount namespace of its own where a tmpfs is mounted under
/// the grant: run [`probe_writes`] in the sandbox, and pass on what it reports.
#[test]
#[ignore = "the host side of the read-only test, in a namespace with a submount"]
fn probe_sandbox_over_a_submount() {
    let Some(grant) = std::env::var_os(GRANT) else {
        return;
    };
    let mounts = vec![Mount {
        source: PathBuf::from(grant),
        target: PathBuf::from(INSIDE),
        read_only: true,
    }];
    let plan = common::plan("probe_writes", mounts, BTreeMap::new());
    let (code, out, _) = dm_sandbox::linux::run_captured(&plan).unwrap();
    println!("{}", String::from_utf8_lossy(&out));
    assert_eq!(code, 0);
}

/// Write `text` to `path` with nothing but syscalls, as a child between fork and exec may.
fn write_all(path: &CStr, text: &[u8]) -> std::io::Result<()> {
    // SAFETY: a NUL-terminated path, a buffer with its own length, and a close of the
    // descriptor just opened.
    unsafe {
        let fd = libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let n = libc::write(fd, text.as_ptr().cast(), text.len());
        let e = std::io::Error::last_os_error();
        libc::close(fd);
        if n < 0 {
            return Err(e);
        }
    }
    Ok(())
}

/// This binary's [`probe_sandbox_over_a_submount`], in a user and mount namespace of its
/// own, mapped to the caller, with a tmpfs mounted at `sub`. The tmpfs is the caller's,
/// so only the mount's read-only flag can refuse a write in it.
fn over_a_submount(grant: &Path, sub: &Path) -> Command {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args([
        "--exact",
        "probe_sandbox_over_a_submount",
        "--ignored",
        "--nocapture",
        "--test-threads=1",
    ]);
    cmd.env(GRANT, grant);
    // SAFETY: getuid and getgid cannot fail.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let uid_map = format!("{uid} {uid} 1\n");
    let gid_map = format!("{gid} {gid} 1\n");
    let sub = CString::new(sub.as_os_str().as_bytes()).unwrap();
    // SAFETY: between fork and exec the child makes syscalls on buffers made before the
    // fork, and allocates nothing.
    unsafe {
        cmd.pre_exec(move || {
            if libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            write_all(c"/proc/self/setgroups", b"deny")?;
            write_all(c"/proc/self/uid_map", uid_map.as_bytes())?;
            write_all(c"/proc/self/gid_map", gid_map.as_bytes())?;
            let none = std::ptr::null();
            let private = libc::MS_REC | libc::MS_PRIVATE;
            if libc::mount(none, c"/".as_ptr(), none, private, none.cast()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            let tmpfs = c"tmpfs".as_ptr();
            if libc::mount(tmpfs, sub.as_ptr(), tmpfs, 0, none.cast()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd
}

#[test]
fn a_mount_under_a_read_only_grant_is_read_only_inside() {
    if let Err(why) = dm_sandbox::linux::availability() {
        eprintln!("skipping: {why}");
        return;
    }
    let grant = tempfile::tempdir().unwrap();
    let sub = grant.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    let out = over_a_submount(grant.path(), &sub).output().unwrap();
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{said}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let refused = libc::EROFS.to_string();
    let want: BTreeMap<String, String> = [("top", &refused), ("submount", &refused)]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();
    assert_eq!(common::report(&out.stdout), want, "{said}");
}
