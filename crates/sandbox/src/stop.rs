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

/// Whether this process has a controlling terminal, which a process group of a run's
/// own would be stopped for touching.
#[must_use]
pub fn has_a_terminal() -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // without waiting: a serial line without CLOCAL would hold the open
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
            .open("/dev/tty")
            .is_ok()
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// Whether nothing listens on the unix socket at `path`: a connection to it is refused.
/// The connection does not wait. On Linux a listener whose backlog is full, one that is
/// stopped and takes no more, counts as one that listens. Elsewhere, macOS say, a full
/// backlog refuses as well; the sweep asks only after the pid the socket is named for
/// has been found not running, and without pid namespaces there that pid is the
/// listener's, so a full backlog there is not a live launcher's.
#[must_use]
pub fn socket_refuses(path: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let bytes = path.as_os_str().as_bytes();
        // SAFETY: sockaddr_un is plain data, for which all zeroes is a valid value.
        let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        if bytes.len() >= addr.sun_path.len() {
            return false;
        }
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        for (to, from) in addr.sun_path.iter_mut().zip(bytes) {
            *to = *from as libc::c_char;
        }
        let fd = nonblocking_unix_stream();
        if fd < 0 {
            return false;
        }
        let len = libc::socklen_t::try_from(std::mem::size_of::<libc::sockaddr_un>()).unwrap_or(0);
        // SAFETY: connect of the socket made above to an address of the size given, then
        // close of that socket.
        unsafe {
            let r = libc::connect(fd, (&raw const addr).cast(), len);
            let refused =
                r < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ECONNREFUSED);
            libc::close(fd);
            refused
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

/// A non-blocking, close-on-exec unix stream socket, or -1.
#[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
fn nonblocking_unix_stream() -> libc::c_int {
    // SAFETY: a socket call with constant arguments; it returns a descriptor or -1.
    unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    }
}

/// A non-blocking, close-on-exec unix stream socket, or -1: by fcntl after the socket
/// call, where the socket call cannot set either.
#[cfg(all(
    unix,
    not(any(target_os = "linux", target_os = "android", target_os = "freebsd"))
))]
fn nonblocking_unix_stream() -> libc::c_int {
    // SAFETY: a socket call with constant arguments, then fcntl on the descriptor it
    // returned, and its close when fcntl fails.
    unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
        if fd < 0 {
            return -1;
        }
        let fl = libc::fcntl(fd, libc::F_GETFL);
        let set = fl >= 0
            && libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK) == 0
            && libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) == 0;
        if set {
            fd
        } else {
            libc::close(fd);
            -1
        }
    }
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
        let killed = h.killed.clone();
        list.push(h);
        Registered {
            runs: self,
            pid: child.id(),
            killed,
        }
    }
}

/// A spawned run, until it has been waited for.
pub(crate) struct Registered<'a> {
    runs: &'a Runs,
    pid: u32,
    killed: std::sync::Arc<AtomicBool>,
}

impl Registered<'_> {
    /// Whether a stop killed the run while it was still running: then a SIGKILL it ended
    /// with was the stop's, and not a death to report.
    pub(crate) fn killed(&self) -> bool {
        self.killed.load(Ordering::SeqCst)
    }
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
    /// Set when a stop kills the run before it has exited.
    killed: std::sync::Arc<AtomicBool>,
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
            Handle {
                pid,
                killed: std::sync::Arc::default(),
                group,
                pidfd,
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            #[cfg(not(unix))]
            let _ = group;
            Handle {
                pid,
                killed: std::sync::Arc::default(),
                #[cfg(unix)]
                group,
            }
        }
    }

    /// Whether the run has already exited, though not yet reaped.
    fn exited(&self) -> bool {
        #[cfg(unix)]
        {
            let pid: libc::id_t = self.pid;
            // SAFETY: siginfo_t is plain data, for which all zeroes is a valid value.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            // SAFETY: waitid on this process's own child, without waiting and leaving it
            // unreaped, with a valid pointer to write into.
            let r = unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid,
                    &raw mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            // a child that has exited fills the siginfo; one still running leaves it zero
            r == 0 && info.si_signo != 0
        }
        #[cfg(not(unix))]
        {
            false
        }
    }

    fn kill(&self) {
        if !self.exited() {
            self.killed.store(true, Ordering::SeqCst);
        }
        #[cfg(unix)]
        if self.group {
            if let Ok(pid) = libc::pid_t::try_from(self.pid) {
                // SAFETY: kill of the process group this run's child leads. The handle is
                // dropped before the leader is reaped, so while it is here no other
                // process or group can have the leader's number.
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

    #[test]
    fn a_socket_refuses_only_when_nothing_listens_and_a_full_one_does_not_hang() {
        let dir = tempfile::tempdir().unwrap();
        // bound and never listening: refused, whatever a fork by another test copies
        let gone = dir.path().join("gone.sock");
        bound_only(&gone);
        assert!(socket_refuses(&gone));
        let live = dir.path().join("live.sock");
        let _live = std::os::unix::net::UnixListener::bind(&live).unwrap();
        assert!(!socket_refuses(&live));
        assert!(
            !socket_refuses(&dir.path().join("none.sock")),
            "not there: not refused"
        );

        full_backlog_is_a_listener(dir.path());
    }

    /// Bind a unix stream socket at `path` and close it, without listening.
    fn bound_only(path: &std::path::Path) {
        use std::os::unix::ffi::OsStrExt;
        // SAFETY: a zeroed address filled with a path that fits, a socket, its bind and
        // its close.
        unsafe {
            let mut addr: libc::sockaddr_un = std::mem::zeroed();
            addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
            for (to, from) in addr.sun_path.iter_mut().zip(path.as_os_str().as_bytes()) {
                *to = *from as libc::c_char;
            }
            let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
            assert!(fd >= 0);
            let len = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
            assert_eq!(libc::bind(fd, (&raw const addr).cast(), len), 0);
            libc::close(fd);
        }
    }

    /// On Linux a full backlog is a listener, said without waiting. macOS refuses a
    /// connection to a full backlog.
    #[cfg(not(target_os = "linux"))]
    fn full_backlog_is_a_listener(_dir: &std::path::Path) {}

    #[cfg(target_os = "linux")]
    fn full_backlog_is_a_listener(dir: &std::path::Path) {
        // a listener that accepts nothing, its backlog filled
        let full = dir.join("full.sock");
        let listener = std::os::unix::net::UnixListener::bind(&full).unwrap();
        {
            use std::os::fd::AsRawFd;
            // SAFETY: listen on a socket this test holds, with the smallest backlog.
            assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 0) }, 0);
        }
        let mut held = Vec::new();
        loop {
            let s = socket2_like_connect(&full);
            match s {
                Some(fd) => held.push(fd),
                None => break,
            }
            assert!(held.len() < 1000, "the backlog never filled");
        }
        // on a thread of its own, so a probe that waits fails here rather than hangs
        let (said, answer) = std::sync::mpsc::channel();
        let probed = full.clone();
        std::thread::spawn(move || said.send(socket_refuses(&probed)).ok());
        let refused = answer.recv_timeout(std::time::Duration::from_secs(2));
        assert_eq!(
            refused,
            Ok(false),
            "a full backlog is a listener, said at once"
        );
        for fd in held {
            // SAFETY: close of a socket this test opened.
            unsafe { libc::close(fd) };
        }
    }

    /// A non-blocking connection to `path` that is kept open; `None` once one would wait.
    #[cfg(target_os = "linux")]
    fn socket2_like_connect(path: &std::path::Path) -> Option<libc::c_int> {
        use std::os::unix::ffi::OsStrExt;
        // SAFETY: as in socket_refuses: a zeroed address filled with a path that fits, a
        // non-blocking socket, a connect of it.
        unsafe {
            let mut addr: libc::sockaddr_un = std::mem::zeroed();
            addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
            for (to, from) in addr.sun_path.iter_mut().zip(path.as_os_str().as_bytes()) {
                *to = *from as libc::c_char;
            }
            let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
            libc::fcntl(
                fd,
                libc::F_SETFL,
                libc::fcntl(fd, libc::F_GETFL) | libc::O_NONBLOCK,
            );
            let len = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
            if libc::connect(fd, (&raw const addr).cast(), len) == 0 {
                Some(fd)
            } else {
                libc::close(fd);
                None
            }
        }
    }
}
