//! The Linux backend: namespaces, natively and unprivileged.
//!
//! The child unshares user, mount, network, uts and ipc namespaces, maps itself to the
//! caller's uid, builds a new root on a tmpfs from the plan's bind mounts, pivots into it,
//! and execs the relay. The relay (`memory relay`) then unshares a pid namespace and
//! spawns the harness, mounting `/proc` from inside it, so the harness is pid 1 of its
//! own world and sees no other process.
//!
//! No network interface exists inside except loopback. The only way out is the unix
//! socket bind-mounted from the host, which the relay forwards to. The last step before
//! the exec sets `no_new_privs` and installs a seccomp filter that refuses
//! what a network namespace does not cover: vsock and io_uring.

mod seccomp;

use crate::{Error, Mount, Plan, Resources};
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

/// Why this host will not let an unprivileged process confine anything, in a sentence
/// that names the fix.
fn restricted_why() -> String {
    let what = "this host will not let an unprivileged process enter a user namespace";
    let apparmor =
        std::fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
            .is_ok_and(|v| v.trim() == "1");
    if apparmor {
        format!(
            "{what}: kernel.apparmor_restrict_unprivileged_userns is 1, so AppArmor moves a \
             binary that has no profile of its own into one without CAP_SYS_ADMIN. Give amem a \
             profile that allows it: packaging/apparmor/amem"
        )
    } else {
        format!("{what}; --unconfined runs the harness without a sandbox")
    }
}

/// Whether this host lets an unprivileged process build the sandbox at all: can it
/// create a user namespace and map itself into it?
///
/// Probed in a child, because the answer is a namespace the asking process would
/// otherwise be stuck in, and by calling [`enter_user_namespace`] itself rather than a
/// copy of it: a probe that unshares a different set of namespaces, or writes the maps
/// in a different order, can disagree with the launch it is meant to predict, and a
/// check that says a working host is broken is worse than no check. The first cut of
/// this did exactly that.
///
/// A sentence rather than a flag, since what to do differs: a kernel without user
/// namespaces is one thing, Ubuntu's AppArmor moving an unprofiled binary into a profile
/// without CAP_SYS_ADMIN is another, and the second is fixed by installing a profile
/// rather than by giving up on confinement.
///
/// # Errors
///
/// Why not, in a sentence `amem doctor` and the launch line can carry.
pub fn availability() -> Result<(), String> {
    // SAFETY: fork in a process that only waits for the child; the child makes the same
    // syscalls a launch does and _exits without running atexit handlers, which belong to
    // the parent.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(format!("fork: {}", std::io::Error::last_os_error()));
    }
    if pid == 0 {
        let ok = enter_user_namespace(getuid().as_raw(), getgid().as_raw()).is_ok();
        // SAFETY: exiting the probe child without running the parent's atexit handlers.
        unsafe { libc::_exit(i32::from(!ok)) };
    }
    let mut status: libc::c_int = 0;
    // SAFETY: waitpid on our own child with a valid status pointer.
    if unsafe { libc::waitpid(pid, &mut status, 0) } < 0 {
        return Err(format!("waitpid: {}", std::io::Error::last_os_error()));
    }
    if !libc::WIFEXITED(status) {
        return Err("the user-namespace probe did not exit normally".to_string());
    }
    if libc::WEXITSTATUS(status) == 0 {
        Ok(())
    } else {
        Err(restricted_why())
    }
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
            MsFlags::MS_BIND
                | MsFlags::MS_REMOUNT
                | MsFlags::MS_RDONLY
                | MsFlags::MS_NOSUID
                | kept_flags(&target),
            None::<&str>,
        )
        .map_err(|e| confine(format!("remount ro {}: {e}", target.display())))?;
    }
    Ok(())
}

/// The restricting flags a mount already carries, to carry into its read-only remount.
/// A mount made outside the user namespace, a FUSE mount the launcher serves say, has
/// its `nodev`, `noatime` and the like locked by the kernel: a remount that leaves one
/// out asks to lift it and is refused, so they are read back and kept.
fn kept_flags(target: &Path) -> MsFlags {
    use nix::sys::statvfs::{statvfs, FsFlags};
    let Ok(st) = statvfs(target) else {
        return MsFlags::empty();
    };
    let have = st.flags();
    let pairs = [
        (FsFlags::ST_NOSUID, MsFlags::MS_NOSUID),
        (FsFlags::ST_NODEV, MsFlags::MS_NODEV),
        (FsFlags::ST_NOEXEC, MsFlags::MS_NOEXEC),
        (FsFlags::ST_NOATIME, MsFlags::MS_NOATIME),
        (FsFlags::ST_NODIRATIME, MsFlags::MS_NODIRATIME),
        (FsFlags::ST_RELATIME, MsFlags::MS_RELATIME),
    ];
    pairs
        .into_iter()
        .filter(|(st, _)| have.contains(*st))
        .fold(MsFlags::empty(), |acc, (_, ms)| acc | ms)
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
    // merged-usr: /bin -> usr/bin and so on, recreated so `#!/bin/sh` resolves inside;
    // the plan took them from the rootfs it runs in
    for (name, target) in &plan.links {
        let inside = root.join(name);
        if !inside.exists() {
            std::os::unix::fs::symlink(target, &inside)
                .map_err(|e| confine(format!("symlink {}: {e}", inside.display())))?;
        }
    }
    std::fs::create_dir_all(root.join("proc")).map_err(confine)?;
    // The procfs for the harness's pid namespace, mounted here rather than over /proc
    // after the pivot. `mount -t proc` from a non-initial user namespace is refused
    // unless a fully visible procfs already exists in this mount namespace:
    // fs/proc/root.c, mount_too_revealing(). The host's is still visible at this point
    // and satisfies that; after pivot_root detaches the old root, nothing does, and the
    // mount fails with EPERM on kernels that enforce the check (seen on 6.17).
    //
    // Binding the host's procfs instead is not an option: the kernel refuses to clone
    // it into the new namespace at all, recursively or not, with EINVAL.
    //
    // This is why the pid namespace is entered before the root is built: the mount
    // shows the pid namespace it was made in, so it has to come after the fork.
    mount(
        Some("proc"),
        &root.join("proc"),
        Some("proc"),
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
        None::<&str>,
    )
    .map_err(|e| confine(format!("mount proc: {e}")))?;
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

/// Run the plan: fork, confine the child, exec the relay. Returns the exit code and what
/// the confined tree cost the host.
///
/// # Errors
///
/// When the sandbox cannot be built or the relay cannot be started.
pub fn run(plan: &Plan) -> Result<(i32, Resources), Error> {
    let (code, _, r) = measured(plan, false)?;
    Ok((code, r))
}

/// As [`run`], with the harness's standard output captured; standard error is inherited.
///
/// # Errors
///
/// When the sandbox cannot be built or the relay cannot be started.
pub fn run_captured(plan: &Plan) -> Result<(i32, Vec<u8>, Resources), Error> {
    measured(plan, true)
}

fn measured(plan: &Plan, capture: bool) -> Result<(i32, Vec<u8>, Resources), Error> {
    let (mut cmd, root) = command(plan)?;
    let ran = crate::measure::run(&mut cmd, capture)
        .map_err(|e| confine(format!("spawn {}: {e}", plan.argv[0])));
    std::fs::remove_dir_all(&root).ok();
    ran
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
    // compiled here, since compiling allocates and installing it does not
    let filter = seccomp::program()?;
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
                // before build_root: it mounts this namespace's procfs, which must be
                // made after the fork that enters the namespace and before pivot_root
                // takes the host's procfs out of view
                become_pid_one()?;
                build_root(&plan2, &root2)?;
                Ok(())
            };
            go().map_err(|e| {
                // std carries only the raw errno back to the parent, so say what
                // failed here, where stderr is still the terminal
                eprintln!("memory: sandbox: {e}");
                std::io::Error::other(e.to_string())
            })?;
            // last, so the setup above may make calls the filter refuses
            seccomp::install(&filter).inspect_err(|_| {
                // a static message, which needs no allocation between fork and exec
                let say = b"memory: sandbox: cannot install the seccomp filter\n";
                libc::write(2, say.as_ptr().cast(), say.len());
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
/// child, now pid 1, goes on to build the root and exec the relay.
///
/// Called before [`build_root`], which mounts the procfs for this namespace while the
/// host's is still visible. A procfs mount shows the pid namespace it was made in, so it
/// has to happen after this fork, and the visibility the kernel demands of it is gone
/// once `pivot_root` has detached the old root -- which leaves only the window between
/// the two.
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
