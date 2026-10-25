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
pub(crate) fn register(child: &std::process::Child) -> Registered<'static> {
    RUNS.register(child)
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

    fn register(&self, child: &std::process::Child) -> Registered<'_> {
        let h = Handle::of(child.id());
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
    #[cfg(target_os = "linux")]
    pidfd: Option<std::os::fd::OwnedFd>,
}

impl Handle {
    fn of(pid: u32) -> Handle {
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
            Handle { pid, pidfd }
        }
        #[cfg(not(target_os = "linux"))]
        {
            Handle { pid }
        }
    }

    fn kill(&self) {
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
        let kept = runs.register(&before);
        let mut done = Command::new("/bin/sh").args(["-c", ":"]).spawn().unwrap();
        let gone = runs.register(&done);
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
        let _r = runs.register(&after);
        assert!(killed(after), "a run spawned after the stop is killed");
    }
}
