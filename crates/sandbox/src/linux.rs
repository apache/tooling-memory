//! The Linux backend: namespaces, natively and unprivileged.
//!
//! The child unshares user, mount, network, uts and ipc namespaces, maps itself to the
//! caller's uid, builds a new root on a tmpfs from the plan's bind mounts, pivots into it,
//! and execs the relay. The relay (`memory relay`) then unshares a pid namespace and
//! spawns the harness, mounting `/proc` from inside it, so the harness is pid 1 of its
//! own world and sees no other process.
//!
//! No network interface exists inside except loopback. The only way out is the unix
//! socket bind-mounted from the host, which the relay forwards to.

use crate::{Error, Mount, Plan};
use nix::mount::{mount, MsFlags};
use nix::sched::{unshare, CloneFlags};
use nix::unistd::{getgid, getuid, pivot_root, sethostname};
use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn confine(e: impl std::fmt::Display) -> Error {
    Error::Confine(e.to_string())
}

fn write_file(path: &str, text: &str) -> Result<(), Error> {
    std::fs::write(path, text).map_err(|e| confine(format!("{path}: {e}")))
}

/// Become root in a fresh user namespace, mapped to the caller outside.
fn enter_user_namespace(uid: u32, gid: u32) -> Result<(), Error> {
    unshare(
        CloneFlags::CLONE_NEWUSER
            | CloneFlags::CLONE_NEWNS
            | CloneFlags::CLONE_NEWNET
            | CloneFlags::CLONE_NEWUTS
            | CloneFlags::CLONE_NEWIPC,
    )
    .map_err(|e| confine(format!("unshare: {e}")))?;
    write_file("/proc/self/setgroups", "deny")?;
    write_file("/proc/self/uid_map", &format!("{uid} {uid} 1\n"))?;
    write_file("/proc/self/gid_map", &format!("{gid} {gid} 1\n"))?;
    Ok(())
}

fn bind(m: &Mount, root: &Path) -> Result<(), Error> {
    let target = root.join(m.target.strip_prefix("/").unwrap_or(&m.target));
    if m.source.is_dir() {
        std::fs::create_dir_all(&target)
            .map_err(|e| confine(format!("{}: {e}", target.display())))?;
    } else {
        if let Some(p) = target.parent() {
            std::fs::create_dir_all(p).map_err(|e| confine(format!("{}: {e}", p.display())))?;
        }
        if !target.exists() {
            std::fs::write(&target, b"")
                .map_err(|e| confine(format!("{}: {e}", target.display())))?;
        }
    }
    mount(
        Some(&m.source),
        &target,
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REC,
        None::<&str>,
    )
    .map_err(|e| {
        confine(format!(
            "bind {} -> {}: {e}",
            m.source.display(),
            target.display()
        ))
    })?;
    if m.read_only {
        mount(
            None::<&str>,
            &target,
            None::<&str>,
            MsFlags::MS_BIND | MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY | MsFlags::MS_NOSUID,
            None::<&str>,
        )
        .map_err(|e| confine(format!("remount ro {}: {e}", target.display())))?;
    }
    Ok(())
}

fn tmpfs(path: &Path, opts: &str) -> Result<(), Error> {
    std::fs::create_dir_all(path).map_err(|e| confine(format!("{}: {e}", path.display())))?;
    mount(
        Some("tmpfs"),
        path,
        Some("tmpfs"),
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
        Some(opts),
    )
    .map_err(|e| confine(format!("tmpfs {}: {e}", path.display())))
}

fn dev_nodes(root: &Path) -> Result<(), Error> {
    let dev = root.join("dev");
    tmpfs(&dev, "mode=755,size=1m")?;
    for n in ["null", "zero", "full", "random", "urandom", "tty"] {
        let src = PathBuf::from("/dev").join(n);
        if src.exists() {
            bind(
                &Mount {
                    source: src,
                    target: PathBuf::from("/dev").join(n),
                    read_only: false,
                },
                root,
            )?;
        }
    }
    // pseudo-terminals, so an interactive harness can open the one it was given
    let pts = PathBuf::from("/dev/pts");
    if pts.exists() {
        bind(
            &Mount {
                source: pts,
                target: PathBuf::from("/dev/pts"),
                read_only: false,
            },
            root,
        )?;
        std::os::unix::fs::symlink("pts/ptmx", dev.join("ptmx")).ok();
    }
    tmpfs(&dev.join("shm"), "mode=1777,size=64m")?;
    for (link, to) in [
        ("fd", "/proc/self/fd"),
        ("stdin", "/proc/self/fd/0"),
        ("stdout", "/proc/self/fd/1"),
        ("stderr", "/proc/self/fd/2"),
    ] {
        std::os::unix::fs::symlink(to, dev.join(link)).ok();
    }
    Ok(())
}

/// Bring the loopback interface up. A fresh network namespace has `lo` down, and the
/// relay listens on it.
fn loopback_up() -> Result<(), Error> {
    // SAFETY: a datagram socket and two ioctls on a zeroed, correctly sized ifreq whose
    // name is "lo"; the socket is closed before returning.
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            return Err(confine(format!(
                "socket: {}",
                std::io::Error::last_os_error()
            )));
        }
        let mut req: libc::ifreq = std::mem::zeroed();
        for (i, b) in b"lo".iter().enumerate() {
            req.ifr_name[i] = *b as libc::c_char;
        }
        let r = libc::ioctl(fd, libc::SIOCGIFFLAGS, &mut req);
        if r < 0 {
            libc::close(fd);
            return Err(confine(format!(
                "SIOCGIFFLAGS lo: {}",
                std::io::Error::last_os_error()
            )));
        }
        req.ifr_ifru.ifru_flags |= (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
        let r = libc::ioctl(fd, libc::SIOCSIFFLAGS, &req);
        libc::close(fd);
        if r < 0 {
            return Err(confine(format!(
                "SIOCSIFFLAGS lo: {}",
                std::io::Error::last_os_error()
            )));
        }
    }
    Ok(())
}

/// Build the root and pivot into it. Runs in the child, after `enter_user_namespace`.
fn build_root(plan: &Plan, root: &Path) -> Result<(), Error> {
    // make every mount private so nothing propagates back to the host
    mount(
        None::<&str>,
        "/",
        None::<&str>,
        MsFlags::MS_REC | MsFlags::MS_PRIVATE,
        None::<&str>,
    )
    .map_err(|e| confine(format!("make / private: {e}")))?;
    tmpfs(root, "mode=755")?;
    // /tmp and /dev first, so a plan mount under them (a working tree in /tmp, say) lands
    // on top rather than being hidden by them
    tmpfs(&root.join("tmp"), "mode=1777")?;
    dev_nodes(root)?;
    for m in &plan.mounts {
        bind(m, root)?;
    }
    // merged-usr: /bin -> usr/bin and so on, recreated so `#!/bin/sh` resolves inside
    for name in ["bin", "sbin", "lib", "lib64", "lib32"] {
        let host = Path::new("/").join(name);
        if let Ok(target) = std::fs::read_link(&host) {
            let inside = root.join(name);
            if !inside.exists() {
                std::os::unix::fs::symlink(&target, &inside)
                    .map_err(|e| confine(format!("symlink {}: {e}", inside.display())))?;
            }
        }
    }
    std::fs::create_dir_all(root.join("proc")).map_err(confine)?;
    std::fs::create_dir_all(root.join("run")).map_err(confine)?;
    let old = root.join(".old-root");
    std::fs::create_dir_all(&old).map_err(confine)?;
    pivot_root(root, &old).map_err(|e| confine(format!("pivot_root: {e}")))?;
    std::env::set_current_dir("/").map_err(confine)?;
    nix::mount::umount2("/.old-root", nix::mount::MntFlags::MNT_DETACH)
        .map_err(|e| confine(format!("umount old root: {e}")))?;
    std::fs::remove_dir("/.old-root").ok();
    std::env::set_current_dir(&plan.cwd)
        .map_err(|e| confine(format!("chdir {}: {e}", plan.cwd.display())))?;
    Ok(())
}

/// Run the plan: fork, confine the child, exec the relay. Returns the exit code.
///
/// # Errors
///
/// When the sandbox cannot be built or the relay cannot be started.
pub fn run(plan: &Plan) -> Result<i32, Error> {
    let (mut cmd, root) = command(plan)?;
    let status = cmd
        .status()
        .map_err(|e| confine(format!("spawn {}: {e}", plan.argv[0])))?;
    std::fs::remove_dir_all(&root).ok();
    Ok(status.code().unwrap_or(1))
}

/// As [`run`], with the harness's standard output captured; standard error is inherited.
///
/// # Errors
///
/// When the sandbox cannot be built or the relay cannot be started.
pub fn run_captured(plan: &Plan) -> Result<(i32, Vec<u8>), Error> {
    let (mut cmd, root) = command(plan)?;
    let out = cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .output()
        .map_err(|e| confine(format!("spawn {}: {e}", plan.argv[0])))?;
    std::fs::remove_dir_all(&root).ok();
    Ok((out.status.code().unwrap_or(1), out.stdout))
}

/// The command that builds the sandbox in `pre_exec` and execs the relay, and the
/// scratch directory it pivots into, for the caller to remove.
fn command(plan: &Plan) -> Result<(Command, PathBuf), Error> {
    let uid = getuid().as_raw();
    let gid = getgid().as_raw();
    let root = std::env::temp_dir().join(format!("memory-sandbox-{}", std::process::id()));
    std::fs::create_dir_all(&root).map_err(|e| confine(format!("{}: {e}", root.display())))?;

    let plan2 = plan.clone();
    let root2 = root.clone();
    let mut cmd = Command::new(&plan.argv[0]);
    cmd.args(&plan.argv[1..]);
    cmd.env_clear();
    for (k, v) in &plan.env {
        cmd.env(k, v);
    }
    cmd.current_dir(&plan.cwd);
    // SAFETY: everything in pre_exec is syscalls and small allocations in a fresh
    // single-threaded child; nothing touches the parent's state.
    unsafe {
        cmd.pre_exec(move || {
            let go = || -> Result<(), Error> {
                enter_user_namespace(uid, gid)?;
                sethostname(&plan2.hostname).map_err(|e| confine(format!("hostname: {e}")))?;
                loopback_up()?;
                build_root(&plan2, &root2)?;
                become_pid_one()?;
                Ok(())
            };
            go().map_err(|e| {
                // std carries only the raw errno back to the parent, so say what
                // failed here, where stderr is still the terminal
                eprintln!("memory: sandbox: {e}");
                std::io::Error::other(e.to_string())
            })
        });
    }
    Ok((cmd, root))
}

/// Enter a pid namespace and become its pid 1. Runs between fork and exec, while the
/// process still holds the user namespace's capabilities (exec of a non-root uid drops
/// them, which is why the relay cannot do this itself).
///
/// `unshare(CLONE_NEWPID)` only moves the caller's *children* into the new namespace, so
/// this forks once more: the parent waits for the child and exits with its status; the
/// child, now pid 1, mounts `/proc` for its namespace and goes on to exec the relay.
fn become_pid_one() -> Result<(), Error> {
    unshare(CloneFlags::CLONE_NEWPID).map_err(|e| confine(format!("unshare pid: {e}")))?;
    // SAFETY: fork between fork and exec in an already single-threaded child; the parent
    // branch only waits and exits, the child branch only mounts and returns to exec.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(confine(format!(
            "fork: {}",
            std::io::Error::last_os_error()
        )));
    }
    if pid > 0 {
        let mut status: libc::c_int = 0;
        // SAFETY: waitpid on our own child with a valid status pointer.
        let r = unsafe { libc::waitpid(pid, &mut status, 0) };
        let code = if r < 0 {
            1
        } else if libc::WIFEXITED(status) {
            libc::WEXITSTATUS(status)
        } else if libc::WIFSIGNALED(status) {
            128 + libc::WTERMSIG(status)
        } else {
            1
        };
        // SAFETY: exiting the intermediate process without running atexit handlers,
        // which belong to the parent that forked us.
        unsafe { libc::_exit(code) };
    }
    mount(
        Some("proc"),
        "/proc",
        Some("proc"),
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
        None::<&str>,
    )
    .map_err(|e| confine(format!("mount /proc: {e}")))?;
    Ok(())
}

/// What `memory relay` does inside: spawn the harness and wait for it. The relay is
/// already pid 1 of the harness's namespace, courtesy of [`run`].
///
/// # Errors
///
/// When the spawn fails.
pub fn spawn_in_pid_namespace(argv: &[OsString]) -> Result<i32, Error> {
    let status = Command::new(&argv[0])
        .args(&argv[1..])
        .status()
        .map_err(|e| confine(format!("spawn {}: {e}", argv[0].to_string_lossy())))?;
    Ok(status.code().unwrap_or(1))
}
