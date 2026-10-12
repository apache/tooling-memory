//! What a run cost the host, beside the tokens the egress meters: the wall clock from
//! spawn to exit, and the CPU time and peak memory of the process tree, as the kernel
//! counted them when the run was reaped. The paper's price vector (§12) counts compute
//! time as a resource class of its own; this is where a run's comes from.
//!
//! `wait4` returns the usage of the child and of every descendant it waited for, so a
//! confined run counts the harness, its tools and the memory server inside the sandbox:
//! each level of the sandbox reaps its children. A process that outlives its parent and
//! is never reaped is not counted. Peak memory is the largest resident set any one
//! process of the tree reached, not their sum.

use serde::Serialize;
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// A run's cost to the host.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Resources {
    /// Spawn to exit, in milliseconds.
    pub wall_ms: u64,
    /// CPU time in user mode, the process tree's, in milliseconds.
    pub user_ms: u64,
    /// CPU time in the kernel on the tree's behalf, in milliseconds.
    pub sys_ms: u64,
    /// The largest resident set one process of the tree reached, in KiB.
    pub max_rss_kib: u64,
    /// Whether CPU and memory were counted; only the wall clock is, where the host has no
    /// `wait4`.
    pub measured: bool,
}

/// Spawn `cmd` and wait for it, measured. With `capture`, standard input is closed and
/// standard output read and returned; standard error goes where it was going. The exit
/// code is the process's own, or 1 when a signal ended it.
///
/// # Errors
///
/// When the process cannot be spawned, its output read, or it cannot be waited for.
pub fn run(cmd: &mut Command, capture: bool) -> std::io::Result<(i32, Vec<u8>, Resources)> {
    let started = Instant::now();
    if capture {
        cmd.stdin(Stdio::null()).stdout(Stdio::piped());
    }
    let mut child = cmd.spawn()?;
    let mut out = Vec::new();
    if let Some(mut s) = child.stdout.take() {
        s.read_to_end(&mut out)?;
    }
    let (code, mut r) = wait(child)?;
    r.wall_ms = millis(started.elapsed());
    Ok((code, out, r))
}

/// This process's own CPU time so far, user and system, in milliseconds: the launcher's
/// share of a run, the egress and the memory server it hosts in process among it.
/// Zero where the host cannot say.
#[must_use]
pub fn own_cpu_ms() -> (u64, u64) {
    #[cfg(unix)]
    {
        // SAFETY: rusage is plain data, for which all zeroes is a valid value.
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        // SAFETY: getrusage on this process, with a valid pointer to write into.
        if unsafe { libc::getrusage(libc::RUSAGE_SELF, &raw mut usage) } == 0 {
            return (timeval_ms(usage.ru_utime), timeval_ms(usage.ru_stime));
        }
    }
    (0, 0)
}

#[cfg(unix)]
fn wait(child: Child) -> std::io::Result<(i32, Resources)> {
    let pid = libc::pid_t::try_from(child.id()).map_err(std::io::Error::other)?;
    let mut status: libc::c_int = 0;
    // SAFETY: rusage is plain data, for which all zeroes is a valid value.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    loop {
        // SAFETY: wait4 on our own child, not yet reaped, with valid pointers.
        let r = unsafe { libc::wait4(pid, &raw mut status, 0, &raw mut usage) };
        if r == pid {
            break;
        }
        let e = std::io::Error::last_os_error();
        if e.kind() != std::io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
    // reaped above; dropping the handle does not wait again
    drop(child);
    let code = if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else {
        1
    };
    Ok((
        code,
        Resources {
            wall_ms: 0,
            user_ms: timeval_ms(usage.ru_utime),
            sys_ms: timeval_ms(usage.ru_stime),
            max_rss_kib: rss_kib(usage.ru_maxrss),
            measured: true,
        },
    ))
}

#[cfg(not(unix))]
fn wait(mut child: Child) -> std::io::Result<(i32, Resources)> {
    let status = child.wait()?;
    Ok((status.code().unwrap_or(1), Resources::default()))
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(unix)]
fn timeval_ms(t: libc::timeval) -> u64 {
    let secs = u64::try_from(t.tv_sec).unwrap_or(0);
    let micros = u64::try_from(t.tv_usec).unwrap_or(0);
    secs * 1000 + micros / 1000
}

/// `ru_maxrss` is in KiB on Linux and in bytes on macOS.
#[cfg(unix)]
fn rss_kib(raw: libc::c_long) -> u64 {
    let raw = u64::try_from(raw).unwrap_or(0);
    if cfg!(target_os = "macos") {
        raw / 1024
    } else {
        raw
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn a_run_is_measured_with_its_descendants_and_its_output() {
        // the work is a grandchild's, which the child waits for: it counts
        let mut cmd = Command::new("sh");
        cmd.args([
            "-c",
            "sh -c 'i=0; while [ $i -lt 200000 ]; do i=$((i+1)); done'; echo done; exit 3",
        ]);
        let (code, out, r) = run(&mut cmd, true).unwrap();
        assert_eq!(code, 3);
        assert_eq!(String::from_utf8_lossy(&out).trim(), "done");
        assert!(r.measured);
        assert!(r.user_ms + r.sys_ms > 0, "{r:?}");
        assert!(r.wall_ms >= r.user_ms / 4, "{r:?}");
        assert!(r.max_rss_kib > 0, "{r:?}");
    }

    #[test]
    fn a_signal_is_exit_one_and_the_launcher_has_cpu_of_its_own() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "kill -9 $$"]);
        let (code, out, r) = run(&mut cmd, false).unwrap();
        assert_eq!(code, 1);
        assert!(out.is_empty());
        assert!(r.measured);
        let (user, sys) = own_cpu_ms();
        assert!(user + sys < u64::MAX);
        assert!(Command::new("/nonexistent/binary").spawn().is_err());
        assert!(run(&mut Command::new("/nonexistent/binary"), false).is_err());
    }
}
