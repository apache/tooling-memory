//! A launcher told to stop. SIGTERM, SIGINT and SIGHUP end a session the way a harness
//! that exits does: the sandbox is killed, the run in flight unmounts the projection and
//! removes its scratch directory as a run that ended does, and the session finishes,
//! removing its socket and its render. The launcher then exits with 128 plus the
//! signal's number, as a shell reports a process a signal ended.
//!
//! SIGKILL cannot be caught. What a launcher killed by it leaves, the next launcher to
//! start a run sweeps: `dm_sandbox` its scratch directory, `dm_mount` its mount, and the
//! launcher its egress socket, each named for or locked by the launcher that made it.
//! Its render stays: nothing records which launcher a render belongs to.

use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};

type Fallible<T> = Result<T, Box<dyn std::error::Error>>;

/// Runs between the start of their setup and the end of their cleanup.
static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

/// A run in flight, from before it mounts anything until it has undone all of it.
pub struct InFlight(());

impl InFlight {
    /// Count a run in flight, or refuse it once the launcher is stopping.
    ///
    /// # Errors
    ///
    /// When a signal has told the launcher to stop.
    pub fn enter() -> Fallible<InFlight> {
        IN_FLIGHT.fetch_add(1, Ordering::SeqCst);
        // counted before the check: a stop either sees this run or this run sees the stop
        let run = InFlight(());
        if dm_sandbox::stop::stopping() {
            return Err("the launcher is stopping".into());
        }
        Ok(run)
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The signals that stop a launcher, listened for from when this is made, so one that
/// arrives while the session starts is not the process's death. A signal the launcher
/// was started to ignore, as under `nohup`, is not listened for and stays ignored.
pub struct Stops {
    #[cfg(unix)]
    signals: [(i32, Option<tokio::signal::unix::Signal>); 3],
}

/// The first of the signals listened for, by number.
#[cfg(unix)]
async fn first(signals: &mut [(i32, Option<tokio::signal::unix::Signal>); 3]) -> i32 {
    async fn one(s: &mut Option<tokio::signal::unix::Signal>) {
        match s {
            Some(s) => {
                s.recv().await;
            }
            None => std::future::pending().await,
        }
    }
    let [(n1, s1), (n2, s2), (n3, s3)] = signals;
    tokio::select! {
        () = one(s1) => *n1,
        () = one(s2) => *n2,
        () = one(s3) => *n3,
    }
}

impl Stops {
    /// Listen for SIGTERM, SIGINT and SIGHUP from now on, each that is not ignored.
    ///
    /// # Errors
    ///
    /// When a signal handler cannot be installed.
    pub fn listen() -> std::io::Result<Stops> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            let listen = |n: i32, kind: SignalKind| -> std::io::Result<_> {
                Ok((
                    n,
                    if dm_sandbox::stop::ignored(n) {
                        None
                    } else {
                        Some(signal(kind)?)
                    },
                ))
            };
            Ok(Stops {
                signals: [
                    listen(15, SignalKind::terminate())?,
                    listen(2, SignalKind::interrupt())?,
                    listen(1, SignalKind::hangup())?,
                ],
            })
        }
        #[cfg(not(unix))]
        {
            Ok(Stops {})
        }
    }

    /// Run `flow`, a session's work, to its end, or until a signal stops it. On a signal
    /// every run is killed; the run in flight is let finish its cleanup, and whatever the
    /// flow would do after it is not done. Returns the flow's code, or 128 plus the
    /// signal's number. A second signal while the cleanup runs ends the wait: `hard`,
    /// quick cleanup that must not be left out, runs, and the launcher exits.
    ///
    /// # Errors
    ///
    /// The flow's, when it ends of itself.
    pub async fn around(
        self,
        flow: impl Future<Output = Fallible<i32>>,
        hard: impl Future<Output = ()>,
    ) -> Fallible<i32> {
        #[cfg(unix)]
        {
            let mut signals = self.signals;
            let mut flow = std::pin::pin!(flow);
            // the signals first: a harness the same hangup ended returns into a flow that
            // would print to the terminal that hung up
            let signal = tokio::select! {
                biased;
                n = first(&mut signals) => n,
                r = &mut flow => return r,
            };
            dm_sandbox::stop::stop();
            let said = say(signal);
            if !said || (signal == 1 && !both_to_files()) {
                // nowhere to say anything: what the flow and the finish print goes nowhere,
                // rather than ending the launcher in a panic before it cleans up
                dm_sandbox::stop::quiet_standard_streams().ok();
            }
            while IN_FLIGHT.load(Ordering::SeqCst) > 0 {
                tokio::select! {
                    biased;
                    n = first(&mut signals) => {
                        // told twice: stop waiting, but leave nothing the next launch
                        // cannot sweep. The cap bounds the waits in it only: a removal or
                        // a ledger statement that runs synchronously runs to its end
                        // (the ledger's own busy timeout bounds a statement).
                        let quick = std::time::Duration::from_secs(5);
                        tokio::time::timeout(quick, hard).await.ok();
                        std::process::exit(128 + n);
                    }
                    _ = &mut flow => break,
                    () = tokio::time::sleep(std::time::Duration::from_millis(20)) => {}
                }
            }
            Ok(128 + signal)
        }
        #[cfg(not(unix))]
        {
            let _ = hard;
            flow.await
        }
    }
}

/// Whether standard output and standard error both go to regular files, as under
/// `nohup`. Otherwise a SIGHUP may mean they have gone: a terminal that hung up, which
/// then no longer answers as a terminal, or a pipe to a reader on it.
#[cfg(unix)]
fn both_to_files() -> bool {
    use std::os::fd::AsFd;
    let to_a_file = |fd: std::os::fd::BorrowedFd<'_>| {
        fd.try_clone_to_owned()
            .map(std::fs::File::from)
            .and_then(|f| f.metadata())
            .is_ok_and(|m| m.is_file())
    };
    to_a_file(std::io::stdout().as_fd()) && to_a_file(std::io::stderr().as_fd())
}

/// Say on standard error that a signal stops the launcher. Whether it was said: a
/// terminal that hung up refuses the write.
#[cfg(unix)]
fn say(signal: i32) -> bool {
    use std::io::Write;
    let name = match signal {
        1 => "SIGHUP",
        2 => "SIGINT",
        _ => "SIGTERM",
    };
    let line = format!("memory: {name}: ending the run and cleaning up\n");
    std::io::stderr().write_all(line.as_bytes()).is_ok()
}

/// Write `text` to standard output, and a line end after it when it has none. A reader
/// that has gone, a closed pipe, is not an error here: the launcher goes on to settle and
/// clean up, where `print!` would panic.
pub fn answer(text: &str) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(text.as_bytes());
    if !text.ends_with('\n') {
        let _ = out.write_all(b"\n");
    }
    let _ = out.flush();
}
