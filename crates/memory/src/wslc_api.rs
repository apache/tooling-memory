//! The `wslc` backend over the WSL containers C API, `wslcsdk.dll`: the same two
//! containers as over the command line, made through the API Microsoft supports for
//! production.
//!
//! A session made through the API is a VM of our own with its own image store, so the
//! image is provisioned into it first: loaded from a `wslc save` archive when the command
//! line's store has it, pulled from its registry otherwise, and only when the store does
//! not already hold that image id. A session lives while a handle to it is held, so the
//! VM, and everything in it, ends with this process, however it ends.
//!
//! One session per disk at a time, and one per name: a launch takes the first free slot,
//! each a name and a disk of its own with its own image store, so a second concurrent
//! launch provisions a second slot rather than waiting.
//!
//! The API gives a process no terminal and makes no networks, so an interactive launch
//! and the gateway inside the VM stay on the command line.
// the descriptions below are built and tested everywhere, and used only on Windows
#![cfg_attr(not(windows), allow(dead_code))]

use crate::requests::{RunRequest, REQUEST_DIR_INSIDE};
use crate::wslc::{self, Launch, INSIDE_HOST_HOME, INSIDE_ROOT, INSIDE_WORK};
use dm_sandbox::{INSIDE_HOME, INSIDE_PROJECTION};
use dm_wslc_api::{Bind, ContainerSpec, ImageInfo, Mount, Networking, ProcessSpec};
use std::path::{Path, PathBuf};

/// The slots' session names are this, a dash, and the slot number.
const SESSION_PREFIX: &str = "durable-memory";
/// How many launches may run at once, each in a slot of its own.
const SLOTS: usize = 8;
/// The shared volume's ceiling; it is dynamically sized and holds requests, exit codes,
/// captured output and the egress socket.
const VOLUME_BYTES: u64 = 4 << 30;
/// The harness user, which the image creates; the API has no user setting, so the
/// harness container's first process drops to it.
const HARNESS_UID: u32 = 1000;

/// The host directories a launch mounts, as Windows names them.
#[derive(Clone, Debug)]
pub(crate) struct Mounts {
    /// The record root.
    pub root: String,
    /// The working tree.
    pub work: String,
    /// The harness's private home.
    pub home: String,
    /// The rendered projection.
    pub projection: Option<String>,
    /// The host home and the login directories under it, read-only.
    pub host_home: Option<(String, Vec<String>)>,
}

/// The egress container: this binary as `amem inside`, the record, the shared volume,
/// the logins read-only, and the session's bridge as its only way out.
pub(crate) fn egress_spec(
    l: &Launch,
    name: &str,
    volume: &str,
    m: &Mounts,
    extra: &[(String, String)],
) -> ContainerSpec {
    let mut binds = vec![Bind {
        host: m.root.clone(),
        container: INSIDE_ROOT.into(),
        read_only: false,
    }];
    if let Some((home, dirs)) = &m.host_home {
        binds.extend(dirs.iter().map(|d| Bind {
            host: format!("{home}{}{d}", std::path::MAIN_SEPARATOR),
            container: format!("{INSIDE_HOST_HOME}/{d}"),
            read_only: true,
        }));
    }
    ContainerSpec {
        image: l.image.clone(),
        name: Some(name.to_string()),
        networking: Networking::Bridged,
        binds,
        mounts: vec![shared(volume)],
        init: ProcessSpec {
            argv: wslc::inside_command(l),
            env: wslc::egress_env(&wslc::tap_credentials(&l.root), extra),
            cwd: None,
        },
        auto_remove: false,
    }
}

/// A harness container, from one request: no network, the shared volume, the private
/// home, the working tree and the projection, run as the harness user, removed on exit.
pub(crate) fn harness_spec(
    req: &RunRequest,
    n: usize,
    image: &str,
    volume: &str,
    m: &Mounts,
) -> ContainerSpec {
    let mut binds = vec![
        Bind {
            host: m.home.clone(),
            container: INSIDE_HOME.into(),
            read_only: false,
        },
        Bind {
            host: m.work.clone(),
            container: INSIDE_WORK.into(),
            read_only: false,
        },
    ];
    // the projection's path is the host's own, from the launch, never the request's:
    // the request file sits in a volume the harness container can write
    if let Some(p) = &m.projection {
        binds.push(Bind {
            host: p.clone(),
            container: INSIDE_PROJECTION.into(),
            read_only: true,
        });
    }
    let mut argv = vec![
        "/usr/bin/setpriv".to_string(),
        format!("--reuid={HARNESS_UID}"),
        format!("--regid={HARNESS_UID}"),
        "--clear-groups".to_string(),
    ];
    argv.extend(wslc::wrapped(req, n));
    ContainerSpec {
        image: image.to_string(),
        name: None,
        networking: Networking::None,
        binds,
        mounts: vec![shared(volume)],
        init: ProcessSpec {
            argv,
            env: req
                .env
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            cwd: Some(req.cwd.clone()),
        },
        auto_remove: true,
    }
}

/// A throwaway container that prints one file from the shared volume.
pub(crate) fn reader_spec(image: &str, volume: &str, file: &str) -> ContainerSpec {
    ContainerSpec {
        image: image.to_string(),
        name: None,
        networking: Networking::None,
        binds: Vec::new(),
        mounts: vec![shared(volume)],
        init: ProcessSpec {
            argv: vec!["cat".into(), format!("{REQUEST_DIR_INSIDE}/{file}")],
            env: Vec::new(),
            cwd: None,
        },
        auto_remove: true,
    }
}

fn shared(volume: &str) -> Mount {
    Mount {
        volume: volume.to_string(),
        container: REQUEST_DIR_INSIDE.into(),
        read_only: false,
    }
}

/// What getting the image into a session's store takes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Provision {
    /// The store holds it, with this id.
    Present { id: String },
    /// Load it from the command line's store; `stale` is a different image under the
    /// same name, to delete first.
    Load { stale: Option<String> },
    /// Pull it from the registry its name gives.
    Pull,
}

/// Decide how to provision `image` from what the session's store holds. The store is
/// trusted when it has the image, since asking the command line boots the command line's
/// VM when it has gone idle; `refresh` asks anyway, for after a rebuild. When asked,
/// the command line's copy is the newer truth: a rebuilt image replaces the session's,
/// and a name only a registry has is pulled again.
pub(crate) fn provision(
    image: &str,
    store: &[ImageInfo],
    refresh: bool,
    source_id: impl FnOnce() -> Option<String>,
) -> Provision {
    let held = store.iter().find(|i| i.answers_to(image));
    if let (Some(h), false) = (held, refresh) {
        return Provision::Present { id: h.id.clone() };
    }
    match (held, source_id()) {
        (Some(h), Some(src)) if h.id == dm_wslc_api::spec::bare_id(&src) => {
            Provision::Present { id: h.id.clone() }
        }
        (held, Some(_)) => Provision::Load {
            stale: held.map(|h| h.id.clone()),
        },
        (_, None) => Provision::Pull,
    }
}

/// Where the sessions' disks live: given, or under the root's state when the root is on
/// a drive. A session disk cannot live on a `\\wsl.localhost` share, so a root there
/// needs one named.
///
/// # Errors
///
/// When the root is a network path and no storage was given.
pub(crate) fn storage_for(root: &Path, given: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(g) = given {
        return Ok(g.to_path_buf());
    }
    let r = root.display().to_string();
    if r.starts_with(r"\\") || r.starts_with("//") {
        return Err(format!(
            "the record root {r} is a network path, where a session disk cannot live; \
             give a directory on a local drive with --wslc-storage or MEMORY_WSLC_STORAGE"
        ));
    }
    Ok(root.join("state").join("wslc-api"))
}

/// The creator's process id of the session `name`, from `wslc system session list`.
pub(crate) fn creator_of(listing: &str, name: &str) -> Option<u32> {
    listing.lines().find_map(|line| {
        let cols: Vec<&str> = line.split_whitespace().collect();
        match cols.as_slice() {
            [_, pid, n] if *n == name => pid.parse().ok(),
            _ => None,
        }
    })
}

/// `ERROR_ALREADY_EXISTS`: another process holds a session by that name.
const ALREADY_EXISTS: u32 = 0x8007_00b7;
/// `ERROR_SHARING_VIOLATION`: another session holds that disk.
const SHARING_VIOLATION: u32 = 0x8007_0020;

/// Take the first free slot: `create` makes slot `n`'s session, and a slot whose name is
/// taken is offered to `reclaim`, which ends the session if its creator is gone. A slot
/// still busy is passed over. The session and its slot number, or `None` when every slot
/// is in use.
///
/// # Errors
///
/// A failure other than the slot being busy.
pub(crate) fn take_slot<S>(
    slots: usize,
    mut create: impl FnMut(usize) -> Result<S, dm_wslc_api::Error>,
    mut reclaim: impl FnMut(usize) -> bool,
) -> Result<Option<(S, usize)>, dm_wslc_api::Error> {
    let busy = |e: &dm_wslc_api::Error| e.hr == ALREADY_EXISTS || e.hr == SHARING_VIOLATION;
    for n in 0..slots {
        match create(n) {
            Ok(s) => return Ok(Some((s, n))),
            // a reclaimed session's disk may still be releasing; then it is busy too
            Err(e) if e.hr == ALREADY_EXISTS && reclaim(n) => match create(n) {
                Ok(s) => return Ok(Some((s, n))),
                Err(e) if busy(&e) => continue,
                Err(e) => return Err(e),
            },
            Err(e) if busy(&e) => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(None)
}

/// The run volumes in a slot's `volumes` directory that are not this run's: a slot is
/// held by one launch at a time, so any other is left by a launch that was killed.
pub(crate) fn stale_volumes(files: impl IntoIterator<Item = String>, own: &str) -> Vec<String> {
    files
        .into_iter()
        .filter_map(|f| f.strip_suffix(".vhdx").map(str::to_string))
        .filter(|v| {
            v.strip_prefix("memory-run-")
                .is_some_and(|pid| !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit()))
                && v != own
        })
        .collect()
}

/// Slot `n`'s session name.
pub(crate) fn slot_name(n: usize) -> String {
    format!("{SESSION_PREFIX}-{n}")
}

#[cfg(windows)]
pub(crate) use on_windows::{check, run};

#[cfg(windows)]
mod on_windows {
    use super::*;
    use crate::requests::DONE;
    use dm_wslc_api::{Api, Container, Running, Session, Sink};
    use std::process::{Command, Stdio};
    use std::sync::Arc;
    use std::time::Duration;

    type Fallible<T> = Result<T, Box<dyn std::error::Error>>;

    /// Open the first free slot's session. A slot whose session outlives the process that
    /// made it, killed before it could end it, is taken back rather than passed over:
    /// the API ends such a session by itself only a minute or so later.
    fn open_slot(api: &Arc<Api>, storage: &Path) -> Fallible<(Session, PathBuf)> {
        let dir = |n: usize| storage.join(n.to_string());
        let taken = take_slot(
            SLOTS,
            |n| {
                std::fs::create_dir_all(dir(n))
                    .map_err(|e| dm_wslc_api::Error::other("slot directory", e.to_string()))?;
                Session::create(api, &slot_name(n), &dir(n))
            },
            |n| reclaim(&slot_name(n)),
        )?;
        let (session, n) = taken.ok_or_else(|| {
            format!(
                "all {SLOTS} session slots under {} are in use",
                storage.display()
            )
        })?;
        Ok((session, dir(n)))
    }

    /// Delete what killed launches left in this slot. The session's disk keeps their
    /// containers, which hold their run volumes, and the volumes; a launch killed before
    /// its harness exited leaves a container auto-removal never reached. A stale volume is
    /// the sign: it is made before any container. The API lists no containers, so the
    /// command line lists this session's, all of them stale while this launch, which has
    /// started none yet, holds the slot.
    fn sweep(session: &Session, slot: &Path, own: &str) {
        let dir = slot.join("volumes");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return;
        };
        let files = entries
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned());
        let stale = stale_volumes(files, own);
        if stale.is_empty() {
            return;
        }
        let name = format!(
            "{SESSION_PREFIX}-{}",
            slot.file_name().unwrap_or_default().to_string_lossy()
        );
        let args = ["--session", &name, "list", "-a", "-q"].map(String::from);
        for id in wslc::run_ok(&args).unwrap_or_default().lines() {
            if let Ok(c) = session.open_container(id.trim()) {
                c.delete().ok();
            }
        }
        for v in stale {
            match session.delete_volume(&v) {
                Ok(()) => eprintln!("memory: removed {v}, left by a launch that was killed"),
                Err(e) => eprintln!("memory: {v}, left by a launch that was killed, stays: {e}"),
            }
        }
    }

    /// End the session `name` if the process that created it is gone. Whether it ended.
    fn reclaim(name: &str) -> bool {
        let Ok(listing) = wslc::run_ok(&["system".into(), "session".into(), "list".into()]) else {
            return false;
        };
        let Some(pid) = creator_of(&listing, name) else {
            return false;
        };
        if process_alive(pid) {
            return false;
        }
        eprintln!("memory: ending {name}, left by process {pid}, which is gone");
        let args = ["--session", name, "system", "session", "terminate"].map(String::from);
        wslc::run_ok(&args).is_ok()
    }

    /// Whether a process with this id is running, as `tasklist` sees it.
    fn process_alive(pid: u32) -> bool {
        Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            // assume alive when it cannot be told: a live session is never ended
            .map_or(true, |o| {
                String::from_utf8_lossy(&o.stdout).contains(&format!("\"{pid}\""))
            })
    }

    /// On Ctrl-C, end the session, and every container in it with it, then exit as an
    /// interrupted process does; otherwise the session outlives this process for a while.
    /// The run's projection goes too: no later run mounts or renders it again.
    fn end_on_interrupt(
        session: &Arc<Session>,
        projection: PathBuf,
    ) -> Option<tokio::task::JoinHandle<()>> {
        let rt = tokio::runtime::Handle::try_current().ok()?;
        let s = session.clone();
        Some(rt.spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                eprintln!("memory: interrupted; ending the session");
                s.terminate().ok();
                std::fs::remove_dir_all(&projection).ok();
                std::process::exit(130);
            }
        }))
    }

    /// The image's id in the command line's store, when it is there.
    fn cli_image_id(image: &str) -> Option<String> {
        let out = Command::new(wslc::wslc())
            .args(["image", "inspect", image])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
        v.get(0)?.get("Id")?.as_str().map(str::to_string)
    }

    /// Make sure the session's store holds the image the launch names.
    fn ensure_image(session: &Session, image: &str, refresh: bool, scratch: &Path) -> Fallible<()> {
        let store = session.images()?;
        match provision(image, &store, refresh, || cli_image_id(image)) {
            Provision::Present { id } => {
                eprintln!(
                    "memory: {image} {} from this session's store; after a rebuild, \
                     `amem wslc-api-check --refresh`",
                    id.get(..12).unwrap_or(&id)
                );
                Ok(())
            }
            Provision::Pull => {
                eprintln!("memory: pulling {image} into this session's store");
                session.pull_image(image, None).map_err(Into::into)
            }
            Provision::Load { stale } => {
                if let Some(id) = stale {
                    session.delete_image(&id).ok();
                }
                load_from_cli(session, image, scratch)
            }
        }
    }

    /// `wslc save` the image to a file beside the session's disk, load it, and remove the
    /// file.
    fn load_from_cli(session: &Session, image: &str, scratch: &Path) -> Fallible<()> {
        eprintln!("memory: loading {image} into this session's store; once per image");
        let archive = scratch.join("image.tar");
        let started = std::time::Instant::now();
        let status = Command::new(wslc::wslc())
            .args(["save", image])
            .stdin(Stdio::null())
            .stdout(std::fs::File::create(&archive)?)
            .status()?;
        let loaded: Fallible<()> = if status.success() {
            session.load_image(&archive).map_err(Into::into)
        } else {
            Err(format!("wslc save {image}: {status}").into())
        };
        std::fs::remove_file(&archive).ok();
        loaded?;
        if !session.images()?.iter().any(|i| i.answers_to(image)) {
            return Err(
                format!("{image} was loaded but the session's store does not list it").into(),
            );
        }
        eprintln!("memory: loaded in {:.0?}", started.elapsed());
        Ok(())
    }

    fn mounts(l: &Launch) -> Fallible<Mounts> {
        let home_dir = l.root.join("state/sandbox").join(l.harness.binary());
        std::fs::create_dir_all(&home_dir)?;
        let (host_home, dirs) = wslc::host_logins()?;
        Ok(Mounts {
            root: wslc::mount_path(&l.root)?,
            work: wslc::mount_path(&l.workdir)?,
            home: wslc::mount_path(&home_dir)?,
            projection: wslc::projection_mount(l)?,
            host_home: host_home.map(|h| (h, dirs)),
        })
    }

    /// Run a launch through the C API. Returns the exit code the inside reports.
    ///
    /// # Errors
    ///
    /// When no slot is free, the image cannot be provisioned, or a container cannot be
    /// started.
    pub(crate) fn run(l: &Launch, api: &Arc<Api>, storage: &Path) -> Fallible<i32> {
        let (session, slot) = open_slot(api, storage)?;
        let session = Arc::new(session);
        let interrupt = end_on_interrupt(&session, wslc::projection_dir(l));
        let volume = format!("memory-run-{}", std::process::id());
        sweep(&session, &slot, &volume);
        ensure_image(&session, &l.image, false, &slot)?;
        let m = mounts(l)?;
        let pid = std::process::id();
        session.create_volume(&volume, VOLUME_BYTES)?;
        let result = (|| -> Fallible<i32> {
            let spec = egress_spec(l, &format!("memory-egress-{pid}"), &volume, &m, &[]);
            let egress = session.create_container(&spec)?;
            // its output is ours, as `wslc logs -f` gives it over the command line
            let running = egress.start(Sink::Inherit, Sink::Inherit)?;
            let code = drive(&session, &egress, &running, &volume, &l.image, &m);
            egress.delete().ok();
            code
        })();
        session.delete_volume(&volume).ok();
        if let Some(i) = interrupt {
            i.abort();
        }
        // ended here rather than on drop: the caller exits the process straight after,
        // and the interrupt task's reference may not have been dropped by then
        session.terminate().ok();
        result
    }

    /// A process in the egress container, its output captured.
    fn exec(c: &Container, argv: &[&str]) -> Fallible<String> {
        let spec = ProcessSpec {
            argv: argv.iter().map(|a| (*a).to_string()).collect(),
            env: Vec::new(),
            cwd: None,
        };
        let r = c.exec(&spec, Sink::Capture, Sink::Capture)?;
        let code = r.wait();
        let (out, err) = r.output();
        if code != 0 {
            return Err(format!(
                "{}: exit {code}: {}",
                argv.join(" "),
                String::from_utf8_lossy(&err).trim()
            )
            .into());
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    /// Serve the inside's requests until it writes `done`.
    fn drive(
        session: &Session,
        egress: &Container,
        running: &Running,
        volume: &str,
        image: &str,
        m: &Mounts,
    ) -> Fallible<i32> {
        let mut n = 0;
        loop {
            let listing = match exec(egress, &["ls", REQUEST_DIR_INSIDE]) {
                Ok(l) => l,
                Err(e) => return after_exit(session, running, volume, image, &e.to_string()),
            };
            let names: Vec<&str> = listing.lines().map(str::trim).collect();
            if names.contains(&DONE) {
                let text = exec(egress, &["cat", &format!("{REQUEST_DIR_INSIDE}/{DONE}")])?;
                return Ok(text.trim().parse().unwrap_or(1));
            }
            let req_name = RunRequest::file(n);
            if names.contains(&req_name.as_str()) {
                let text = exec(
                    egress,
                    &["cat", &format!("{REQUEST_DIR_INSIDE}/{req_name}")],
                )?;
                let req: RunRequest = serde_json::from_str(&text)?;
                let harness = session.create_container(&harness_spec(&req, n, image, volume, m))?;
                // the harness's output is ours; its exit code goes to the inside via the volume
                let _ = harness.start(Sink::Inherit, Sink::Inherit)?.wait();
                n += 1;
                continue;
            }
            std::thread::sleep(Duration::from_millis(400));
        }
    }

    /// The egress container has stopped, or cannot be reached: its `done` is still in the
    /// volume if it finished properly, readable from a throwaway container.
    fn after_exit(
        session: &Session,
        running: &Running,
        volume: &str,
        image: &str,
        why: &str,
    ) -> Fallible<i32> {
        let exited = running.wait_timeout(Duration::from_secs(5));
        let reader = session.create_container(&reader_spec(image, volume, DONE))?;
        let r = reader.start(Sink::Capture, Sink::Discard)?;
        if r.wait() == 0 {
            if let Ok(code) = String::from_utf8_lossy(&r.output().0).trim().parse::<i32>() {
                return Ok(code);
            }
        }
        Err(match exited {
            Some(code) => format!("the egress container stopped early, exit {code}: {why}"),
            None => format!("the egress container stopped answering: {why}"),
        }
        .into())
    }

    /// Prove the API end to end: load it, take a slot, provision `image`, and run one
    /// container from it with no network, its output and exit code back.
    ///
    /// # Errors
    ///
    /// As [`run`].
    pub(crate) fn check(storage: &Path, image: &str, refresh: bool) -> Fallible<String> {
        let api = Api::load()?;
        let version = api.version()?;
        let mut t = Phases::default();
        let (session, slot) = open_slot(&api, storage)?;
        t.mark("session");
        ensure_image(&session, image, refresh, &slot)?;
        t.mark("vm and image");
        let c = session.create_container(&probe_spec(image))?;
        t.mark("create");
        let r = c.start(Sink::Capture, Sink::Capture)?;
        let code = r.wait();
        t.mark("run");
        let out = String::from_utf8_lossy(&r.output().0).trim().to_string();
        drop((r, c, session));
        t.mark("release");
        Ok(format!(
            "wslcsdk {version}; slot {}; {image} ran: {out:?}, exit {code}\n{t}",
            slot.display(),
        ))
    }

    /// A container that says who it runs as and exits 3, with no network.
    fn probe_spec(image: &str) -> ContainerSpec {
        ContainerSpec {
            image: image.to_string(),
            name: None,
            networking: Networking::None,
            binds: Vec::new(),
            mounts: Vec::new(),
            init: ProcessSpec {
                argv: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "echo up as $(id -u); exit 3".into(),
                ],
                env: Vec::new(),
                cwd: None,
            },
            auto_remove: true,
        }
    }

    /// How long each step took.
    struct Phases {
        last: std::time::Instant,
        started: std::time::Instant,
        steps: Vec<(&'static str, Duration)>,
    }

    impl Default for Phases {
        fn default() -> Self {
            let now = std::time::Instant::now();
            Phases {
                last: now,
                started: now,
                steps: Vec::new(),
            }
        }
    }

    impl Phases {
        fn mark(&mut self, step: &'static str) {
            let now = std::time::Instant::now();
            self.steps.push((step, now - self.last));
            self.last = now;
        }
    }

    impl std::fmt::Display for Phases {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            for (step, d) in &self.steps {
                write!(f, "{step} {d:.1?}; ")?;
            }
            write!(f, "total {:.1?}", self.last - self.started)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dm_sandbox::{AuthMode, Harness};
    use std::collections::BTreeMap;

    fn mounts() -> Mounts {
        Mounts {
            root: r"C:\memory".into(),
            work: r"C:\proj".into(),
            home: r"C:\memory\state\sandbox\claude".into(),
            projection: Some(r"C:\memory\state\projection\claude\run-7".into()),
            host_home: Some((r"C:\Users\me".into(), vec![".claude".into()])),
        }
    }

    fn launch() -> Launch {
        Launch {
            projection: "run-7".into(),
            root: r"C:\memory".into(),
            harness: Harness::Claude,
            auth: AuthMode::Subscription,
            provider: None,
            server_auth: None,
            server_provider: None,
            workdir: r"C:\proj".into(),
            image: "img:dev".into(),
            inside_args: vec!["--prompt".into(), "hi".into()],
            interactive: false,
            via: wslc::Via::Api,
            storage: None,
        }
    }

    #[test]
    fn a_harness_container_has_no_network_runs_as_the_harness_user_and_is_removed() {
        let req = RunRequest {
            argv: vec![
                "/opt/memory/memory".into(),
                "relay".into(),
                "--".into(),
                "claude".into(),
            ],
            env: BTreeMap::from([("HOME".to_string(), "/home/user".to_string())]),
            cwd: "/work".into(),
            home: "state/sandbox/claude".into(),
            interactive: false,
        };
        let s = harness_spec(&req, 2, "img:dev", "vol", &mounts());
        assert_eq!(s.networking, Networking::None);
        assert!(s.auto_remove);
        assert_eq!(s.init.cwd.as_deref(), Some("/work"));
        assert_eq!(
            s.init.env,
            vec![("HOME".to_string(), "/home/user".to_string())]
        );
        assert_eq!(
            &s.init.argv[..4],
            [
                "/usr/bin/setpriv",
                "--reuid=1000",
                "--regid=1000",
                "--clear-groups"
            ]
        );
        assert_eq!(&s.init.argv[4..6], ["/bin/sh", "-c"]);
        assert!(
            s.init.argv[6].ends_with("> /run/memory/req-2.out; echo $? > /run/memory/req-2.exit"),
            "{}",
            s.init.argv[6]
        );
        assert_eq!(&s.init.argv[7..], &req.argv[..]);
        assert_eq!(s.mounts, vec![shared("vol")]);
        let projection = s
            .binds
            .iter()
            .find(|b| b.container == INSIDE_PROJECTION)
            .unwrap();
        assert!(projection.read_only, "the projection is read-only");
        assert_eq!(projection.host, r"C:\memory\state\projection\claude\run-7");
        assert!(
            s.binds.iter().all(|b| b.host != mounts().root),
            "never the record root, only its projection"
        );
        let none = harness_spec(
            &req,
            0,
            "img:dev",
            "vol",
            &Mounts {
                projection: None,
                ..mounts()
            },
        );
        assert!(none.binds.iter().all(|b| b.container != INSIDE_PROJECTION));
    }

    #[test]
    fn the_egress_container_has_the_record_the_volume_the_logins_read_only_and_the_bridge() {
        let s = egress_spec(
            &launch(),
            "eg",
            "vol",
            &mounts(),
            &[("K".into(), "v".into())],
        );
        assert_eq!(s.networking, Networking::Bridged);
        assert!(!s.auto_remove);
        assert_eq!(s.name.as_deref(), Some("eg"));
        assert_eq!(
            s.binds[0],
            Bind {
                host: r"C:\memory".into(),
                container: "/memory".into(),
                read_only: false
            }
        );
        let login = &s.binds[1];
        assert!(
            login.read_only && login.container == "/host-home/.claude",
            "{login:?}"
        );
        assert!(login.host.ends_with(".claude"));
        assert_eq!(s.mounts, vec![shared("vol")]);
        assert_eq!(
            s.init.argv[..4],
            ["/opt/memory/memory", "--root", "/memory", "inside"]
        );
        assert!(s
            .init
            .argv
            .ends_with(&["--prompt".to_string(), "hi".to_string()]));
        assert!(s.init.env.contains(&("K".to_string(), "v".to_string())));
    }

    #[test]
    fn a_reader_cats_one_file_from_the_volume_with_no_network() {
        let r = reader_spec("img", "vol", "done");
        assert_eq!(r.init.argv, ["cat", "/run/memory/done"]);
        assert_eq!(r.networking, Networking::None);
    }

    fn info(name: &str, id: &str) -> ImageInfo {
        ImageInfo {
            name: name.into(),
            id: id.into(),
            size_bytes: 1,
        }
    }

    #[test]
    fn the_store_is_trusted_and_the_command_line_asked_only_when_it_lacks_the_image_or_on_refresh()
    {
        let store = [info("durable-memory/harness:dev", "aa11")];
        let img = "durable-memory/harness:dev";
        let present = Provision::Present { id: "aa11".into() };
        let never = || -> Option<String> { panic!("the command line is not asked") };
        assert_eq!(provision(img, &store, false, never), present);
        assert_eq!(
            provision(img, &store, true, || Some("sha256:AA11".into())),
            present
        );
        assert_eq!(
            provision(img, &store, true, || Some("sha256:bb22".into())),
            Provision::Load {
                stale: Some("aa11".into())
            },
            "a rebuild replaces the stored image"
        );
        assert_eq!(
            provision(img, &[], false, || Some("bb22".into())),
            Provision::Load { stale: None }
        );
        assert_eq!(
            provision(img, &store, true, || None),
            Provision::Pull,
            "a registry's tag is pulled again"
        );
        assert_eq!(
            provision("ghcr.io/x/harness@sha256:cc", &store, false, || None),
            Provision::Pull
        );
    }

    #[test]
    fn a_session_disk_lives_under_the_root_unless_the_root_is_a_share() {
        assert_eq!(
            storage_for(Path::new(r"C:\memory"), None).unwrap(),
            Path::new(r"C:\memory").join("state").join("wslc-api")
        );
        let e = storage_for(Path::new(r"\\wsl.localhost\Ubuntu\home\me\memory"), None).unwrap_err();
        assert!(e.contains("--wslc-storage"), "{e}");
        assert_eq!(
            storage_for(Path::new(r"\\wsl.localhost\U\m"), Some(Path::new(r"D:\vm"))).unwrap(),
            Path::new(r"D:\vm")
        );
        assert_eq!(slot_name(3), "durable-memory-3");
    }

    fn err(hr: u32) -> dm_wslc_api::Error {
        dm_wslc_api::Error {
            call: "WslcCreateSession",
            hr,
            message: String::new(),
        }
    }

    #[test]
    fn a_busy_slot_is_passed_over_even_after_a_reclaim_and_any_other_failure_stops() {
        // slot 0: taken by a dead launcher, reclaimed, but its disk is still releasing
        let mut calls = Vec::new();
        let got = take_slot(
            3,
            |n| {
                calls.push(n);
                match (n, calls.iter().filter(|c| **c == n).count()) {
                    (0, 1) => Err(err(ALREADY_EXISTS)),
                    (0, _) => Err(err(SHARING_VIOLATION)),
                    _ => Ok(format!("session {n}")),
                }
            },
            |n| n == 0,
        );
        assert_eq!(
            got,
            Ok(Some(("session 1".to_string(), 1))),
            "the next slot, not a failed launch"
        );
        assert_eq!(calls, [0, 0, 1]);
        // a reclaimed slot is taken when it comes free
        let mut first = true;
        let got = take_slot(
            2,
            |_| {
                if std::mem::take(&mut first) {
                    Err(err(ALREADY_EXISTS))
                } else {
                    Ok("s")
                }
            },
            |_| true,
        );
        assert_eq!(got, Ok(Some(("s", 0))));
        // every slot busy, and nothing to reclaim
        assert_eq!(
            take_slot(2, |_| Err::<(), _>(err(ALREADY_EXISTS)), |_| false),
            Ok(None)
        );
        // anything else is the launch's failure
        assert_eq!(
            take_slot(2, |_| Err::<(), _>(err(0x8007_0005)), |_| false),
            Err(err(0x8007_0005))
        );
    }

    #[test]
    fn a_killed_launchs_run_volume_is_swept_and_nothing_else() {
        let files = [
            "memory-run-33700.vhdx",
            "memory-run-41.vhdx",
            "memory-run-41.vhdx.lock",
            "memory-run-99.vhdx",
            "other.vhdx",
            "memory-run-.vhdx",
        ]
        .map(String::from);
        let mut stale = stale_volumes(files, "memory-run-41");
        stale.sort();
        assert_eq!(stale, ["memory-run-33700", "memory-run-99"]);
    }

    #[test]
    fn a_slot_session_is_traced_to_the_process_that_made_it() {
        let listing = "ID   Creator PID   Display Name\n\
                       2    8984          wslc-cli-sstriker\n\
                       24   44972         durable-memory-0\n\
                       25   1200          durable-memory-10\n";
        assert_eq!(creator_of(listing, "durable-memory-0"), Some(44972));
        assert_eq!(
            creator_of(listing, "durable-memory-1"),
            None,
            "not a prefix match"
        );
        assert_eq!(creator_of(listing, "ID"), None);
        assert_eq!(creator_of("", "durable-memory-0"), None);
    }
}
