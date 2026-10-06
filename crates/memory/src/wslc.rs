//! The `wslc` backend: the harness in the harness image, under WSL containers.
//!
//! Two containers in the session VM. The egress container runs this same binary as
//! `memory inside`, with the record mounted in and the only route outside: it opens the
//! memory, serves the egress on a socket in a shared volume, and does everything the
//! launcher does on Linux, including task mode. The harness container has no network at
//! all, the working tree and its private home mounted in, and reaches the egress through
//! the shared volume. This host side is deliberately dumb: it starts the egress container,
//! then turns each run request the inside writes into one `wslc run`, and reports the
//! exit code the inside leaves behind. Nothing of the record is touched from here.
//!
//! The container is the boundary; the runtime's seccomp filter refuses our own
//! namespaces inside it, and does not need to allow them.

use crate::requests::{RunRequest, DONE, REQUEST_DIR_INSIDE};
use dm_sandbox::{AuthMode, Harness, INSIDE_HOME};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

type Fallible<T> = Result<T, Box<dyn std::error::Error>>;

/// The default image tag; `MEMORY_IMAGE` or `--image` overrides it.
pub const DEFAULT_IMAGE: &str = "durable-memory/harness:dev";

/// The record inside the egress container.
pub(crate) const INSIDE_ROOT: &str = "/memory";
/// The host's login directories, read-only, inside the egress container.
pub(crate) const INSIDE_HOST_HOME: &str = "/host-home";
/// The working tree inside the harness container.
pub(crate) const INSIDE_WORK: &str = "/work";

/// Variables the inside may need, passed to the egress container when set here.
const PASS_ENV: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "GEMINI_API_KEY",
    "XAI_API_KEY",
    "LLMAO_KEY",
    "LLMAO_URL",
    "GH_TOKEN",
    "GOOGLE_CLOUD_PROJECT",
];

/// Login directories copied from, never written.
const LOGIN_DIRS: &[&str] = &[".claude", ".codex", ".gemini"];

/// The gateway inside the session VM, for `--auth gateway`: the same LiteLLM over
/// Postgres as `make local-up` runs on the host, which the VM cannot reach. Both live on a
/// network of their own; the egress container joins it. They are left running between
/// launches, like the host's.
const GATEWAY_NETWORK: &str = "memory-gateway";
const GATEWAY_LITELLM: &str = "memory-litellm";
const GATEWAY_POSTGRES: &str = "memory-postgres";
const LITELLM_IMAGE: &str = "ghcr.io/berriai/litellm:main-stable";
const POSTGRES_IMAGE: &str = "docker.io/library/postgres:16-alpine";
/// The gateway's configuration, the same file the host stack uses.
const LITELLM_CONFIG: &str = include_str!("../../../local/litellm.yaml");

/// What to run.
pub struct Launch {
    /// The record on this host.
    pub root: PathBuf,
    /// Which harness.
    pub harness: Harness,
    /// How it authenticates.
    pub auth: AuthMode,
    /// Another destination, where the harness allows it.
    pub provider: Option<String>,
    /// A credential of their own for the memory's calls, when they have one.
    pub server_auth: Option<AuthMode>,
    /// Where those calls go under a key.
    pub server_provider: Option<String>,
    /// The working tree on this host.
    pub workdir: PathBuf,
    /// The harness image.
    pub image: String,
    /// The flags for `memory inside` that say what to run: `--prompt`, `--work`, and so
    /// on, followed by `--` and the harness's own arguments.
    pub inside_args: Vec<String>,
    /// Attached to the terminal: no prompt and no task.
    pub interactive: bool,
    /// How the host drives WSL containers.
    pub via: Via,
    /// Where the C API's session disks live, when not under the root.
    #[cfg_attr(not(windows), allow(dead_code))] // the C API is Windows only
    pub storage: Option<PathBuf>,
}

/// How the host drives WSL containers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum Via {
    /// The C API on Windows where the launch allows it, the command line otherwise.
    #[default]
    Auto,
    /// The `wslc` command line.
    Cli,
    /// The C API in `wslcsdk.dll`: Windows, and neither interactive nor the gateway.
    Api,
}

/// Which way a launch goes. The API gives a process no terminal and makes no networks,
/// so an interactive launch and the gateway inside the VM need the command line;
/// `auto` picks the API wherever it can serve.
///
/// # Errors
///
/// When the API is asked for and cannot serve this launch.
pub(crate) fn via_for(l: &Launch, windows: bool) -> Result<Via, String> {
    let blocker = if !windows {
        Some(dm_wslc_api::WINDOWS_ONLY)
    } else if l.interactive {
        Some("the C API gives a process no terminal, so an interactive launch goes through the command line")
    } else if wants_gateway(l) {
        Some("the gateway inside the VM needs a network of its own, which the C API cannot make, so --auth gateway goes through the command line")
    } else {
        None
    };
    match (l.via, blocker) {
        (Via::Cli, _) | (Via::Auto, Some(_)) => Ok(Via::Cli),
        (Via::Api | Via::Auto, None) => Ok(Via::Api),
        (Via::Api, Some(why)) => Err(format!(
            "--wslc-via api: {why}; pass --wslc-via cli or auto"
        )),
    }
}
/// The `wslc` command: `wslc.exe` through interop from a WSL distribution, `wslc` on
/// Windows itself.
pub(crate) fn wslc() -> &'static str {
    if which("wslc.exe") {
        "wslc.exe"
    } else {
        "wslc"
    }
}

fn which(name: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(name).is_file()))
}

/// A host path as `wslc` must see it: from inside a WSL distribution that is the UNC
/// name `wslpath -w` gives; on Windows it is the path itself.
pub(crate) fn mount_path(p: &Path) -> Fallible<String> {
    if std::env::var_os("WSL_DISTRO_NAME").is_none() {
        return Ok(p.display().to_string());
    }
    let out = Command::new("wslpath").arg("-w").arg(p).output()?;
    if !out.status.success() {
        return Err(format!(
            "wslpath -w {}: {}",
            p.display(),
            String::from_utf8_lossy(&out.stderr)
        )
        .into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub(crate) fn run_ok(args: &[String]) -> Fallible<String> {
    let out = Command::new(wslc())
        .args(args)
        .stdin(Stdio::null())
        .output()?;
    if !out.status.success() {
        return Err(format!(
            "{} {}: {}",
            wslc(),
            args.first().cloned().unwrap_or_default(),
            String::from_utf8_lossy(&out.stderr).trim()
        )
        .into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| (*x).to_string()).collect()
}

/// The arguments of the egress container's `wslc run`. `extra` is environment the
/// launch adds, such as the gateway's address inside the VM; `network` joins a network.
#[allow(clippy::too_many_arguments)] // one argument per thing the egress container is given
fn egress_run_args(
    l: &Launch,
    name: &str,
    volume: &str,
    root_m: &str,
    projection: &str,
    host_home: Option<(&str, &[String])>,
    extra: &[(String, String)],
    network: Option<&str>,
) -> Vec<String> {
    let mut a = s(&["run", "-d", "--name", name]);
    if let Some(n) = network {
        a.push("--network".into());
        a.push(n.to_string());
    }
    a.push("-v".into());
    a.push(format!("{root_m}:{INSIDE_ROOT}"));
    a.push("-v".into());
    a.push(format!("{volume}:{REQUEST_DIR_INSIDE}"));
    if let Some((home_m, dirs)) = host_home {
        for d in dirs {
            a.push("-v".into());
            a.push(format!(
                "{home_m}{}{d}:{INSIDE_HOST_HOME}/{d}:ro",
                std::path::MAIN_SEPARATOR
            ));
        }
    }
    for (k, v) in egress_env(extra) {
        a.push("-e".into());
        a.push(format!("{k}={v}"));
    }
    a.push(l.image.clone());
    a.extend(inside_command(l));
    a
}

/// The egress container's environment: what `PASS_ENV` names and is set here, then what
/// the launch adds, which wins.
pub(crate) fn egress_env(extra: &[(String, String)]) -> Vec<(String, String)> {
    let mut env = Vec::new();
    for k in PASS_ENV {
        if extra.iter().any(|(ek, _)| ek == k) {
            continue;
        }
        // a GitHub login lives in gh's keyring, where the container cannot ask for it
        let v = std::env::var(k).ok().filter(|v| !v.is_empty());
        let v = if *k == "GH_TOKEN" {
            v.or_else(crate::credentials::gh_token)
        } else {
            v
        };
        if let Some(v) = v {
            env.push(((*k).to_string(), v));
        }
    }
    env.extend(extra.iter().cloned());
    env
}

/// The egress container's command: this binary as `memory inside`, told what to run.
pub(crate) fn inside_command(l: &Launch) -> Vec<String> {
    let mut a = s(&[
        "/opt/memory/memory",
        "--root",
        INSIDE_ROOT,
        "inside",
        "--requests",
        REQUEST_DIR_INSIDE,
        "--host-home",
        INSIDE_HOST_HOME,
        "--workdir-inside",
        INSIDE_WORK,
        "--projection",
        projection,
        "--harness",
        l.harness.binary(),
        "--auth",
    ]);
    a.push(auth_name(l.auth).into());
    if let Some(p) = &l.provider {
        a.push("--provider".into());
        a.push(p.clone());
    }
    if let Some(s) = l.server_auth {
        a.push("--server-auth".into());
        a.push(auth_name(s).into());
    }
    if let Some(p) = &l.server_provider {
        a.push("--server-provider".into());
        a.push(p.clone());
    }
    a.extend(l.inside_args.iter().cloned());
    a
}

/// Whether this launch needs the gateway inside the VM: the harness's route or the
/// memory's own asks for it.
fn wants_gateway(l: &Launch) -> bool {
    l.auth == AuthMode::Gateway || l.server_auth == Some(AuthMode::Gateway)
}

fn auth_name(a: AuthMode) -> &'static str {
    match a {
        AuthMode::Subscription => "subscription",
        AuthMode::Key => "key",
        AuthMode::Gateway => "gateway",
    }
}

/// The arguments of a harness container's `wslc run`, from one request: no network, the
/// harness user, the shared volume, the private home and the working tree, the plan's
/// environment, and a shell wrapper that leaves the exit code (and, when captured, the
/// output) in the shared volume for the inside to read.
fn harness_run_args(
    req: &RunRequest,
    n: usize,
    image: &str,
    volume: &str,
    projection_m: Option<&str>,
    home_m: &str,
    work_m: &str,
) -> Vec<String> {
    let mut a = s(&["run", "--rm"]);
    a.push(if req.interactive {
        "-it".into()
    } else {
        "-i".into()
    });
    a.extend(s(&["--network", "none", "-u", "1000:1000", "-v"]));
    a.push(format!("{volume}:{REQUEST_DIR_INSIDE}"));
    a.push("-v".into());
    a.push(format!("{home_m}:{INSIDE_HOME}"));
    a.push("-v".into());
    a.push(format!("{work_m}:{INSIDE_WORK}"));
    // the projection's path is the host's own, from the launch, never the request's:
    // the request file sits in a volume the harness container can write
    if let Some(projection_m) = projection_m {
        a.push("-v".into());
        a.push(format!(
            "{projection_m}:{}:ro",
            dm_sandbox::INSIDE_PROJECTION
        ));
    }
    a.push("-w".into());
    a.push(req.cwd.clone());
    for (k, v) in &req.env {
        a.push("-e".into());
        a.push(format!("{k}={v}"));
    }
    a.push(image.to_string());
    a.extend(wrapped(req, n));
    a
}

/// A request's command in the shell wrapper that leaves its exit code (and, when
/// captured, its output) in the shared volume for the inside to read.
pub(crate) fn wrapped(req: &RunRequest, n: usize) -> Vec<String> {
    let exit = RunRequest::exit_file(n);
    let script = if req.interactive {
        format!("\"$0\" \"$@\"; echo $? > {exit}")
    } else {
        format!(
            "\"$0\" \"$@\" > {}; echo $? > {exit}",
            RunRequest::out_file(n)
        )
    };
    let mut a = vec!["/bin/sh".to_string(), "-c".to_string(), script];
    a.extend(req.argv.iter().cloned());
    a
}

/// The host home's mount path and the login directories under it that a subscription
/// mode copies from, when there are any; nothing else of the host home is mounted.
pub(crate) fn host_logins() -> Fallible<(Option<String>, Vec<String>)> {
    let host_home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    let login_dirs: Vec<String> = host_home
        .as_ref()
        .map(|h| {
            LOGIN_DIRS
                .iter()
                .filter(|d| h.join(d).is_dir())
                .map(|d| (*d).to_string())
                .collect()
        })
        .unwrap_or_default();
    let host_home_m = match &host_home {
        Some(h) if !login_dirs.is_empty() => Some(mount_path(h)?),
        _ => None,
    };
    Ok((host_home_m, login_dirs))
}

/// The projection's mount path, from the launch and the host's own view of the root,
/// when the launcher rendered one.
pub(crate) fn projection_mount(l: &Launch) -> Fallible<Option<String>> {
    // made here, so the first launch on a root mounts it and the inside's render lands
    // where the container already looks
    let dir = l.root.join(dm_sandbox::PROJECTION).join(l.harness.binary());
    std::fs::create_dir_all(&dir)?;
    Ok(Some(mount_path(&dir)?))
}

/// Run a launch through WSL containers, over the C API or the command line as
/// [`via_for`] decides. Returns the exit code the inside reports.
///
/// # Errors
///
/// When the way asked for cannot serve the launch, `wslc` or the DLL is not there, the
/// image cannot be had, or a container cannot be started.
pub fn run(l: &Launch) -> Fallible<i32> {
    match via_for(l, cfg!(windows))? {
        Via::Api => run_api(l),
        _ => run_cli(l),
    }
}

/// Over the C API. Under `auto`, what keeps the API from serving, a missing DLL or a
/// root with nowhere for a session disk, sends the launch to the command line instead.
#[cfg(windows)]
fn run_api(l: &Launch) -> Fallible<i32> {
    let ready = crate::wslc_api::storage_for(&l.root, l.storage.as_deref())
        .map_err(|e| e.to_string())
        .and_then(|storage| {
            dm_wslc_api::Api::load()
                .map(|api| (api, storage))
                .map_err(|e| e.to_string())
        });
    match ready {
        Ok((api, storage)) => crate::wslc_api::run(l, &api, &storage),
        Err(why) if l.via == Via::Auto => {
            eprintln!("memory: not over the C API ({why}); driving the wslc command line");
            run_cli(l)
        }
        Err(why) => Err(why.into()),
    }
}

#[cfg(not(windows))]
fn run_api(_: &Launch) -> Fallible<i32> {
    Err(dm_wslc_api::WINDOWS_ONLY.into())
}

/// Run a launch through the `wslc` command line.
fn run_cli(l: &Launch) -> Fallible<i32> {
    let pid = std::process::id();
    let volume = format!("memory-run-{pid}");
    let egress = format!("memory-egress-{pid}");
    let root_m = mount_path(&l.root)?;
    let work_m = mount_path(&l.workdir)?;
    let home_dir = l.root.join("state/sandbox").join(l.harness.binary());
    std::fs::create_dir_all(&home_dir)?;
    let home_m = mount_path(&home_dir)?;
    // made here, so the container mounts it before the inside renders into it; the pid
    // alone could come round again on a root a killed run left a render in
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or_default();
    let projection = format!("run-{pid}-{nanos:08x}");
    let projection_host = projection_dir(l, &projection);
    std::fs::create_dir_all(&projection_host)?;
    let projection_m = Some(mount_path(&projection_host)?);
    let (host_home_m, login_dirs) = host_logins()?;

    let (extra, network) = if wants_gateway(l) {
        let key = gateway_up(&l.root)?;
        (
            vec![
                (
                    "LLMAO_URL".to_string(),
                    format!("http://{GATEWAY_LITELLM}:4000"),
                ),
                ("LLMAO_KEY".to_string(), key),
            ],
            Some(GATEWAY_NETWORK),
        )
    } else {
        (Vec::new(), None)
    };
    run_ok(&s(&["volume", "create", &volume]))?;
    let result = (|| -> Fallible<i32> {
        let args = egress_run_args(
            l,
            &egress,
            &volume,
            &root_m,
            &projection,
            host_home_m.as_deref().map(|m| (m, login_dirs.as_slice())),
            &extra,
            network,
        );
        run_ok(&args)?;
        let mut logs = Command::new(wslc())
            .args(["logs", "-f", &egress])
            .stdin(Stdio::null())
            .spawn()?;
        let code = drive(
            &egress,
            &volume,
            &l.image,
            projection_m.as_deref(),
            &home_m,
            &work_m,
        );
        logs.kill().ok();
        logs.wait().ok();
        code
    })();
    Command::new(wslc())
        .args(["remove", "-f", &egress])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok();
    Command::new(wslc())
        .args(["volume", "remove", &volume])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok();
    std::fs::remove_dir_all(&projection_host).ok();
    result
}

fn container_state(name: &str) -> Option<String> {
    run_ok(&s(&["inspect", "--format", "{{.State.Status}}", name]))
        .ok()
        .map(|t| t.trim().to_string())
}

/// Bring the gateway up inside the VM if it is not already, and return the key the egress
/// uses against it. Locally that is the master key, as `make local-key` would mint a
/// virtual key from; a per-session virtual key is for the Foundation's front desk.
fn gateway_up(root: &Path) -> Fallible<String> {
    let master = std::env::var("LITELLM_MASTER_KEY")
        .ok()
        .filter(|k| !k.is_empty())
        .ok_or("--auth gateway inside the VM needs LITELLM_MASTER_KEY (from local/.env)")?;
    let pg_password = std::env::var("POSTGRES_PASSWORD").unwrap_or_else(|_| "litellm".into());
    if run_ok(&s(&["network", "inspect", GATEWAY_NETWORK])).is_err() {
        run_ok(&s(&["network", "create", GATEWAY_NETWORK]))?;
    }
    ensure_running(GATEWAY_POSTGRES, || {
        let mut a = s(&[
            "run",
            "-d",
            "--name",
            GATEWAY_POSTGRES,
            "--network",
            GATEWAY_NETWORK,
        ]);
        a.extend(s(&[
            "-e",
            "POSTGRES_DB=litellm",
            "-e",
            "POSTGRES_USER=litellm",
        ]));
        a.push("-e".into());
        a.push(format!("POSTGRES_PASSWORD={pg_password}"));
        a.push(POSTGRES_IMAGE.into());
        a
    })?;
    let config_dir = root.join("state/gateway");
    std::fs::create_dir_all(&config_dir)?;
    std::fs::write(config_dir.join("litellm.yaml"), LITELLM_CONFIG)?;
    let config_m = mount_path(&config_dir)?;
    ensure_running(GATEWAY_LITELLM, || {
        let mut a = s(&[
            "run",
            "-d",
            "--name",
            GATEWAY_LITELLM,
            "--network",
            GATEWAY_NETWORK,
        ]);
        a.push("-v".into());
        a.push(format!("{config_m}:/etc/litellm"));
        a.push("-e".into());
        a.push(format!("LITELLM_MASTER_KEY={master}"));
        a.push("-e".into());
        a.push(format!(
            "DATABASE_URL=postgresql://litellm:{pg_password}@{GATEWAY_POSTGRES}:5432/litellm"
        ));
        for k in [
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "GEMINI_API_KEY",
            "XAI_API_KEY",
        ] {
            if let Ok(v) = std::env::var(k) {
                if !v.is_empty() {
                    a.push("-e".into());
                    a.push(format!("{k}={v}"));
                }
            }
        }
        a.push(LITELLM_IMAGE.into());
        a.extend(s(&[
            "--config",
            "/etc/litellm/litellm.yaml",
            "--port",
            "4000",
        ]));
        a
    })?;
    // LiteLLM takes a while to come up; wait for its liveliness probe from inside
    let probe = "import urllib.request,sys; urllib.request.urlopen('http://127.0.0.1:4000/health/liveliness', timeout=3)";
    for _ in 0..60 {
        if run_ok(&s(&["exec", GATEWAY_LITELLM, "python3", "-c", probe])).is_ok() {
            eprintln!("memory: gateway up inside the VM at http://{GATEWAY_LITELLM}:4000");
            return Ok(master);
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    Err("the gateway inside the VM did not come up in two minutes; `wslc logs memory-litellm` says why".into())
}

/// Start a named container if it is not running: create it if absent, start it if it
/// exited.
fn ensure_running(name: &str, create: impl FnOnce() -> Vec<String>) -> Fallible<()> {
    match container_state(name).as_deref() {
        Some("running") => Ok(()),
        Some(_) => run_ok(&s(&["start", name])).map(|_| ()),
        None => run_ok(&create()).map(|_| ()),
    }
}

/// Serve the inside's requests until it writes `done`.
fn drive(
    egress: &str,
    volume: &str,
    image: &str,
    projection_m: Option<&str>,
    home_m: &str,
    work_m: &str,
) -> Fallible<i32> {
    let mut n = 0;
    loop {
        let listing = match run_ok(&s(&["exec", egress, "ls", REQUEST_DIR_INSIDE])) {
            Ok(l) => l,
            Err(e) => {
                // the egress container has exited; its `done` is still in the volume if it
                // finished properly, readable from a throwaway container
                let done = format!("{REQUEST_DIR_INSIDE}/{DONE}");
                let vol = format!("{volume}:{REQUEST_DIR_INSIDE}");
                if let Ok(text) = run_ok(&s(&["run", "--rm", "-v", &vol, image, "cat", &done])) {
                    if let Ok(code) = text.trim().parse::<i32>() {
                        return Ok(code);
                    }
                }
                let logs = run_ok(&s(&["logs", egress])).unwrap_or_default();
                return Err(format!("the egress container stopped early: {e}\n{logs}").into());
            }
        };
        let names: Vec<&str> = listing.lines().map(str::trim).collect();
        if names.contains(&DONE) {
            let text = run_ok(&s(&[
                "exec",
                egress,
                "cat",
                &format!("{REQUEST_DIR_INSIDE}/{DONE}"),
            ]))?;
            return Ok(text.trim().parse().unwrap_or(1));
        }
        let req_name = RunRequest::file(n);
        if names.contains(&req_name.as_str()) {
            let text = run_ok(&s(&[
                "exec",
                egress,
                "cat",
                &format!("{REQUEST_DIR_INSIDE}/{req_name}"),
            ]))?;
            let req: RunRequest = serde_json::from_str(&text)?;
            let args = harness_run_args(&req, n, image, volume, projection_m, home_m, work_m);
            // the harness's terminal is ours; its exit code goes to the inside via the volume
            Command::new(wslc()).args(&args).status()?;
            n += 1;
            continue;
        }
        std::thread::sleep(Duration::from_millis(400));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn request(interactive: bool) -> RunRequest {
        RunRequest {
            argv: vec![
                "/opt/memory/memory".into(),
                "relay".into(),
                "--".into(),
                "/usr/local/bin/claude".into(),
                "-p".into(),
                "hi".into(),
            ],
            env: BTreeMap::from([
                ("HOME".to_string(), "/home/user".to_string()),
                ("PATH".to_string(), "/usr/local/bin".to_string()),
            ]),
            cwd: "/work".into(),
            home: "state/sandbox/claude".into(),
            interactive,
        }
    }

    #[test]
    fn a_harness_container_has_no_network_and_leaves_its_exit_code_in_the_volume() {
        let a = harness_run_args(
            &request(false),
            3,
            "img:dev",
            "vol",
            Some(r"\\wsl.localhost\U\home\me\memory\state\projection\claude"),
            r"\\wsl.localhost\U\home\me\memory\state\sandbox\claude",
            r"\\wsl.localhost\U\home\me\proj",
        );
        let joined = a.join(" ");
        assert!(joined.contains("--network none"), "{joined}");
        assert!(
            joined.contains(r"\projection\claude:/memory:ro"),
            "the projection is mounted read-only, from the host's own path: {joined}"
        );
        assert!(joined.contains("-u 1000:1000"), "{joined}");
        assert!(joined.contains("vol:/run/memory"), "{joined}");
        assert!(joined.contains(r"\proj:/work"), "{joined}");
        assert!(joined.contains("-e HOME=/home/user"), "{joined}");
        assert!(
            joined.contains("> /run/memory/req-3.out; echo $? > /run/memory/req-3.exit"),
            "captured: {joined}"
        );
        assert!(a.contains(&"-i".to_string()) && !a.contains(&"-it".to_string()));
        assert_eq!(&a[a.len() - 6..], &request(false).argv[..]);
        let a = harness_run_args(&request(true), 0, "img:dev", "vol", None, "h", "w");
        assert!(
            !a.join(" ").contains(":/memory"),
            "nothing rendered, nothing mounted"
        );
        let joined = a.join(" ");
        assert!(a.contains(&"-it".to_string()));
        assert!(
            !joined.contains("req-0.out"),
            "interactive output stays on the terminal: {joined}"
        );
        assert!(joined.contains("echo $? > /run/memory/req-0.exit"));
    }

    #[test]
    fn the_egress_container_gets_the_record_the_volume_and_the_logins_read_only() {
        let l = Launch {
            root: "/home/me/memory".into(),
            harness: Harness::Codex,
            auth: AuthMode::Key,
            provider: Some("xai".into()),
            server_auth: Some(AuthMode::Gateway),
            server_provider: Some("llmao".into()),
            workdir: "/home/me/proj".into(),
            image: "img:dev".into(),
            inside_args: vec!["--prompt".into(), "hello".into()],
            interactive: false,
            via: Via::Auto,
            storage: None,
        };
        let dirs = vec![".codex".to_string()];
        let a = egress_run_args(
            &l,
            "eg",
            "vol",
            "/home/me/memory",
            "run-7",
            Some(("/home/me", &dirs)),
            &[(
                "LLMAO_URL".to_string(),
                "http://memory-litellm:4000".to_string(),
            )],
            Some("memory-gateway"),
        );
        let joined = a.join(" ");
        assert!(
            joined.starts_with(
                "run -d --name eg --network memory-gateway -v /home/me/memory:/memory -v vol:/run/memory"
            ),
            "{joined}"
        );
        assert!(
            joined.contains("-e LLMAO_URL=http://memory-litellm:4000"),
            "{joined}"
        );
        assert!(
            joined.contains(&format!(
                "/home/me{}.codex:/host-home/.codex:ro",
                std::path::MAIN_SEPARATOR
            )),
            "{joined}"
        );
        assert!(
            joined.contains(
                "img:dev /opt/memory/memory --root /memory inside --requests /run/memory"
            ),
            "{joined}"
        );
        assert!(
            joined.contains("--projection run-7 --harness codex"),
            "the inside renders under the name the outside mounts: {joined}"
        );
        assert!(
            joined.ends_with(
                "--harness codex --auth key --provider xai --server-auth gateway \
                 --server-provider llmao --prompt hello"
            ),
            "{joined}"
        );
        // the server's route alone wanting the gateway is enough to bring it up
        assert!(wants_gateway(&l));
        assert!(!wants_gateway(&Launch {
            server_auth: None,
            server_provider: None,
            ..l
        }));
    }

    #[test]
    fn the_api_serves_where_it_can_and_says_why_where_it_cannot() {
        let l = l_default();
        assert_eq!(via_for(&l, true), Ok(Via::Api));
        assert_eq!(
            via_for(&l, false),
            Ok(Via::Cli),
            "off Windows, the command line"
        );
        let interactive = Launch {
            interactive: true,
            ..l
        };
        assert_eq!(via_for(&interactive, true), Ok(Via::Cli));
        let asked = Launch {
            via: Via::Api,
            ..interactive
        };
        assert!(via_for(&asked, true).unwrap_err().contains("no terminal"));
        let gateway = Launch {
            interactive: false,
            server_auth: Some(AuthMode::Gateway),
            ..asked
        };
        assert!(via_for(&gateway, true)
            .unwrap_err()
            .contains("network of its own"));
        assert!(via_for(
            &Launch {
                server_auth: None,
                ..gateway
            },
            false
        )
        .unwrap_err()
        .contains("Windows only"));
        let cli = Launch {
            via: Via::Cli,
            ..l_default()
        };
        assert_eq!(
            via_for(&cli, true),
            Ok(Via::Cli),
            "asked for, always served"
        );
    }

    fn l_default() -> Launch {
        Launch {
            root: "/m".into(),
            harness: Harness::Claude,
            auth: AuthMode::Key,
            provider: None,
            server_auth: None,
            server_provider: None,
            workdir: "/w".into(),
            image: "img".into(),
            inside_args: Vec::new(),
            interactive: false,
            via: Via::Auto,
            storage: None,
        }
    }

    #[test]
    fn outside_wsl_a_path_is_itself() {
        if std::env::var_os("WSL_DISTRO_NAME").is_some() {
            return;
        }
        assert_eq!(mount_path(Path::new("/x/y")).unwrap(), "/x/y");
    }
}
