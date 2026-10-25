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
/// arrives while the session starts is not the process's death.
pub struct Stops {
    #[cfg(unix)]
    signals: [(i32, tokio::signal::unix::Signal); 3],
}

impl Stops {
    /// Listen for SIGTERM, SIGINT and SIGHUP from now on.
    ///
    /// # Errors
    ///
    /// When a signal handler cannot be installed.
    pub fn listen() -> std::io::Result<Stops> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            Ok(Stops {
                signals: [
                    (15, signal(SignalKind::terminate())?),
                    (2, signal(SignalKind::interrupt())?),
                    (1, signal(SignalKind::hangup())?),
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
    /// signal's number.
    ///
    /// # Errors
    ///
    /// The flow's, when it ends of itself.
    pub async fn around(self, flow: impl Future<Output = Fallible<i32>>) -> Fallible<i32> {
        #[cfg(unix)]
        {
            let [(n1, mut s1), (n2, mut s2), (n3, mut s3)] = self.signals;
            let mut flow = std::pin::pin!(flow);
            // the signals first: a harness the same hangup ended returns into a flow that
            // would print to the terminal that hung up
            let signal = tokio::select! {
                biased;
                _ = s1.recv() => n1,
                _ = s2.recv() => n2,
                _ = s3.recv() => n3,
                r = &mut flow => return r,
            };
            dm_sandbox::stop::stop();
            if !say(signal) || signal == 1 {
                // nowhere to say anything: what the flow and the finish print goes nowhere,
                // rather than ending the launcher in a panic before it cleans up
                dm_sandbox::stop::quiet_standard_streams().ok();
            }
            while IN_FLIGHT.load(Ordering::SeqCst) > 0 {
                tokio::select! {
                    biased;
                    // told twice: stop waiting for the cleanup
                    _ = s1.recv() => std::process::exit(128 + n1),
                    _ = s2.recv() => std::process::exit(128 + n2),
                    _ = s3.recv() => std::process::exit(128 + n3),
                    _ = &mut flow => break,
                    () = tokio::time::sleep(std::time::Duration::from_millis(20)) => {}
                }
            }
            Ok(128 + signal)
        }
        #[cfg(not(unix))]
        {
            flow.await
        }
    }
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
