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
const INSIDE_ROOT: &str = "/memory";
/// The host's login directories, read-only, inside the egress container.
const INSIDE_HOST_HOME: &str = "/host-home";
/// The working tree inside the harness container.
const INSIDE_WORK: &str = "/work";

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
    /// The working tree on this host.
    pub workdir: PathBuf,
    /// The harness image.
    pub image: String,
    /// The flags for `memory inside` that say what to run: `--prompt`, `--work`, and so
    /// on, followed by `--` and the harness's own arguments.
    pub inside_args: Vec<String>,
}

/// The `wslc` command: `wslc.exe` through interop from a WSL distribution, `wslc` on
/// Windows itself.
fn wslc() -> &'static str {
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
fn mount_path(p: &Path) -> Fallible<String> {
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

fn run_ok(args: &[String]) -> Fallible<String> {
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

/// The arguments of the egress container's `wslc run`.
fn egress_run_args(
    l: &Launch,
    name: &str,
    volume: &str,
    root_m: &str,
    host_home: Option<(&str, &[String])>,
) -> Vec<String> {
    let mut a = s(&["run", "-d", "--name", name, "-v"]);
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
    for k in PASS_ENV {
        if let Ok(v) = std::env::var(k) {
            if !v.is_empty() {
                a.push("-e".into());
                a.push(format!("{k}={v}"));
            }
        }
    }
    a.push(l.image.clone());
    a.extend(s(&[
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
        "--harness",
        l.harness.binary(),
        "--auth",
    ]));
    a.push(auth_name(l.auth).into());
    if let Some(p) = &l.provider {
        a.push("--provider".into());
        a.push(p.clone());
    }
    a.extend(l.inside_args.iter().cloned());
    a
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
    a.push("-w".into());
    a.push(req.cwd.clone());
    for (k, v) in &req.env {
        a.push("-e".into());
        a.push(format!("{k}={v}"));
    }
    a.push(image.to_string());
    a.push("/bin/sh".into());
    a.push("-c".into());
    let exit = RunRequest::exit_file(n);
    a.push(if req.interactive {
        format!("\"$0\" \"$@\"; echo $? > {exit}")
    } else {
        format!(
            "\"$0\" \"$@\" > {}; echo $? > {exit}",
            RunRequest::out_file(n)
        )
    });
    a.extend(req.argv.iter().cloned());
    a
}

/// Run a launch through WSL containers. Returns the exit code the inside reports.
///
/// # Errors
///
/// When `wslc` is not there, the image is missing, or a container cannot be started.
pub fn run(l: &Launch) -> Fallible<i32> {
    let pid = std::process::id();
    let volume = format!("memory-run-{pid}");
    let egress = format!("memory-egress-{pid}");
    let root_m = mount_path(&l.root)?;
    let work_m = mount_path(&l.workdir)?;
    let home_dir = l.root.join("state/sandbox").join(l.harness.binary());
    std::fs::create_dir_all(&home_dir)?;
    let home_m = mount_path(&home_dir)?;
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

    run_ok(&s(&["volume", "create", &volume]))?;
    let result = (|| -> Fallible<i32> {
        let args = egress_run_args(
            l,
            &egress,
            &volume,
            &root_m,
            host_home_m.as_deref().map(|m| (m, login_dirs.as_slice())),
        );
        run_ok(&args)?;
        let mut logs = Command::new(wslc())
            .args(["logs", "-f", &egress])
            .stdin(Stdio::null())
            .spawn()?;
        let code = drive(&egress, &volume, &l.image, &home_m, &work_m);
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
    result
}

/// Serve the inside's requests until it writes `done`.
fn drive(egress: &str, volume: &str, image: &str, home_m: &str, work_m: &str) -> Fallible<i32> {
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
            let args = harness_run_args(&req, n, image, volume, home_m, work_m);
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
            r"\\wsl.localhost\U\home\me\memory\state\sandbox\claude",
            r"\\wsl.localhost\U\home\me\proj",
        );
        let joined = a.join(" ");
        assert!(joined.contains("--network none"), "{joined}");
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
        let a = harness_run_args(&request(true), 0, "img:dev", "vol", "h", "w");
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
            workdir: "/home/me/proj".into(),
            image: "img:dev".into(),
            inside_args: vec!["--prompt".into(), "hello".into()],
        };
        let dirs = vec![".codex".to_string()];
        let a = egress_run_args(
            &l,
            "eg",
            "vol",
            "/home/me/memory",
            Some(("/home/me", &dirs)),
        );
        let joined = a.join(" ");
        assert!(
            joined.starts_with("run -d --name eg -v /home/me/memory:/memory -v vol:/run/memory"),
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
            joined.ends_with("--harness codex --auth key --provider xai --prompt hello"),
            "{joined}"
        );
    }

    #[test]
    fn outside_wsl_a_path_is_itself() {
        if std::env::var_os("WSL_DISTRO_NAME").is_some() {
            return;
        }
        assert_eq!(mount_path(Path::new("/x/y")).unwrap(), "/x/y");
    }
}
