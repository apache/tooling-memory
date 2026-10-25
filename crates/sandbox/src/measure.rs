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
use std::ffi::OsStr;
use std::io::Read;
use std::process::{Child, Command, ExitStatus, Stdio};
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
/// code is as [`exit_code`] gives it.
///
/// # Errors
///
/// When the process cannot be spawned, its output read, or it cannot be waited for.
pub fn run(cmd: &mut Command, capture: bool) -> std::io::Result<(i32, Vec<u8>, Resources)> {
    run_as(cmd, capture, false)
}

/// As [`run`], with the process in a process group of its own, which
/// [`crate::stop::stop`] kills whole with what the run left in the background, when this
/// process has no controlling terminal. With one, the process stays in the terminal's
/// foreground group: in a group of its own it would be stopped by the first touch of
/// the terminal, a password prompt or `stty`.
///
/// # Errors
///
/// As [`run`].
pub fn run_grouped(cmd: &mut Command, capture: bool) -> std::io::Result<(i32, Vec<u8>, Resources)> {
    #[cfg(unix)]
    if !crate::stop::has_a_terminal() {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
        return run_as(cmd, capture, true);
    }
    run_as(cmd, capture, false)
}

/// How long a stopped run's output is read for once the run itself has gone: what it
/// left in the background, and did not die with it, may hold the output open.
const GRACE: Duration = Duration::from_secs(2);

fn run_as(
    cmd: &mut Command,
    capture: bool,
    group: bool,
) -> std::io::Result<(i32, Vec<u8>, Resources)> {
    let started = Instant::now();
    if capture {
        cmd.stdin(Stdio::null()).stdout(Stdio::piped());
    }
    let mut child = cmd.spawn()?;
    let stoppable = crate::stop::register(&child, group);
    // read on a thread of its own, so the run is waited for while its output is read
    let read = child.stdout.take().map(|mut s| {
        let out = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (done, ended) = std::sync::mpsc::channel();
        let into = out.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            let r = loop {
                match s.read(&mut buf) {
                    Ok(0) => break Ok(()),
                    Ok(n) => lock(&into).extend_from_slice(&buf[..n]),
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) => break Err(e),
                }
            };
            done.send(r).ok();
        });
        (out, ended)
    });
    exited(&child);
    // whether a stop had been sent by the time it exited: then a SIGKILL was the stop's
    let stopped = crate::stop::stopping();
    // read to the end while the exited child is not yet reaped: a stop meanwhile still
    // kills its process group, what it left in the background, and its number is
    // given to no other process or group while it is unreaped
    let mut out = Vec::new();
    let mut read_failed = Ok(());
    if let Some((buf, ended)) = read {
        read_failed = read_until_ended(&ended);
        out = std::mem::take(&mut *lock(&buf));
    }
    // no longer stopped once it is reaped, after which its numbers may be given again
    drop(stoppable);
    let (code, mut r) = wait(child, cmd.get_program(), stopped)?;
    read_failed?;
    r.wall_ms = millis(started.elapsed());
    Ok((code, out, r))
}

fn lock(m: &std::sync::Mutex<Vec<u8>>) -> std::sync::MutexGuard<'_, Vec<u8>> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Wait for the reader to reach the end of the output, or, once the launcher is told
/// to stop, for [`GRACE`] at most: what the run left in the background may hold it open.
fn read_until_ended(ended: &std::sync::mpsc::Receiver<std::io::Result<()>>) -> std::io::Result<()> {
    let mut since: Option<Instant> = None;
    loop {
        match ended.recv_timeout(Duration::from_millis(50)) {
            Ok(r) => return r,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if crate::stop::stopping() {
                    let at = *since.get_or_insert_with(Instant::now);
                    if at.elapsed() > GRACE {
                        return Ok(());
                    }
                }
            }
        }
    }
}

/// Wait for `child` to exit, without reaping it.
#[cfg(unix)]
fn exited(child: &Child) {
    let pid: libc::id_t = child.id();
    loop {
        // SAFETY: siginfo_t is plain data, for which all zeroes is a valid value.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: waitid on our own child, not yet reaped, leaving it unreaped (WNOWAIT),
        // with a valid pointer to write into.
        let r = unsafe {
            libc::waitid(
                libc::P_PID,
                pid,
                &raw mut info,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if r == 0 || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return;
        }
    }
}

#[cfg(not(unix))]
fn exited(_child: &Child) {}

/// The code a process ended with, as a shell gives it: its own exit code, or 128 plus
/// the number of the signal that killed it. A death by signal is said on standard error,
/// naming `program`, since the code alone does not tell it from an exit; SIGINT and
/// SIGPIPE are not, as a shell does not say them, nor a SIGKILL once
/// [`crate::stop::stop`] has sent it.
#[must_use]
pub fn exit_code(status: ExitStatus, program: &OsStr) -> i32 {
    exit_code_as(status, program, crate::stop::stopping())
}

/// As [`exit_code`], with `stopped` whether the stop had been sent when the process
/// exited: a SIGKILL from before it, an out-of-memory kill say, is said.
fn exit_code_as(status: ExitStatus, program: &OsStr, stopped: bool) -> i32 {
    #[cfg(not(unix))]
    let _ = (program, stopped);
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            let stopped = signal == libc::SIGKILL && stopped;
            if is_said(signal) && !stopped {
                let core = if status.core_dumped() {
                    ", core dumped"
                } else {
                    ""
                };
                let name = signal_name(signal).map_or_else(
                    || format!("signal {signal}"),
                    |name| format!("{name} (signal {signal})"),
                );
                let line = format!(
                    "memory: {} was killed by {name}{core}\n",
                    program.to_string_lossy()
                );
                // one write, so what other threads print does not land inside the line
                let _ = std::io::stderr().write_all(line.as_bytes());
            }
            return 128 + signal;
        }
    }
    status.code().unwrap_or(1)
}

/// Whether a death by this signal is said: not SIGINT, which the terminal sent, nor
/// SIGPIPE, which a reader that stopped reading caused.
#[cfg(unix)]
pub(crate) fn is_said(signal: i32) -> bool {
    !matches!(signal, libc::SIGINT | libc::SIGPIPE)
}

/// A signal's name, where it has one here.
#[cfg(unix)]
pub(crate) fn signal_name(signal: i32) -> Option<&'static str> {
    Some(match signal {
        libc::SIGHUP => "SIGHUP",
        libc::SIGINT => "SIGINT",
        libc::SIGQUIT => "SIGQUIT",
        libc::SIGILL => "SIGILL",
        libc::SIGTRAP => "SIGTRAP",
        libc::SIGABRT => "SIGABRT",
        libc::SIGBUS => "SIGBUS",
        libc::SIGFPE => "SIGFPE",
        libc::SIGKILL => "SIGKILL",
        libc::SIGUSR1 => "SIGUSR1",
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGUSR2 => "SIGUSR2",
        libc::SIGPIPE => "SIGPIPE",
        libc::SIGALRM => "SIGALRM",
        libc::SIGTERM => "SIGTERM",
        libc::SIGXCPU => "SIGXCPU",
        libc::SIGXFSZ => "SIGXFSZ",
        libc::SIGVTALRM => "SIGVTALRM",
        libc::SIGPROF => "SIGPROF",
        libc::SIGSYS => "SIGSYS",
        _ => return None,
    })
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
fn wait(child: Child, program: &OsStr, stopped: bool) -> std::io::Result<(i32, Resources)> {
    use std::os::unix::process::ExitStatusExt;
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
    Ok((
        exit_code_as(ExitStatus::from_raw(status), program, stopped),
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
fn wait(mut child: Child, program: &OsStr, stopped: bool) -> std::io::Result<(i32, Resources)> {
    let status = child.wait()?;
    Ok((exit_code_as(status, program, stopped), Resources::default()))
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
    fn a_death_by_signal_is_128_and_its_number_and_the_launcher_has_cpu_of_its_own() {
        for (signal, number) in [("KILL", 9), ("ABRT", 6)] {
            let mut cmd = Command::new("sh");
            cmd.args(["-c", &format!("kill -{signal} $$")]);
            let (code, out, r) = run(&mut cmd, false).unwrap();
            assert_eq!(code, 128 + number, "SIG{signal}");
            assert!(out.is_empty());
            assert!(r.measured);
        }
        let (user, sys) = own_cpu_ms();
        assert!(user + sys < u64::MAX);
        assert!(Command::new("/nonexistent/binary").spawn().is_err());
        assert!(run(&mut Command::new("/nonexistent/binary"), false).is_err());
    }
}
