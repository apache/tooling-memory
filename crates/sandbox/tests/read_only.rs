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

/// Why a launch cannot make a read-only grant read-only below its submounts, as the
/// backend says it.
const NO_MOUNT_SETATTR: &str = "mount_setattr is not available here: it needs Linux 5.12 or \
                                later, and a seccomp profile that allows it";

/// Run inside the sandbox: try to write at the top of the grant and in the mount below
/// it, and say whether the mount below is nosuid.
#[test]
#[ignore = "the program the read-only test runs inside the sandbox"]
fn probe_writes() {
    for (name, dir) in [("top", INSIDE), ("submount", "/grant/sub")] {
        let file = Path::new(dir).join("written");
        say(name, std::fs::write(file, b"x"));
    }
    let sub = CString::new("/grant/sub").unwrap();
    // SAFETY: a NUL-terminated path and a zeroed statvfs for the kernel to fill.
    let flags = unsafe {
        let mut st: libc::statvfs = std::mem::zeroed();
        assert_eq!(libc::statvfs(sub.as_ptr(), &mut st), 0);
        st.f_flag
    };
    let nosuid = u8::from(flags & libc::ST_NOSUID != 0);
    println!("{}submount_nosuid {nosuid}", common::REPORT);
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
    if !common::confinable() {
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
    let want: BTreeMap<String, String> = [
        ("top", refused.as_str()),
        ("submount", refused.as_str()),
        ("submount_nosuid", "1"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    assert_eq!(common::report(&out.stdout), want, "{said}");
}

/// Run by the test below, under a seccomp filter that answers `mount_setattr` with
/// `ENOSYS`: say what [`dm_sandbox::linux::availability`] answers, and whether a launch
/// with read-only grants starts.
#[test]
#[ignore = "the host side of the mount_setattr test, under a filter that hides the call"]
fn probe_availability_without_mount_setattr() {
    let said = match dm_sandbox::linux::availability() {
        Ok(()) => "ok".to_string(),
        Err(why) => why,
    };
    println!("{}availability {said}", common::REPORT);
    let plan = common::plan("probe_writes", Vec::new(), BTreeMap::new());
    let launch = match dm_sandbox::linux::run_captured(&plan) {
        Ok(_) => "started",
        Err(_) => "refused",
    };
    println!("{}launch {launch}", common::REPORT);
}

/// `AUDIT_ARCH_*` of this target, which the filter below checks before the number.
#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: u32 = 0xC000_003E;
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: u32 = 0xC000_00B7;

fn bpf(code: u32, k: u32, jt: u8, jf: u8) -> libc::sock_filter {
    libc::sock_filter {
        code: u16::try_from(code).unwrap(),
        jt,
        jf,
        k,
    }
}

/// This binary's [`probe_availability_without_mount_setattr`], under a seccomp filter
/// that answers `mount_setattr` with `ENOSYS` and allows every other call, as a
/// container runtime's profile that does not know the call does.
fn without_mount_setattr() -> Command {
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args([
        "--exact",
        "probe_availability_without_mount_setattr",
        "--ignored",
        "--nocapture",
        "--test-threads=1",
    ]);
    let nr = u32::try_from(libc::SYS_mount_setattr).unwrap();
    let errno = libc::SECCOMP_RET_ERRNO | u32::try_from(libc::ENOSYS).unwrap();
    let program = [
        bpf(libc::BPF_LD | libc::BPF_W | libc::BPF_ABS, 4, 0, 0),
        bpf(
            libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K,
            AUDIT_ARCH,
            0,
            3,
        ),
        bpf(libc::BPF_LD | libc::BPF_W | libc::BPF_ABS, 0, 0, 0),
        bpf(libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K, nr, 0, 1),
        bpf(libc::BPF_RET | libc::BPF_K, errno, 0, 0),
        bpf(libc::BPF_RET | libc::BPF_K, libc::SECCOMP_RET_ALLOW, 0, 0),
    ];
    // SAFETY: between fork and exec the child makes two syscalls on a program built
    // before the fork, and allocates nothing.
    unsafe {
        cmd.pre_exec(move || {
            let prog = libc::sock_fprog {
                len: program.len() as u16,
                filter: program.as_ptr().cast_mut(),
            };
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
                || libc::syscall(libc::SYS_seccomp, libc::SECCOMP_SET_MODE_FILTER, 0, &prog) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd
}

#[test]
fn without_mount_setattr_availability_says_so_and_a_launch_does_not_start() {
    if !common::confinable() {
        return;
    }
    let out = without_mount_setattr().output().unwrap();
    let said = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{said}{stderr}");
    let want: BTreeMap<String, String> =
        [("availability", NO_MOUNT_SETATTR), ("launch", "refused")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
    assert_eq!(common::report(&out.stdout), want, "{said}{stderr}");
    assert!(stderr.contains(NO_MOUNT_SETATTR), "{stderr}");
}
