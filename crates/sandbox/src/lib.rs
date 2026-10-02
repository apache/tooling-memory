//! Confinement for a harness: what it may see, and the one way out.
//!
//! A [`Plan`] says what goes into the sandbox: a root with the system's binaries read-only,
//! a private home, the working tree, the memory binary, the egress socket, and nothing
//! else. The network namespace is empty. The harness reaches the egress through a relay
//! on loopback that forwards to the socket, and the egress is the only process with the
//! network.
//!
//! Backends:
//! - Linux: user, mount, network, uts, ipc and pid namespaces, native, unprivileged, no
//!   bubblewrap or container runtime needed.
//! - macOS: not yet; the plan is Seatbelt. Until then `run` refuses rather than pretend.
//! - Windows: run under WSL2.
//!
//! The same plan feeds every backend; the difference is how it is enforced.

use dm_core::Label;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
pub mod linux;

/// What can go wrong preparing or running a sandbox.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A file under the sandbox home could not be written.
    #[error("{path}: {source}")]
    Io {
        /// The path.
        path: String,
        /// Why.
        source: std::io::Error,
    },
    /// The harness binary could not be found on PATH.
    #[error("cannot find {0} on PATH")]
    NoHarness(String),
    /// The combination of harness and auth is not supported.
    #[error("{0}")]
    Unsupported(String),
    /// This platform has no confinement backend yet.
    #[error("no confinement backend on this platform; run under WSL2 or Linux")]
    NoBackend,
    /// The confinement itself failed.
    #[error("confinement: {0}")]
    Confine(String),
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> Error + '_ {
    move |source| Error::Io {
        path: path.display().to_string(),
        source,
    }
}

/// Which harness to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Harness {
    /// Claude Code.
    Claude,
    /// OpenAI Codex.
    Codex,
}

impl Harness {
    /// The binary name on PATH.
    #[must_use]
    pub fn binary(self) -> &'static str {
        match self {
            Harness::Claude => "claude",
            Harness::Codex => "codex",
        }
    }

    /// The destination this harness's model calls go to.
    #[must_use]
    pub fn provider(self) -> &'static str {
        match self {
            Harness::Claude => "anthropic",
            Harness::Codex => "openai",
        }
    }
}

/// How the harness's model calls are authenticated.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthMode {
    /// Through the local LiteLLM gateway with a virtual key the egress injects.
    Gateway,
    /// Straight to the provider with an API key the egress injects.
    Key,
    /// Straight to the provider with the harness's own subscription login, which it
    /// holds; the egress passes it through.
    Subscription,
}

/// One bind mount into the sandbox.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mount {
    /// On the host.
    pub source: PathBuf,
    /// Inside the sandbox.
    pub target: PathBuf,
    /// Read-only.
    pub read_only: bool,
}

/// Everything a backend needs to run one confined session.
#[derive(Clone, Debug)]
pub struct Plan {
    /// Bind mounts, applied in order. Parents must come before children.
    pub mounts: Vec<Mount>,
    /// The environment inside, complete; nothing is inherited.
    pub env: BTreeMap<String, String>,
    /// The command inside: the relay, then the harness.
    pub argv: Vec<String>,
    /// Working directory inside.
    pub cwd: PathBuf,
    /// The hostname inside.
    pub hostname: String,
    /// The label this session's reads are capped at, from the destination's ceiling.
    pub handling_cap: Label,
}

/// The inputs to a plan.
pub struct Request<'a> {
    /// Which harness.
    pub harness: Harness,
    /// How it authenticates.
    pub auth: AuthMode,
    /// The working tree to mount read-write.
    pub workdir: &'a Path,
    /// Where the sandbox's private home lives on the host, persisted between sessions.
    pub home: &'a Path,
    /// The egress unix socket on the host.
    pub egress_socket: &'a Path,
    /// The `memory` binary on the host, mounted in for the relay.
    pub memory_binary: &'a Path,
    /// The destination's ceiling, which caps the session's handling.
    pub destination_ceiling: Label,
    /// Arguments after the harness binary.
    pub harness_args: &'a [String],
    /// The host's `~/.claude/.credentials.json`, copied in for subscription mode.
    pub claude_credentials: Option<&'a Path>,
}

/// Where things sit inside the sandbox.
pub const INSIDE_HOME: &str = "/home/user";
/// The egress socket inside.
pub const INSIDE_SOCKET: &str = "/run/memory/egress.sock";
/// The memory binary inside.
pub const INSIDE_MEMORY: &str = "/opt/memory/memory";
/// The loopback address the relay listens on inside.
pub const INSIDE_RELAY: &str = "127.0.0.1:4100";

fn write(path: &Path, text: &str) -> Result<(), Error> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).map_err(io(p))?;
    }
    std::fs::write(path, text).map_err(io(path))
}

fn find_on_path(name: &str) -> Result<PathBuf, Error> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
        .and_then(|p| std::fs::canonicalize(p).ok())
        .ok_or_else(|| Error::NoHarness(name.to_string()))
}

/// Directories of the host system mounted read-only so binaries run.
fn system_mounts() -> Vec<Mount> {
    let mut m = Vec::new();
    for d in [
        "/usr",
        "/bin",
        "/sbin",
        "/lib",
        "/lib64",
        "/lib32",
        "/etc/alternatives",
        "/etc/ssl",
    ] {
        let p = PathBuf::from(d);
        if p.exists() && !p.is_symlink() {
            m.push(Mount {
                source: p.clone(),
                target: p,
                read_only: true,
            });
        }
    }
    for f in [
        "/etc/ld.so.cache",
        "/etc/localtime",
        "/etc/passwd",
        "/etc/group",
        "/etc/nsswitch.conf",
        "/etc/hosts",
    ] {
        let p = PathBuf::from(f);
        if p.exists() {
            m.push(Mount {
                source: p.clone(),
                target: p,
                read_only: true,
            });
        }
    }
    m
}

/// Build the plan: write the harness's configuration into the private home and decide
/// mounts, environment, and command.
///
/// # Errors
///
/// A harness not on PATH, an unsupported harness and auth combination, or a file under
/// the home that cannot be written.
pub fn prepare(req: &Request<'_>) -> Result<Plan, Error> {
    let harness_bin = find_on_path(req.harness.binary())?;
    std::fs::create_dir_all(req.home).map_err(io(req.home))?;
    let inside_home = PathBuf::from(INSIDE_HOME);
    let relay = format!("http://{INSIDE_RELAY}");

    let mut env: BTreeMap<String, String> = BTreeMap::new();
    env.insert("HOME".into(), INSIDE_HOME.into());
    env.insert("USER".into(), "user".into());
    env.insert("LOGNAME".into(), "user".into());
    env.insert(
        "PATH".into(),
        "/opt/memory:/usr/local/bin:/usr/bin:/bin".into(),
    );
    env.insert("TMPDIR".into(), "/tmp".into());
    for k in [
        "TERM",
        "COLORTERM",
        "LANG",
        "LC_ALL",
        "NO_COLOR",
        "FORCE_COLOR",
    ] {
        if let Ok(v) = std::env::var(k) {
            env.insert(k.into(), v);
        }
    }
    env.entry("TERM".into())
        .or_insert_with(|| "xterm-256color".into());

    let mut mounts = system_mounts();
    mounts.push(Mount {
        source: req.memory_binary.to_path_buf(),
        target: PathBuf::from(INSIDE_MEMORY),
        read_only: true,
    });
    // The harness binary and whatever it symlinks into, read-only, at its real path.
    let harness_real = harness_bin.clone();
    if let Some(dir) = harness_real.parent() {
        if !dir.starts_with("/usr") && !dir.starts_with("/bin") {
            mounts.push(Mount {
                source: dir.to_path_buf(),
                target: dir.to_path_buf(),
                read_only: true,
            });
        }
    }
    mounts.push(Mount {
        source: req.home.to_path_buf(),
        target: inside_home.clone(),
        read_only: false,
    });
    mounts.push(Mount {
        source: req.workdir.to_path_buf(),
        target: req.workdir.to_path_buf(),
        read_only: false,
    });
    mounts.push(Mount {
        source: req.egress_socket.to_path_buf(),
        target: PathBuf::from(INSIDE_SOCKET),
        read_only: false,
    });

    let mut argv = vec![
        INSIDE_MEMORY.to_string(),
        "relay".into(),
        "--socket".into(),
        INSIDE_SOCKET.into(),
        "--listen".into(),
        INSIDE_RELAY.into(),
        "--".into(),
        harness_bin.display().to_string(),
    ];

    setup_harness(req, &relay, &mut env, &mut argv)?;
    argv.extend(req.harness_args.iter().cloned());

    Ok(Plan {
        mounts,
        env,
        argv,
        cwd: req.workdir.to_path_buf(),
        hostname: "memory-sandbox".into(),
        handling_cap: req.destination_ceiling,
    })
}

/// Write the harness's configuration into the private home and set its environment and
/// arguments. The relay URL is where it reaches the egress.
fn setup_harness(
    req: &Request<'_>,
    relay: &str,
    env: &mut BTreeMap<String, String>,
    argv: &mut Vec<String>,
) -> Result<(), Error> {
    match req.harness {
        Harness::Claude => {
            let mcp = serde_json::json!({
                "mcpServers": { "memory": { "type": "http", "url": format!("{relay}/mcp") } }
            });
            write(&req.home.join(".claude-mcp.json"), &mcp.to_string())?;
            claude_global_config(&req.home.join(".claude.json"), req.workdir)?;
            env.insert("ANTHROPIC_BASE_URL".into(), format!("{relay}/anthropic"));
            env.insert("DISABLE_AUTOUPDATER".into(), "1".into());
            match req.auth {
                AuthMode::Subscription => {
                    let Some(src) = req.claude_credentials else {
                        return Err(Error::Unsupported(
                            "subscription mode needs ~/.claude/.credentials.json from a Claude Code login".into(),
                        ));
                    };
                    let dst = req.home.join(".claude/.credentials.json");
                    if let Some(p) = dst.parent() {
                        std::fs::create_dir_all(p).map_err(io(p))?;
                    }
                    std::fs::copy(src, &dst).map_err(io(&dst))?;
                }
                AuthMode::Gateway | AuthMode::Key => {
                    // the egress injects the real credential; this one only has to exist
                    env.insert("ANTHROPIC_AUTH_TOKEN".into(), "egress".into());
                }
            }
            argv.push("--mcp-config".into());
            argv.push(format!("{INSIDE_HOME}/.claude-mcp.json"));
            argv.push("--strict-mcp-config".into());
        }
        Harness::Codex => {
            if req.auth == AuthMode::Subscription {
                return Err(Error::Unsupported(
                    "codex with a ChatGPT login routes to chatgpt.com, which the egress does not proxy yet; use --auth key or --auth gateway".into(),
                ));
            }
            let config = format!(
                r#"# written by memory launch; edits are overwritten
model_provider = "egress"

[model_providers.egress]
name = "egress"
base_url = "{relay}/openai/v1"
env_key = "DM_EGRESS_TOKEN"
wire_api = "responses"

[mcp_servers.memory]
url = "{relay}/mcp"
"#
            );
            write(&req.home.join(".codex/config.toml"), &config)?;
            env.insert("CODEX_HOME".into(), format!("{INSIDE_HOME}/.codex"));
            env.insert("DM_EGRESS_TOKEN".into(), "egress".into());
        }
    }
    Ok(())
}

/// Claude Code's global config inside: onboarding done, the working tree trusted, so no
/// dialog blocks a confined session. Merged into an existing file.
fn claude_global_config(path: &Path, workdir: &Path) -> Result<(), Error> {
    let mut v: serde_json::Value = std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    v["hasCompletedOnboarding"] = serde_json::Value::Bool(true);
    v["projects"][workdir.display().to_string()]["hasTrustDialogAccepted"] =
        serde_json::Value::Bool(true);
    write(path, &serde_json::to_string_pretty(&v).unwrap_or_default())
}

/// Run a plan under this platform's backend, returning the harness's exit code.
///
/// # Errors
///
/// [`Error::NoBackend`] off Linux, or a confinement failure.
pub fn run(plan: &Plan) -> Result<i32, Error> {
    #[cfg(target_os = "linux")]
    {
        linux::run(plan)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = plan;
        Err(Error::NoBackend)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        _dir: tempfile::TempDir,
        bins: PathBuf,
        work: PathBuf,
        sock: PathBuf,
        memory_bin: PathBuf,
        creds: PathBuf,
        root: PathBuf,
    }

    fn fake_bin(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        p
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let bins = dir.path().join("bins");
        std::fs::create_dir_all(&bins).unwrap();
        fake_bin(&bins, "claude");
        fake_bin(&bins, "codex");
        let memory_bin = fake_bin(&bins, "memory");
        let work = dir.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        let sock = dir.path().join("egress.sock");
        std::fs::write(&sock, "").unwrap();
        let creds = dir.path().join("creds.json");
        std::fs::write(&creds, r#"{"claudeAiOauth":{"accessToken":"x"}}"#).unwrap();
        let root = dir.path().to_path_buf();
        Fixture {
            _dir: dir,
            bins,
            work,
            sock,
            memory_bin,
            creds,
            root,
        }
    }

    impl Fixture {
        fn plan(
            &self,
            harness: Harness,
            auth: AuthMode,
            home: &str,
            args: &[String],
            creds: bool,
        ) -> Result<(PathBuf, Plan), Error> {
            let home = self.root.join(home);
            let old = std::env::var_os("PATH").unwrap_or_default();
            let mut paths = vec![self.bins.clone()];
            paths.extend(std::env::split_paths(&old));
            std::env::set_var("PATH", std::env::join_paths(paths).unwrap());
            let r = prepare(&Request {
                harness,
                auth,
                workdir: &self.work,
                home: &home,
                egress_socket: &self.sock,
                memory_binary: &self.memory_bin,
                destination_ceiling: if auth == AuthMode::Subscription {
                    Label::Internal
                } else {
                    Label::Public
                },
                harness_args: args,
                claude_credentials: creds.then_some(self.creds.as_path()),
            });
            std::env::set_var("PATH", old);
            r.map(|p| (home, p))
        }
    }

    #[test]
    fn claude_plan_env_and_config() {
        let f = fixture();
        let args = vec!["-p".to_string(), "hello".to_string()];
        let (home, plan) = f
            .plan(Harness::Claude, AuthMode::Subscription, "h", &args, true)
            .unwrap();
        assert_eq!(
            plan.env["ANTHROPIC_BASE_URL"],
            "http://127.0.0.1:4100/anthropic"
        );
        assert!(
            !plan.env.contains_key("ANTHROPIC_AUTH_TOKEN"),
            "subscription: the harness's own login"
        );
        assert_eq!(plan.env["HOME"], INSIDE_HOME);
        assert!(
            home.join(".claude/.credentials.json").is_file(),
            "credential copied in"
        );
        let mcp: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(home.join(".claude-mcp.json")).unwrap())
                .unwrap();
        assert_eq!(
            mcp["mcpServers"]["memory"]["url"],
            "http://127.0.0.1:4100/mcp"
        );
        let g: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(home.join(".claude.json")).unwrap())
                .unwrap();
        assert_eq!(
            g["projects"][f.work.display().to_string()]["hasTrustDialogAccepted"],
            true
        );
        assert_eq!(plan.argv[0], INSIDE_MEMORY);
        assert_eq!(plan.argv[1], "relay");
        assert!(plan.argv.contains(&"--strict-mcp-config".to_string()));
        assert_eq!(&plan.argv[plan.argv.len() - 2..], &args[..]);
        assert_eq!(plan.handling_cap, Label::Internal);
    }

    #[test]
    fn claude_plan_mounts() {
        let f = fixture();
        let (_home, plan) = f
            .plan(Harness::Claude, AuthMode::Subscription, "h", &[], true)
            .unwrap();
        let targets: Vec<&Path> = plan.mounts.iter().map(|m| m.target.as_path()).collect();
        assert!(targets.contains(&Path::new(INSIDE_HOME)));
        assert!(targets.contains(&f.work.as_path()));
        assert!(targets.contains(&Path::new(INSIDE_SOCKET)));
        assert!(targets.contains(&Path::new(INSIDE_MEMORY)));
        #[cfg(unix)]
        assert!(plan
            .mounts
            .iter()
            .any(|m| m.target == Path::new("/usr") && m.read_only));
        assert!(!plan
            .mounts
            .iter()
            .any(|m| m.target.starts_with("/etc") && !m.read_only));
        assert!(
            !targets
                .iter()
                .any(|t| t.to_string_lossy().contains(".claude")),
            "the host's ~/.claude is never mounted"
        );
        assert!(
            !Path::new(INSIDE_MEMORY).starts_with("/usr"),
            "the binary mounts outside the read-only /usr"
        );
    }

    #[test]
    fn key_mode_gives_a_placeholder_credential() {
        let f = fixture();
        let (_home, plan) = f
            .plan(Harness::Claude, AuthMode::Key, "h1", &[], false)
            .unwrap();
        assert_eq!(
            plan.env["ANTHROPIC_AUTH_TOKEN"], "egress",
            "a placeholder the egress replaces"
        );
        assert_eq!(plan.handling_cap, Label::Public);
        let (_home, plan) = f
            .plan(Harness::Claude, AuthMode::Gateway, "h2", &[], false)
            .unwrap();
        assert_eq!(plan.env["ANTHROPIC_AUTH_TOKEN"], "egress");
    }

    #[test]
    fn codex_gets_a_config_and_unsupported_modes_are_refused() {
        let f = fixture();
        let (home, plan) = f
            .plan(Harness::Codex, AuthMode::Gateway, "c1", &[], false)
            .unwrap();
        let cfg = std::fs::read_to_string(home.join(".codex/config.toml")).unwrap();
        assert!(
            cfg.contains("base_url = \"http://127.0.0.1:4100/openai/v1\""),
            "{cfg}"
        );
        assert!(cfg.contains("[mcp_servers.memory]"));
        assert_eq!(plan.env["CODEX_HOME"], "/home/user/.codex");
        assert_eq!(plan.env["DM_EGRESS_TOKEN"], "egress");
        assert!(matches!(
            f.plan(Harness::Codex, AuthMode::Subscription, "c2", &[], false),
            Err(Error::Unsupported(_))
        ));
        assert!(
            matches!(
                f.plan(Harness::Claude, AuthMode::Subscription, "c3", &[], false),
                Err(Error::Unsupported(_))
            ),
            "subscription without a credentials file"
        );
    }

    #[test]
    fn a_missing_harness_is_a_clear_error() {
        let dir = tempfile::tempdir().unwrap();
        let empty = dir.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let old = std::env::var_os("PATH").unwrap_or_default();
        std::env::set_var("PATH", &empty);
        let r = prepare(&Request {
            harness: Harness::Codex,
            auth: AuthMode::Key,
            workdir: dir.path(),
            home: &dir.path().join("h"),
            egress_socket: dir.path(),
            memory_binary: dir.path(),
            destination_ceiling: Label::Public,
            harness_args: &[],
            claude_credentials: None,
        });
        std::env::set_var("PATH", old);
        assert!(matches!(r, Err(Error::NoHarness(_))));
    }
}
