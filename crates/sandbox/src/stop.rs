//! Stopping the runs this process has started, for a launcher told to stop by a signal.
//! [`stop`] kills every run [`crate::measure::run`] has spawned and not yet waited for,
//! and every run spawned after it, so each returns as a run that ended does: the
//! launcher then undoes what the run set up and exits, rather than dying with the
//! sandbox's scratch directory, its mount and its socket left behind.
//!
//! A run is killed with SIGKILL. On Linux the process spawned is the sandbox's
//! intermediate, which never execs and so still has the launcher's own signal handlers;
//! a signal it could catch would not end it. Its death ends the sandbox's pid namespace,
//! and the harness in it, through the parent-death signal its pid 1 holds.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

/// The runs of this process.
static RUNS: Runs = Runs::new();

/// Kill every run this process has spawned and not yet waited for, and every run it
/// spawns from now on.
pub fn stop() {
    RUNS.stop();
}

/// Whether [`stop`] has been called: a launcher starts no further run once it has.
#[must_use]
pub fn stopping() -> bool {
    RUNS.stopping()
}

/// Whether a process with this pid runs. One that does, whoever's, or one that cannot be
/// told, counts as running, so that what a live process owns is never swept.
#[must_use]
pub fn process_runs(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            return true;
        };
        if pid <= 0 {
            return true;
        }
        // SAFETY: kill with signal 0 sends nothing; it only checks that the pid exists.
        let r = unsafe { libc::kill(pid, 0) };
        r == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

/// Point standard output and standard error at the null device, for a launcher whose
/// terminal has hung up: a write to it fails, and `print!` and `eprintln!` panic on a
/// failed write, which would end the launcher before it cleans up.
///
/// # Errors
///
/// When the null device cannot be opened or put in place.
pub fn quiet_standard_streams() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let null = std::fs::OpenOptions::new().write(true).open("/dev/null")?;
        for fd in [1, 2] {
            // SAFETY: dup2 of a descriptor this function holds open onto a standard one.
            if unsafe { libc::dup2(null.as_raw_fd(), fd) } < 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
    }
    Ok(())
}

/// Count `child` among the runs [`stop`] kills, until the returned value is dropped;
/// killed at once when [`stop`] has already been called. Called before the child is
/// waited for, while its pid is still its own.
///
/// With `group`, the child leads a process group of its own, and the whole group is
/// killed: what the run left running in the background holds its output open.
pub(crate) fn register(child: &std::process::Child, group: bool) -> Registered<'static> {
    RUNS.register(child, group)
}

/// Whether this process ignores `signal`, as one started under `nohup`, or in the
/// background by a shell without job control, ignores SIGHUP or SIGINT. A launcher keeps
/// that: it does not stop on a signal it was started to ignore.
#[must_use]
pub fn ignored(signal: i32) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: sigaction is plain data, for which all zeroes is a valid value.
        let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
        // SAFETY: sigaction with a null new action only reads the current one into `old`.
        let r = unsafe { libc::sigaction(signal, std::ptr::null(), &raw mut old) };
        r == 0 && old.sa_sigaction == libc::SIG_IGN
    }
    #[cfg(not(unix))]
    {
        let _ = signal;
        false
    }
}

/// Runs spawned and not yet waited for, and whether they are being stopped.
struct Runs {
    stopping: AtomicBool,
    running: Mutex<Vec<Handle>>,
}

impl Runs {
    const fn new() -> Runs {
        Runs {
            stopping: AtomicBool::new(false),
            running: Mutex::new(Vec::new()),
        }
    }

    fn running(&self) -> MutexGuard<'_, Vec<Handle>> {
        self.running.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        for h in self.running().iter() {
            h.kill();
        }
    }

    fn stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }

    fn register(&self, child: &std::process::Child, group: bool) -> Registered<'_> {
        let h = Handle::of(child.id(), group);
        let mut list = self.running();
        // read under the lock that stop takes after setting it: a run registered while
        // stop goes through its list is either in the list or sees the flag
        if self.stopping() {
            h.kill();
        }
        list.push(h);
        Registered {
            runs: self,
            pid: child.id(),
        }
    }
}

/// A spawned run, until it has been waited for.
pub(crate) struct Registered<'a> {
    runs: &'a Runs,
    pid: u32,
}

impl Drop for Registered<'_> {
    fn drop(&mut self) {
        self.runs.running().retain(|h| h.pid != self.pid);
    }
}

/// What a run is killed through: on Linux a pidfd, which names the process itself and
/// not a number a later process may be given once it is reaped.
struct Handle {
    pid: u32,
    /// Whether the pid leads a process group of the run's own, killed with it.
    #[cfg(unix)]
    group: bool,
    #[cfg(target_os = "linux")]
    pidfd: Option<std::os::fd::OwnedFd>,
}

impl Handle {
    fn of(pid: u32, group: bool) -> Handle {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::FromRawFd;
            let raw = libc::pid_t::try_from(pid).unwrap_or(0);
            // SAFETY: pidfd_open takes a pid and flags and returns a new descriptor or -1.
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, raw, 0) };
            let pidfd = libc::c_int::try_from(fd)
                .ok()
                .filter(|fd| *fd >= 0)
                // SAFETY: a descriptor pidfd_open just returned, owned by nothing else.
                .map(|fd| unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) });
            Handle { pid, group, pidfd }
        }
        #[cfg(not(target_os = "linux"))]
        {
            #[cfg(not(unix))]
            let _ = group;
            Handle {
                pid,
                #[cfg(unix)]
                group,
            }
        }
    }

    fn kill(&self) {
        #[cfg(unix)]
        if self.group {
            if let Ok(pid) = libc::pid_t::try_from(self.pid) {
                // SAFETY: kill of the process group this run's child leads; the leader is
                // not yet waited for, so no other group has its number.
                unsafe { libc::kill(-pid, libc::SIGKILL) };
            }
        }
        #[cfg(target_os = "linux")]
        if let Some(fd) = &self.pidfd {
            use std::os::fd::AsRawFd;
            // SAFETY: pidfd_send_signal on a pidfd this handle owns, with no siginfo.
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    fd.as_raw_fd(),
                    libc::SIGKILL,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                )
            };
            return;
        }
        #[cfg(unix)]
        if let Ok(pid) = libc::pid_t::try_from(self.pid) {
            // SAFETY: kill with a positive pid of a child not yet waited for.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use std::process::{Child, Command, Stdio};

    /// A process that runs until it is killed. By absolute path: other tests change PATH.
    fn waiting() -> Child {
        Command::new("/bin/sh")
            .args(["-c", "read x"])
            .stdin(Stdio::piped())
            .spawn()
            .unwrap()
    }

    /// Whether `child` was killed within ten seconds; killed here if not, so a failing
    /// test does not hang.
    fn killed(mut child: Child) -> bool {
        for _ in 0..100 {
            if let Some(status) = child.try_wait().unwrap() {
                return !status.success();
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        child.kill().ok();
        child.wait().ok();
        false
    }

    #[test]
    fn a_stop_kills_the_runs_in_flight_and_every_run_after_it() {
        let runs = Runs::new();
        let before = waiting();
        let kept = runs.register(&before, false);
        let mut done = Command::new("/bin/sh").args(["-c", ":"]).spawn().unwrap();
        let gone = runs.register(&done, false);
        done.wait().unwrap();
        drop(gone);
        assert_eq!(runs.running().len(), 1);
        assert!(!runs.stopping());

        runs.stop();

        assert!(runs.stopping());
        assert!(killed(before), "a run in flight is killed");
        drop(kept);
        assert!(runs.running().is_empty());
        let after = waiting();
        let _r = runs.register(&after, false);
        assert!(killed(after), "a run spawned after the stop is killed");
    }

    #[test]
    fn a_stop_kills_a_grouped_run_and_what_it_left_in_the_background() {
        use std::io::BufRead;
        use std::os::unix::process::CommandExt;
        let runs = Runs::new();
        let mut leader = Command::new("/bin/sh");
        leader
            .args(["-c", "/bin/sleep 300 & echo $!; wait"])
            .stdout(Stdio::piped())
            .process_group(0);
        let mut leader = leader.spawn().unwrap();
        let mut line = String::new();
        std::io::BufReader::new(leader.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let background: u32 = line.trim().parse().unwrap();
        let _r = runs.register(&leader, true);
        assert!(process_runs(background));

        runs.stop();

        assert!(killed(leader));
        let gone = (0..100).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(50));
            !process_runs(background)
        });
        if !gone {
            Command::new("kill")
                .args(["-KILL", &background.to_string()])
                .status()
                .ok();
        }
        assert!(gone, "the background child goes with its group");
    }

    #[test]
    fn a_signal_this_process_ignores_is_said_to_be_ignored() {
        assert!(!ignored(libc::SIGUSR2));
        // SAFETY: setting SIGUSR2, which nothing in this test binary uses, to ignored.
        unsafe { libc::signal(libc::SIGUSR2, libc::SIG_IGN) };
        assert!(ignored(libc::SIGUSR2));
        // SAFETY: as above, back to the default.
        unsafe { libc::signal(libc::SIGUSR2, libc::SIG_DFL) };
        assert!(!ignored(libc::SIGUSR2));
    }
}
