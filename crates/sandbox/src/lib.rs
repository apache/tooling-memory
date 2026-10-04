//! Confinement for a harness: what it may see, and the one way out.
//!
//! A [`Plan`] says what goes into the sandbox: a root with the system's binaries read-only,
//! a private home, the working tree, the memory binary, the egress socket, and nothing
//! else. The network namespace is empty. The harness reaches the egress through a relay
//! on loopback that forwards to the socket, and the egress is the only process with the
//! network.
//!
//! Seven harnesses are known. Each needs three things from the launcher, and the adapter
//! for each is the `setup_*` function below:
//!
//! | Harness | Model calls | Memory over MCP | Config the launcher owns |
//! |---|---|---|---|
//! | Claude Code | `ANTHROPIC_BASE_URL` | `--mcp-config` file | `~/.claude.json` |
//! | Codex | `model_providers.<id>.base_url` | `mcp_servers.<id>.url` | `CODEX_HOME/config.toml` |
//! | Gemini CLI | `GOOGLE_GEMINI_BASE_URL` | `mcpServers.<id>.httpUrl` | `~/.gemini/settings.json` |
//! | Antigravity CLI | `GOOGLE_GEMINI_BASE_URL`; a login tunnels | `mcpServers.<id>.serverUrl` | `~/.gemini/antigravity-cli/settings.json`, `~/.gemini/config/` |
//! | Qwen Code | `OPENAI_BASE_URL`; a login tunnels | `mcpServers.<id>.httpUrl` | `~/.qwen/settings.json` |
//! | Copilot CLI | no override; `HTTPS_PROXY` to the egress, which tunnels to allowed hosts | `mcp-config.json` | `COPILOT_HOME` |
//! | OpenCode | `provider.<id>.options.baseURL` | `mcp.<id>.url` | `XDG_CONFIG_HOME/opencode/opencode.json` |
//!
//! Backends:
//! - Linux: user, mount, network, uts, ipc and pid namespaces, native, unprivileged, no
//!   bubblewrap or container runtime needed.
//! - macOS: not yet; the plan is Seatbelt. Until then `run` refuses rather than pretend.
//! - Windows: run under WSL2.

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
    /// The combination of harness, auth and provider is not supported.
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
    /// Google's Gemini CLI.
    Gemini,
    /// Google's Antigravity CLI, `agy`, which replaced Gemini CLI for individual accounts.
    Antigravity,
    /// Alibaba's Qwen Code, a Gemini CLI fork that speaks OpenAI's API.
    Qwen,
    /// GitHub Copilot CLI.
    Copilot,
    /// OpenCode.
    OpenCode,
    /// A program rather than a harness: the first harness argument is run as given, with
    /// the egress and the memory server in its environment and nothing else configured.
    /// For workloads such as a Gofannon agent, which is ordinary Python calling a gateway.
    Command,
}

impl Harness {
    /// The binary name on PATH.
    #[must_use]
    pub fn binary(self) -> &'static str {
        match self {
            Harness::Claude => "claude",
            Harness::Codex => "codex",
            Harness::Gemini => "gemini",
            Harness::Antigravity => "agy",
            Harness::Qwen => "qwen",
            Harness::Copilot => "copilot",
            Harness::OpenCode => "opencode",
            Harness::Command => "command",
        }
    }

    /// The destination this harness's model calls go to by default.
    #[must_use]
    pub fn provider(self) -> &'static str {
        match self {
            Harness::Claude | Harness::OpenCode => "anthropic",
            Harness::Codex | Harness::Command => "openai",
            Harness::Gemini | Harness::Antigravity => "gemini",
            Harness::Qwen => "dashscope",
            Harness::Copilot => "github",
        }
    }

    /// The destination a login, rather than a key, talks to. Differs where the
    /// subscription backend is not the public API: Codex's ChatGPT login talks to
    /// the ChatGPT backend, Gemini's Google login to Code Assist, Qwen Code's Qwen
    /// login to Qwen's own hosts.
    #[must_use]
    pub fn subscription_provider(self) -> &'static str {
        match self {
            Harness::Codex => "chatgpt",
            Harness::Gemini | Harness::Antigravity => "google",
            Harness::Qwen => "qwen",
            other => other.provider(),
        }
    }

    /// The harness's non-interactive form: run this prompt, print the answer, exit.
    /// Each harness's own permission prompts are turned off, since nobody is there to
    /// answer them and a task needs edits and commands, not only memory calls. The
    /// sandbox is what bounds the harness: the working tree, the private home, and the
    /// egress are all it can reach, whatever it is allowed to try.
    #[must_use]
    pub fn headless(self, prompt: &str) -> Vec<String> {
        let p = prompt.to_string();
        match self {
            Harness::Claude => vec!["-p".into(), p, "--dangerously-skip-permissions".into()],
            // codex exec cannot ask, and refuses an MCP tool call that would need
            // approval; its own gate and sandbox are bypassed, since ours is the boundary
            Harness::Codex => vec![
                "exec".into(),
                "--skip-git-repo-check".into(),
                "--dangerously-bypass-approvals-and-sandbox".into(),
                // hooks the launcher wrote are ours to trust; nobody is there to say so
                "--dangerously-bypass-hook-trust".into(),
                p,
            ],
            Harness::Gemini => vec!["-p".into(), p, "--approval-mode".into(), "yolo".into()],
            // agy split Gemini CLI's yolo into two: every permission, and file edits
            Harness::Antigravity => vec![
                "-p".into(),
                p,
                "--dangerously-skip-permissions".into(),
                "--mode".into(),
                "accept-edits".into(),
            ],
            // Qwen Code kept Gemini CLI's yolo, as a flag of its own
            Harness::Qwen => vec!["-p".into(), p, "--yolo".into()],
            Harness::Copilot => vec!["-p".into(), p, "--allow-all-tools".into()],
            Harness::OpenCode => vec!["run".into(), p],
            // a program has no headless form of its own; the prompt is its first argument
            Harness::Command => vec![p],
        }
    }

    /// The flag that picks a model for one run, where the harness has one.
    #[must_use]
    pub fn model_flag(self, model: &str) -> Vec<String> {
        match self {
            Harness::Claude | Harness::Copilot | Harness::Antigravity => {
                vec!["--model".into(), model.into()]
            }
            Harness::Codex | Harness::Gemini | Harness::Qwen | Harness::OpenCode => {
                vec!["-m".into(), model.into()]
            }
            Harness::Command => Vec::new(),
        }
    }

    /// Whether `--provider` may point this harness at another destination.
    #[must_use]
    pub fn accepts_provider(self) -> bool {
        matches!(
            self,
            Harness::Codex | Harness::Qwen | Harness::OpenCode | Harness::Command
        )
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
    /// Straight to the provider with the harness's own login, which it holds; the egress
    /// passes it through.
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
    /// Symlinks to recreate at the root, as `(name, target)`: merged-usr's `/bin ->
    /// usr/bin` and friends, taken from the rootfs the plan runs in.
    pub links: Vec<(PathBuf, PathBuf)>,
}

/// The inputs to a plan.
pub struct Request<'a> {
    /// Which harness.
    pub harness: Harness,
    /// How it authenticates.
    pub auth: AuthMode,
    /// The destination its model calls go to; `None` means the harness's default.
    pub provider: Option<&'a str>,
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
    /// The host's home, read only for the login file a subscription mode copies in:
    /// Claude Code's `.claude/.credentials.json`, Codex's `.codex/auth.json`, Gemini's
    /// `.gemini/oauth_creds.json`, Qwen Code's `.qwen/oauth_creds.json`. Nothing else is
    /// read from it and it is never mounted.
    pub host_home: Option<&'a Path>,
    /// Credentials the harness itself must hold, passed into its environment. For
    /// subscription modes that are a token rather than a file, such as Copilot's
    /// `GH_TOKEN`.
    pub harness_env: &'a [(String, String)],
    /// The harness runs in the harness image rather than from this host's binaries: the
    /// harness is at `/usr/local/bin/<name>`, the memory binary is already inside, and
    /// nothing of the host's system is mounted. Set by the `wslc` backend.
    pub image: bool,
    /// Where the working tree appears inside, when not at its host path. The `wslc`
    /// backend mounts it at `/work`; the namespace backend keeps the host path.
    pub workdir_inside: Option<&'a Path>,
    /// An unpacked harness image to pivot into instead of this host's system: its
    /// `/usr`, `/etc`, `/opt` and the rest are mounted read-only in place of the host's,
    /// so a Linux run is the same environment as a run in the image elsewhere. Implies
    /// `image`.
    pub rootfs: Option<&'a Path>,
    /// The record's projection the launcher rendered for this session, on the host, to
    /// mount read-only at [`INSIDE_PROJECTION`]; `None` when there is none.
    pub projection: Option<&'a Path>,
}

impl Request<'_> {
    /// The working tree's path as the harness sees it.
    #[must_use]
    pub fn workdir_inside(&self) -> &Path {
        self.workdir_inside.unwrap_or(self.workdir)
    }
}

/// Where things sit inside the sandbox.
pub const INSIDE_HOME: &str = "/home/user";
/// The egress socket inside.
pub const INSIDE_SOCKET: &str = "/run/memory/egress.sock";
/// The memory binary inside. Outside `/usr`, which is mounted read-only before this.
pub const INSIDE_MEMORY: &str = "/opt/memory/memory";
/// The loopback address the relay listens on inside.
pub const INSIDE_RELAY: &str = "127.0.0.1:4100";
/// Where the record's projection, the memory as read-only files, appears inside when
/// the launcher rendered one.
pub const INSIDE_PROJECTION: &str = "/memory";
/// Where the launcher renders the projection on the host, relative to the record root,
/// one directory per harness: the launcher's own, never under the private home the
/// harness can write.
pub const PROJECTION: &str = "state/projection";
/// Where hooks leave notes for the launcher, under the private home.
pub const OUTBOX: &str = ".memory-outbox";

fn write(path: &Path, text: &str) -> Result<(), Error> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).map_err(io(p))?;
    }
    std::fs::write(path, text).map_err(io(path))
}

/// Copy one login file from the host's home into the sandbox's private home, at the same
/// relative path. The harness then finds its own login where it expects it.
fn copy_login(req: &Request<'_>, rel: &str, login: &str) -> Result<(), Error> {
    let src = req.host_home.map(|h| h.join(rel)).filter(|p| p.is_file());
    let Some(src) = src else {
        return Err(Error::Unsupported(format!(
            "subscription mode needs ~/{rel} from {login}; run that login on the host first"
        )));
    };
    let dst = req.home.join(rel);
    if let Some(p) = dst.parent() {
        std::fs::create_dir_all(p).map_err(io(p))?;
    }
    std::fs::copy(src, &dst).map_err(io(&dst))?;
    Ok(())
}

/// Point the harness's HTTPS at the egress for what its base-URL override does not
/// cover, such as a token refresh; the egress tunnels only to the hosts the
/// destination allows.
fn proxy_via(relay: &str, env: &mut BTreeMap<String, String>) {
    env.insert("HTTPS_PROXY".into(), relay.to_string());
    env.insert("HTTP_PROXY".into(), relay.to_string());
    env.insert("NO_PROXY".into(), "127.0.0.1,localhost".into());
}

/// A variable from the launcher's environment the harness may see, when it is set.
fn env_passthrough(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn find_on_path(name: &str) -> Result<PathBuf, Error> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
        .and_then(|p| std::fs::canonicalize(p).ok())
        .ok_or_else(|| Error::NoHarness(name.to_string()))
}

/// Directories of the host system mounted read-only so binaries run. `/bin`, `/lib` and
/// friends are symlinks into `/usr` on merged-usr systems; the backend recreates those
/// links inside instead of mounting through them.
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
    system_files(&mut m, Path::new("/"));
    m
}

/// The system from an unpacked image: whole directories, read-only, including `/etc`
/// and `/opt` since the image's `passwd`, certificates and `memory` binary are the ones
/// a run in the image elsewhere would see. Merged-usr symlinks are skipped here and
/// recreated by the backend from [`Plan::links`].
fn rootfs_mounts(rootfs: &Path) -> Vec<Mount> {
    let mut m = Vec::new();
    for d in [
        "usr", "bin", "sbin", "lib", "lib64", "lib32", "etc", "opt", "var",
    ] {
        let p = rootfs.join(d);
        if p.is_dir() && !p.is_symlink() {
            m.push(Mount {
                source: p,
                target: Path::new("/").join(d),
                read_only: true,
            });
        }
    }
    m
}

/// Merged-usr's symlinks at the root of `rootfs`, to recreate inside.
fn root_links(rootfs: &Path) -> Vec<(PathBuf, PathBuf)> {
    ["bin", "sbin", "lib", "lib64", "lib32"]
        .iter()
        .filter_map(|name| {
            std::fs::read_link(rootfs.join(name))
                .ok()
                .map(|t| (PathBuf::from(name), t))
        })
        .collect()
}

fn system_files(m: &mut Vec<Mount>, _root: &Path) {
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
}

/// What to mount so the harness binary runs, and what to add to PATH for it.
///
/// A Node-installed harness lives under `<prefix>/lib/node_modules/...` and is started by
/// `<prefix>/bin/node`, so the whole prefix is mounted and its `bin` goes on PATH. Any
/// other binary gets its own directory mounted, unless that is already under `/usr`.
fn harness_mounts(real: &Path) -> (Vec<Mount>, Option<PathBuf>) {
    let text = real.to_string_lossy();
    if let Some(i) = text.find("/lib/node_modules/") {
        let prefix = PathBuf::from(&text[..i]);
        let m = Mount {
            source: prefix.clone(),
            target: prefix.clone(),
            read_only: true,
        };
        return (vec![m], Some(prefix.join("bin")));
    }
    match real.parent() {
        Some(dir) if !dir.starts_with("/usr") && !dir.starts_with("/bin") => (
            vec![Mount {
                source: dir.to_path_buf(),
                target: dir.to_path_buf(),
                read_only: true,
            }],
            Some(dir.to_path_buf()),
        ),
        _ => (Vec::new(), None),
    }
}

/// The environment every harness starts with: a private home, the terminal's locale and
/// colour settings, and whatever credential the harness must hold itself.
fn base_env(harness_env: &[(String, String)]) -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    env.insert("HOME".into(), INSIDE_HOME.into());
    // where every harness's hooks leave notes for the launcher
    env.insert("MEMORY_OUTBOX".into(), format!("{INSIDE_HOME}/{OUTBOX}"));
    env.insert("USER".into(), "user".into());
    env.insert("LOGNAME".into(), "user".into());
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
    for (k, v) in harness_env {
        env.insert(k.clone(), v.clone());
    }
    env
}

/// Where the projection is inside, when the launcher rendered one: the same read-only
/// mount on every backend, so the hook has one path to know.
fn projection_env(req: &Request<'_>, env: &mut BTreeMap<String, String>) {
    if req.projection.is_some_and(Path::is_dir) {
        env.insert("MEMORY_PROJECTION".into(), INSIDE_PROJECTION.to_string());
    }
}

/// Everything mounted into the sandbox, in order, and the directory the harness's own
/// binaries want on PATH.
fn plan_mounts(req: &Request<'_>, harness_bin: &Path) -> (Vec<Mount>, Option<PathBuf>) {
    let mut mounts = system_mounts();
    mounts.push(Mount {
        source: req.memory_binary.to_path_buf(),
        target: PathBuf::from(INSIDE_MEMORY),
        read_only: true,
    });
    let (harness_m, extra_path) = harness_mounts(harness_bin);
    mounts.extend(harness_m);
    mounts.push(Mount {
        source: req.home.to_path_buf(),
        target: PathBuf::from(INSIDE_HOME),
        read_only: false,
    });
    mounts.push(Mount {
        source: req.workdir.to_path_buf(),
        target: req.workdir_inside().to_path_buf(),
        read_only: false,
    });
    // the projection is the launcher's own directory, mounted read-only: the harness
    // reads it there and nowhere else, and cannot edit it into lies
    if let Some(projection) = req.projection.filter(|p| p.is_dir()) {
        mounts.push(Mount {
            source: projection.to_path_buf(),
            target: PathBuf::from(INSIDE_PROJECTION),
            read_only: true,
        });
    }
    mounts.push(Mount {
        source: req.egress_socket.to_path_buf(),
        target: PathBuf::from(INSIDE_SOCKET),
        read_only: false,
    });
    (mounts, extra_path)
}

/// In the harness image only the two things that are the session's: its private home
/// and the working tree. The runner turns these into the container's bind mounts; the
/// egress socket arrives through a shared volume instead.
fn image_mounts(req: &Request<'_>) -> Vec<Mount> {
    vec![
        Mount {
            source: req.home.to_path_buf(),
            target: PathBuf::from(INSIDE_HOME),
            read_only: false,
        },
        Mount {
            source: req.workdir.to_path_buf(),
            target: req.workdir_inside().to_path_buf(),
            read_only: false,
        },
    ]
}

/// Build the plan: write the harness's configuration into the private home and decide
/// mounts, environment, and command.
///
/// # Errors
///
/// A harness not on PATH, an unsupported harness, auth and provider combination, or a
/// file under the home that cannot be written.
pub fn prepare(req: &Request<'_>) -> Result<Plan, Error> {
    let image = req.image || req.rootfs.is_some();
    let harness_bin = if req.harness == Harness::Command {
        program_of(req.harness_args, image)?
    } else if image {
        PathBuf::from("/usr/local/bin").join(req.harness.binary())
    } else {
        find_on_path(req.harness.binary())?
    };
    let harness_args: &[String] = if req.harness == Harness::Command {
        req.harness_args.get(1..).unwrap_or(&[])
    } else {
        req.harness_args
    };
    std::fs::create_dir_all(req.home).map_err(io(req.home))?;
    let relay = format!("http://{INSIDE_RELAY}");
    let own = [req.harness.provider(), req.harness.subscription_provider()];
    let provider = match req.provider {
        Some(p) if req.harness.accepts_provider() || own.contains(&p) => p.to_string(),
        Some(p) => {
            return Err(Error::Unsupported(format!(
                "{} does not take --provider ({p}); its model calls go to {}",
                req.harness.binary(),
                req.harness.provider()
            )))
        }
        None => req.harness.provider().to_string(),
    };

    let mut env = base_env(req.harness_env);
    // the memory server's address, for the hooks as for the harness
    env.insert("MEMORY_MCP_URL".into(), format!("{relay}/mcp"));
    projection_env(req, &mut env);
    let (mounts, extra_path) = match req.rootfs {
        Some(rootfs) => {
            let mut m = rootfs_mounts(rootfs);
            m.extend(image_mounts(req));
            m.push(Mount {
                source: req.egress_socket.to_path_buf(),
                target: PathBuf::from(INSIDE_SOCKET),
                read_only: false,
            });
            (m, None)
        }
        None if image => (image_mounts(req), None),
        None => plan_mounts(req, &harness_bin),
    };
    let links = root_links(req.rootfs.unwrap_or(Path::new("/")));
    let mut path = String::from("/opt/memory:/usr/local/bin:/usr/bin:/bin");
    if let Some(p) = extra_path {
        path = format!("{}:{path}", p.display());
    }
    env.insert("PATH".into(), path);
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
    match req.harness {
        Harness::Claude => setup_claude(req, &relay, &mut env, &mut argv)?,
        Harness::Codex => setup_codex(req, &relay, &provider, &mut env)?,
        Harness::Gemini => setup_gemini(req, &relay, &mut env)?,
        Harness::Antigravity => setup_antigravity(req, &relay, &mut env)?,
        Harness::Qwen => setup_qwen(req, &relay, &provider, &mut env)?,
        Harness::Copilot => setup_copilot(req, &relay, &mut env)?,
        Harness::OpenCode => setup_opencode(req, &relay, &provider, &mut env)?,
        Harness::Command => setup_command(req, &relay, &provider, &mut env),
    }
    argv.extend(harness_args.iter().cloned());

    Ok(Plan {
        mounts,
        env,
        argv,
        cwd: req.workdir_inside().to_path_buf(),
        hostname: "memory-sandbox".into(),
        handling_cap: req.destination_ceiling,
        links,
    })
}

fn setup_claude(
    req: &Request<'_>,
    relay: &str,
    env: &mut BTreeMap<String, String>,
    argv: &mut Vec<String>,
) -> Result<(), Error> {
    let mcp = serde_json::json!({
        "mcpServers": { "memory": { "type": "http", "url": format!("{relay}/mcp") } }
    });
    write(&req.home.join(".claude-mcp.json"), &mcp.to_string())?;
    claude_global_config(&req.home.join(".claude.json"), req.workdir_inside())?;
    claude_hooks(&req.home.join(".claude/settings.json"))?;
    env.insert("ANTHROPIC_BASE_URL".into(), format!("{relay}/anthropic"));
    env.insert("DISABLE_AUTOUPDATER".into(), "1".into());
    match req.auth {
        AuthMode::Subscription => {
            copy_login(req, ".claude/.credentials.json", "a Claude Code login")?;
        }
        AuthMode::Gateway | AuthMode::Key => {
            // the egress injects the real credential; this one only has to exist
            env.insert("ANTHROPIC_AUTH_TOKEN".into(), "egress".into());
        }
    }
    argv.push("--mcp-config".into());
    argv.push(format!("{INSIDE_HOME}/.claude-mcp.json"));
    argv.push("--strict-mcp-config".into());
    Ok(())
}

/// The hooks that leave a handoff the agent did not write, before compaction and at
/// session end, and the one that turns a read of the projection into a `get`, before a
/// tool runs. Where the projection is mounted is set with the environment, below. The
/// hook before compaction and at
/// session end, the `memory` binary inside reads the event and the transcript and
/// drops a note in the outbox for the launcher to file. Merged into the user settings
/// in the private home, which the launcher owns.
fn claude_hooks(path: &Path) -> Result<(), Error> {
    merge_hooks(
        path,
        "claude",
        &["PreCompact", "SessionEnd", "PreToolUse"],
        30,
    )
}

/// Merge the hooks for `events` into a JSON settings file that may hold other things:
/// Claude Code's `settings.json`, Codex's `hooks.json`, Qwen Code's `settings.json`.
/// All share the shape, `hooks → event → matcher groups → command handlers`; they differ
/// in the name of the compaction event and in how long a session-end hook may run.
fn merge_hooks(path: &Path, harness: &str, events: &[&str], timeout: u32) -> Result<(), Error> {
    let mut settings: serde_json::Value = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    if !settings.is_object() {
        settings = serde_json::json!({});
    }
    let hook = serde_json::json!([{
        "matcher": "*",
        "hooks": [{ "type": "command", "command": format!("{INSIDE_MEMORY} hook {harness}"), "timeout": timeout }]
    }]);
    for e in events {
        settings["hooks"][*e] = hook.clone();
    }
    write(
        path,
        &serde_json::to_string_pretty(&settings).unwrap_or_default(),
    )
}

fn setup_codex(
    req: &Request<'_>,
    relay: &str,
    provider: &str,
    env: &mut BTreeMap<String, String>,
) -> Result<(), Error> {
    let config = if req.auth == AuthMode::Subscription {
        // a ChatGPT login talks to the ChatGPT backend, which refuses relayed requests, so
        // Codex's HTTPS goes through the egress as a tunnel to the ChatGPT hosts, as
        // Copilot CLI's does; the built-in provider and its backend stay as they are
        copy_login(
            req,
            ".codex/auth.json",
            "`codex login` (or `codex login --device-auth`)",
        )?;
        proxy_via(relay, env);
        // the forward variant, kept for diagnosis: the backend behind the egress
        let base = if std::env::var_os("MEMORY_CHATGPT_FORWARD").is_some() {
            format!("chatgpt_base_url = \"{relay}/chatgpt/\"\n\n")
        } else {
            String::new()
        };
        format!(
            r#"# written by memory launch; edits are overwritten
{base}[features]
hooks = true

[mcp_servers.memory]
url = "{relay}/mcp"
"#
        )
    } else {
        env.insert("DM_EGRESS_TOKEN".into(), "egress".into());
        format!(
            r#"# written by memory launch; edits are overwritten
model_provider = "egress"

[model_providers.egress]
name = "egress ({provider})"
base_url = "{relay}/{provider}/v1"
env_key = "DM_EGRESS_TOKEN"
wire_api = "responses"

[features]
hooks = true

[mcp_servers.memory]
url = "{relay}/mcp"
"#
        )
    };
    write(&req.home.join(".codex/config.toml"), &config)?;
    // Codex routes its tools, MCP included, through a helper it expects beside itself;
    // the npm install ships it, a bare release tarball does not
    if !req.image && req.rootfs.is_none() {
        if let Ok(bin) = find_on_path("codex") {
            if !bin.with_file_name("codex-code-mode-host").is_file() {
                eprintln!(
                    "memory: warning: {} has no codex-code-mode-host beside it; Codex's tools, memory among them, will fail. Install it from the same Codex release",
                    bin.display()
                );
            }
        }
    }
    // Codex gives a session-end hook three seconds at most; the projection's guard
    // before a tool runs has the time a tool hook may take
    let hooks = req.home.join(".codex/hooks.json");
    merge_hooks(&hooks, "codex", &["PreCompact", "SessionEnd"], 3)?;
    merge_hooks(&hooks, "codex", &["PreToolUse"], 30)?;
    env.insert("CODEX_HOME".into(), format!("{INSIDE_HOME}/.codex"));
    Ok(())
}

fn setup_gemini(
    req: &Request<'_>,
    relay: &str,
    env: &mut BTreeMap<String, String>,
) -> Result<(), Error> {
    let subscription = req.auth == AuthMode::Subscription;
    if subscription {
        // a Google login talks to Code Assist, not the Gemini API: its endpoint moves to
        // the egress, and the token refresh at oauth2.googleapis.com goes through the
        // egress as a tunnel
        copy_login(req, ".gemini/oauth_creds.json", "a Gemini CLI Google login")?;
        // which account and tier; optional, Gemini asks Google again without it
        let _ = copy_login(req, ".gemini/google_accounts.json", "");
        proxy_via(relay, env);
        env.insert("CODE_ASSIST_ENDPOINT".into(), format!("{relay}/google"));
        if let Some(project) = env_passthrough("GOOGLE_CLOUD_PROJECT") {
            env.insert("GOOGLE_CLOUD_PROJECT".into(), project);
        }
    } else {
        env.insert("GOOGLE_GEMINI_BASE_URL".into(), format!("{relay}/gemini"));
        // the egress injects the real key as x-goog-api-key and drops any ?key= it sees
        env.insert("GEMINI_API_KEY".into(), "egress".into());
    }
    let auth_type = if subscription {
        "oauth-personal"
    } else {
        "gemini-api-key"
    };
    // Gemini's hook timeout is in milliseconds, unlike Claude Code's and Codex's seconds
    let hook = serde_json::json!([{
        "matcher": "*",
        "hooks": [{ "type": "command", "command": format!("{INSIDE_MEMORY} hook gemini"), "timeout": 10_000 }]
    }]);
    let settings = serde_json::json!({
        // BeforeTool is Gemini's name for the event before a tool runs
        "hooks": { "PreCompress": hook, "SessionEnd": hook, "BeforeTool": hook },
        "security": { "auth": { "selectedType": auth_type } },
        "general": { "enableAutoUpdate": false },
        "privacy": { "usageStatisticsEnabled": false },
        "mcpServers": { "memory": { "httpUrl": format!("{relay}/mcp") } }
    });
    write(
        &req.home.join(".gemini/settings.json"),
        &serde_json::to_string_pretty(&settings).unwrap_or_default(),
    )?;
    // the working tree is the one thing mounted read-write; it is trusted by construction
    env.insert("GEMINI_CLI_TRUST_WORKSPACE".into(), "true".into());
    Ok(())
}

/// Antigravity CLI keeps Gemini CLI's variables for a key, `GEMINI_API_KEY` behind
/// `GOOGLE_GEMINI_BASE_URL`, once its settings name the Gemini API as the model
/// provider; a Google login is a token file the host's login left, with its HTTPS through
/// the egress as a tunnel to Google's hosts, since the login's endpoint has no override
/// of its own that is known to work. MCP servers and hooks live in files of their own
/// under `.gemini/config`, not in the settings, and a remote server is `serverUrl`.
fn setup_antigravity(
    req: &Request<'_>,
    relay: &str,
    env: &mut BTreeMap<String, String>,
) -> Result<(), Error> {
    let subscription = req.auth == AuthMode::Subscription;
    let mut settings = serde_json::json!({});
    if subscription {
        copy_login(
            req,
            ".gemini/antigravity-cli/antigravity-oauth-token",
            "an Antigravity CLI Google login (run `agy` once on the host)",
        )?;
        let _ = copy_login(req, ".gemini/google_accounts.json", "");
        // the login's model host is one the egress tunnels to; no endpoint override
        // for it is known, so the route is pinned egress without metering
        proxy_via(relay, env);
        if let Some(project) = env_passthrough("GOOGLE_CLOUD_PROJECT") {
            env.insert("GOOGLE_CLOUD_PROJECT".into(), project);
        }
    } else {
        // the key is read only when the settings say so; the egress holds the real one
        settings["modelProvider"] = "gemini".into();
        env.insert("GOOGLE_GEMINI_BASE_URL".into(), format!("{relay}/gemini"));
        env.insert("GEMINI_API_KEY".into(), "egress".into());
    }
    write(
        &req.home.join(".gemini/antigravity-cli/settings.json"),
        &serde_json::to_string_pretty(&settings).unwrap_or_default(),
    )?;
    let mcp = serde_json::json!({
        "mcpServers": { "memory": { "serverUrl": format!("{relay}/mcp") } }
    });
    write(
        &req.home.join(".gemini/config/mcp_config.json"),
        &serde_json::to_string_pretty(&mcp).unwrap_or_default(),
    )?;
    // agy's hooks: named groups of events; the tool events take matcher groups like
    // Claude Code's, the loop events a flat list of handlers. `Stop` is the end of a
    // turn, which the hook folds into one note, and `PreToolUse` guards the projection
    // in interactive runs; the headless flag that skips permissions skips this layer
    // too, so a `work` run has no observation here. There is no pre-compaction event.
    let command = serde_json::json!({
        "type": "command",
        "command": format!("{INSIDE_MEMORY} hook antigravity"),
        "timeout": 30
    });
    let hooks = serde_json::json!({
        "memory": {
            "PreToolUse": [{ "matcher": "*", "hooks": [command] }],
            "Stop": [command]
        }
    });
    write(
        &req.home.join(".gemini/config/hooks.json"),
        &serde_json::to_string_pretty(&hooks).unwrap_or_default(),
    )?;
    env.insert("AGY_CLI_DISABLE_AUTO_UPDATE".into(), "true".into());
    Ok(())
}

/// Qwen Code is Gemini CLI's fork for Alibaba's models and speaks OpenAI's API: a key is
/// `OPENAI_API_KEY` behind `OPENAI_BASE_URL`, read once its settings select the OpenAI
/// auth type, and DashScope's compatible endpoint is the default destination, any
/// other OpenAI-compatible one on request. A Qwen login is the credential file the
/// host's login left under `~/.qwen`, with the harness's HTTPS through the egress as a
/// tunnel to Qwen's hosts, since the login's endpoint comes from the credential itself.
/// MCP servers and hooks live in the settings, the hooks in Claude Code's shape with
/// timeouts in seconds.
fn setup_qwen(
    req: &Request<'_>,
    relay: &str,
    provider: &str,
    env: &mut BTreeMap<String, String>,
) -> Result<(), Error> {
    let subscription = req.auth == AuthMode::Subscription;
    if subscription {
        copy_login(
            req,
            ".qwen/oauth_creds.json",
            "a Qwen Code login (run `qwen` once on the host and pick Qwen OAuth)",
        )?;
        proxy_via(relay, env);
    } else {
        env.insert("OPENAI_BASE_URL".into(), format!("{relay}/{provider}/v1"));
        // the egress injects the real key; this one only has to exist
        env.insert("OPENAI_API_KEY".into(), "egress".into());
    }
    let auth_type = if subscription { "qwen-oauth" } else { "openai" };
    let settings = serde_json::json!({
        "security": { "auth": { "selectedType": auth_type } },
        "general": { "enableAutoUpdate": false },
        "privacy": { "usageStatisticsEnabled": false },
        "mcpServers": { "memory": { "httpUrl": format!("{relay}/mcp") } }
    });
    let path = req.home.join(".qwen/settings.json");
    write(
        &path,
        &serde_json::to_string_pretty(&settings).unwrap_or_default(),
    )?;
    merge_hooks(
        &path,
        "qwen",
        &["PreCompact", "SessionEnd", "PreToolUse"],
        30,
    )
}

fn setup_copilot(
    req: &Request<'_>,
    relay: &str,
    env: &mut BTreeMap<String, String>,
) -> Result<(), Error> {
    if req.auth != AuthMode::Subscription {
        return Err(Error::Unsupported(
            "copilot has no API-key mode; it uses your GitHub login. Use --auth subscription with GH_TOKEN set".into(),
        ));
    }
    if !env.contains_key("GH_TOKEN") {
        return Err(Error::Unsupported(
            "copilot needs GH_TOKEN in the launcher's environment (try: GH_TOKEN=$(gh auth token))"
                .into(),
        ));
    }
    let mcp = serde_json::json!({
        "mcpServers": { "memory": { "type": "http", "url": format!("{relay}/mcp") } }
    });
    write(&req.home.join(".copilot/mcp-config.json"), &mcp.to_string())?;
    env.insert("COPILOT_HOME".into(), format!("{INSIDE_HOME}/.copilot"));
    env.insert("COPILOT_AUTO_UPDATE".into(), "false".into());
    // no base-url override exists, so its HTTPS goes through the egress as a CONNECT
    // tunnel to the hosts the github destination allows
    proxy_via(relay, env);
    Ok(())
}

/// The program a `command` harness runs: the first harness argument, as given when it is
/// a path, found on PATH otherwise. In the image it is taken as given.
fn program_of(args: &[String], image: bool) -> Result<PathBuf, Error> {
    let Some(first) = args.first() else {
        return Err(Error::Unsupported(
            "--harness command needs the program to run after `--`".into(),
        ));
    };
    let p = Path::new(first);
    if p.is_absolute() || image {
        return Ok(p.to_path_buf());
    }
    find_on_path(first)
}

/// A program gets the egress and the memory server as environment and nothing else:
/// base URLs for the three API shapes at the destination it was given, a placeholder
/// credential the egress replaces, and the memory server's address.
fn setup_command(
    req: &Request<'_>,
    relay: &str,
    provider: &str,
    env: &mut BTreeMap<String, String>,
) {
    env.insert("MEMORY_MCP_URL".into(), format!("{relay}/mcp"));
    env.insert("MEMORY_PROVIDER".into(), provider.to_string());
    env.insert("OPENAI_BASE_URL".into(), format!("{relay}/{provider}/v1"));
    env.insert("ANTHROPIC_BASE_URL".into(), format!("{relay}/{provider}"));
    env.insert(
        "GOOGLE_GEMINI_BASE_URL".into(),
        format!("{relay}/{provider}"),
    );
    if req.auth != AuthMode::Subscription {
        for k in ["OPENAI_API_KEY", "ANTHROPIC_API_KEY", "GEMINI_API_KEY"] {
            env.insert(k.into(), "egress".into());
        }
    }
}

/// OpenCode on a GitHub Copilot subscription, which GitHub supports: the login OpenCode
/// made on the host is copied in, and its HTTPS goes through the egress as a tunnel to
/// GitHub's hosts, as Copilot CLI's does. Pinned egress without metering.
fn setup_opencode_copilot(
    req: &Request<'_>,
    relay: &str,
    env: &mut BTreeMap<String, String>,
) -> Result<(), Error> {
    copy_login(
        req,
        ".local/share/opencode/auth.json",
        "an OpenCode GitHub Copilot login (`opencode auth login`, GitHub Copilot)",
    )?;
    proxy_via(relay, env);
    let config = serde_json::json!({
        "$schema": "https://opencode.ai/config.json",
        "autoupdate": false,
        "mcp": {
            "memory": { "type": "remote", "url": format!("{relay}/mcp"), "enabled": true }
        }
    });
    write(
        &req.home.join(".config/opencode/opencode.json"),
        &serde_json::to_string_pretty(&config).unwrap_or_default(),
    )?;
    env.insert("XDG_CONFIG_HOME".into(), format!("{INSIDE_HOME}/.config"));
    env.insert(
        "XDG_DATA_HOME".into(),
        format!("{INSIDE_HOME}/.local/share"),
    );
    Ok(())
}

fn setup_opencode(
    req: &Request<'_>,
    relay: &str,
    provider: &str,
    env: &mut BTreeMap<String, String>,
) -> Result<(), Error> {
    if req.auth == AuthMode::Subscription && provider == "github-copilot" {
        return setup_opencode_copilot(req, relay, env);
    }
    if req.auth == AuthMode::Subscription {
        return Err(Error::Unsupported(
            "opencode's Claude Pro/Max login is something Anthropic prohibits for third-party clients; use --auth key, or --provider github-copilot with your GitHub Copilot login".into(),
        ));
    }
    let (npm, key_env) = match provider {
        "anthropic" => ("@ai-sdk/anthropic", "ANTHROPIC_API_KEY"),
        "openai" => ("@ai-sdk/openai", "OPENAI_API_KEY"),
        "xai" => ("@ai-sdk/xai", "XAI_API_KEY"),
        _ => ("@ai-sdk/openai-compatible", "DM_EGRESS_TOKEN"),
    };
    let config = serde_json::json!({
        "$schema": "https://opencode.ai/config.json",
        "autoupdate": false,
        "provider": {
            provider: {
                "npm": npm,
                "options": {
                    "baseURL": format!("{relay}/{provider}/v1"),
                    "apiKey": "egress"
                }
            }
        },
        "mcp": {
            "memory": { "type": "remote", "url": format!("{relay}/mcp"), "enabled": true }
        }
    });
    write(
        &req.home.join(".config/opencode/opencode.json"),
        &serde_json::to_string_pretty(&config).unwrap_or_default(),
    )?;
    env.insert("XDG_CONFIG_HOME".into(), format!("{INSIDE_HOME}/.config"));
    env.insert(
        "XDG_DATA_HOME".into(),
        format!("{INSIDE_HOME}/.local/share"),
    );
    env.insert("XDG_CACHE_HOME".into(), format!("{INSIDE_HOME}/.cache"));
    env.insert(key_env.into(), "egress".into());
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

/// Run a plan with the harness's standard output captured, for a non-interactive run
/// whose answer the launcher files. Standard error still reaches the terminal.
///
/// # Errors
///
/// [`Error::NoBackend`] off Linux, or a confinement failure.
pub fn run_captured(plan: &Plan) -> Result<(i32, Vec<u8>), Error> {
    #[cfg(target_os = "linux")]
    {
        linux::run_captured(plan)
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

    /// Tests that change the process's PATH take this, since cargo runs tests in
    /// parallel and PATH is one variable for the whole process.
    static PATH_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn path_lock() -> std::sync::MutexGuard<'static, ()> {
        PATH_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        bins: PathBuf,
        work: PathBuf,
        sock: PathBuf,
        memory_bin: PathBuf,
        host_home: PathBuf,
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
        for b in [
            "claude", "codex", "gemini", "agy", "qwen", "copilot", "opencode",
        ] {
            fake_bin(&bins, b);
        }
        let memory_bin = fake_bin(&bins, "memory");
        let work = dir.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        let sock = dir.path().join("egress.sock");
        std::fs::write(&sock, "").unwrap();
        // a host home holding one login per harness, and a file that must not leak
        let host_home = dir.path().join("host-home");
        for (rel, text) in [
            (
                ".claude/.credentials.json",
                r#"{"claudeAiOauth":{"accessToken":"x"}}"#,
            ),
            (".codex/auth.json", r#"{"tokens":{"access_token":"x"}}"#),
            (
                ".gemini/oauth_creds.json",
                r#"{"access_token":"x","refresh_token":"y"}"#,
            ),
            (".ssh/id_ed25519", "private"),
        ] {
            let p = host_home.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, text).unwrap();
        }
        let root = dir.path().to_path_buf();
        Fixture {
            _dir: dir,
            bins,
            work,
            sock,
            memory_bin,
            host_home,
            root,
        }
    }

    impl Fixture {
        #[allow(clippy::too_many_arguments)] // a test fixture mirroring Request's fields
        fn plan_with(
            &self,
            harness: Harness,
            auth: AuthMode,
            provider: Option<&str>,
            home: &str,
            args: &[String],
            creds: bool,
            harness_env: &[(String, String)],
        ) -> Result<(PathBuf, Plan), Error> {
            self.plan_full(
                harness,
                auth,
                provider,
                home,
                args,
                creds,
                harness_env,
                None,
            )
        }

        #[allow(clippy::too_many_arguments)] // the test's twin of Request
        fn plan_full(
            &self,
            harness: Harness,
            auth: AuthMode,
            provider: Option<&str>,
            home: &str,
            args: &[String],
            creds: bool,
            harness_env: &[(String, String)],
            projection: Option<&Path>,
        ) -> Result<(PathBuf, Plan), Error> {
            let home = self.root.join(home);
            let _path = path_lock();
            let old = std::env::var_os("PATH").unwrap_or_default();
            let mut paths = vec![self.bins.clone()];
            paths.extend(std::env::split_paths(&old));
            std::env::set_var("PATH", std::env::join_paths(paths).unwrap());
            let r = prepare(&Request {
                harness,
                auth,
                provider,
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
                host_home: creds.then_some(self.host_home.as_path()),
                harness_env,
                image: false,
                workdir_inside: None,
                rootfs: None,
                projection,
            });
            std::env::set_var("PATH", old);
            r.map(|p| (home, p))
        }

        fn plan(
            &self,
            harness: Harness,
            auth: AuthMode,
            home: &str,
            args: &[String],
            creds: bool,
        ) -> Result<(PathBuf, Plan), Error> {
            self.plan_with(harness, auth, None, home, args, creds, &[])
        }
    }

    fn json_file(p: &Path) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
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
        let mcp = json_file(&home.join(".claude-mcp.json"));
        assert_eq!(
            mcp["mcpServers"]["memory"]["url"],
            "http://127.0.0.1:4100/mcp"
        );
        let g = json_file(&home.join(".claude.json"));
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
    #[cfg(unix)] // rootfs mode is the Linux backend's; the symlink here is unix-only
    fn an_unpacked_image_replaces_the_host_system() {
        let f = fixture();
        let rootfs = f.root.join("rootfs");
        for d in ["usr/local/bin", "etc", "opt/memory", "var"] {
            std::fs::create_dir_all(rootfs.join(d)).unwrap();
        }
        std::os::unix::fs::symlink("usr/bin", rootfs.join("bin")).unwrap();
        let home = f.root.join("img-home");
        let plan = prepare(&Request {
            harness: Harness::Claude,
            auth: AuthMode::Key,
            provider: None,
            workdir: &f.work,
            home: &home,
            egress_socket: &f.sock,
            memory_binary: &f.memory_bin,
            destination_ceiling: Label::Public,
            harness_args: &[],
            host_home: None,
            harness_env: &[],
            image: false,
            workdir_inside: None,
            rootfs: Some(&rootfs),
            projection: None,
        })
        .unwrap();
        let from_rootfs = |t: &str| {
            plan.mounts
                .iter()
                .find(|m| m.target == Path::new(t))
                .map(|m| m.source.starts_with(&rootfs) && m.read_only)
        };
        assert_eq!(from_rootfs("/usr"), Some(true));
        assert_eq!(from_rootfs("/etc"), Some(true));
        assert_eq!(from_rootfs("/opt"), Some(true), "the image's memory binary");
        assert_eq!(from_rootfs("/var"), Some(true));
        assert!(
            !plan.mounts.iter().any(|m| m.source == Path::new("/usr")),
            "nothing of the host's system"
        );
        assert!(
            !plan
                .mounts
                .iter()
                .any(|m| m.target == Path::new(INSIDE_MEMORY)),
            "the binary comes with the image"
        );
        assert!(plan
            .mounts
            .iter()
            .any(|m| m.target == Path::new(INSIDE_SOCKET)));
        assert!(plan.argv.contains(&"/usr/local/bin/claude".to_string()));
        assert_eq!(
            plan.links,
            vec![(PathBuf::from("bin"), PathBuf::from("usr/bin"))],
            "merged-usr links come from the rootfs, not the host"
        );
    }

    #[test]
    fn the_projection_is_mounted_read_only_and_named_when_rendered() {
        let f = fixture();
        let (_home, plan) = f
            .plan(Harness::Claude, AuthMode::Subscription, "h", &[], true)
            .unwrap();
        assert!(
            !plan.env.contains_key("MEMORY_PROJECTION")
                && !plan
                    .mounts
                    .iter()
                    .any(|m| m.target == Path::new(INSIDE_PROJECTION)),
            "nothing rendered, nothing mounted"
        );
        let rendered = f.root.join(PROJECTION).join("claude");
        std::fs::create_dir_all(&rendered).unwrap();
        let (home, plan) = f
            .plan_full(
                Harness::Claude,
                AuthMode::Subscription,
                None,
                "h2",
                &[],
                true,
                &[],
                Some(&rendered),
            )
            .unwrap();
        let m = plan
            .mounts
            .iter()
            .find(|m| m.target == Path::new(INSIDE_PROJECTION))
            .expect("the projection is mounted");
        assert!(
            m.read_only,
            "read-only: the files cannot be edited into lies"
        );
        assert_eq!(m.source, rendered);
        assert!(
            !m.source.starts_with(&home),
            "the launcher's own directory, not the home the harness writes"
        );
        assert_eq!(plan.env["MEMORY_PROJECTION"], INSIDE_PROJECTION);
        assert_eq!(
            plan.env["MEMORY_MCP_URL"], "http://127.0.0.1:4100/mcp",
            "the hook needs the server's address to turn an open into a get"
        );
        let settings = json_file(&home.join(".claude/settings.json"));
        assert_eq!(
            settings["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            "/opt/memory/memory hook claude"
        );
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
        assert!(!Path::new(INSIDE_MEMORY).starts_with("/usr"));
        // the plan canonicalizes the harness's directory; on Windows that adds a prefix
        let bins = std::fs::canonicalize(&f.bins).unwrap();
        assert!(
            plan.env["PATH"].starts_with(&format!("{}:", bins.display())),
            "{}",
            plan.env["PATH"]
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
    fn codex_config_and_provider() {
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
        let hooks = json_file(&home.join(".codex/hooks.json"));
        assert_eq!(
            hooks["hooks"]["SessionEnd"][0]["hooks"][0]["command"],
            "/opt/memory/memory hook codex"
        );
        assert_eq!(hooks["hooks"]["SessionEnd"][0]["hooks"][0]["timeout"], 3);
        assert!(hooks["hooks"]["PreCompact"].is_array());
        // the projection's guard, with the time a tool hook may take rather than the
        // three seconds Codex gives a session's end
        assert_eq!(
            hooks["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            "/opt/memory/memory hook codex"
        );
        assert_eq!(hooks["hooks"]["PreToolUse"][0]["hooks"][0]["timeout"], 30);
        assert_eq!(plan.env["MEMORY_OUTBOX"], "/home/user/.memory-outbox");
        assert_eq!(plan.env["DM_EGRESS_TOKEN"], "egress");
        let (home, _) = f
            .plan_with(
                Harness::Codex,
                AuthMode::Key,
                Some("xai"),
                "c2",
                &[],
                false,
                &[],
            )
            .unwrap();
        let cfg = std::fs::read_to_string(home.join(".codex/config.toml")).unwrap();
        assert!(
            cfg.contains("base_url = \"http://127.0.0.1:4100/xai/v1\""),
            "grok through the xai destination: {cfg}"
        );
        assert!(
            matches!(
                f.plan_with(
                    Harness::Claude,
                    AuthMode::Key,
                    Some("xai"),
                    "c4",
                    &[],
                    false,
                    &[]
                ),
                Err(Error::Unsupported(_))
            ),
            "claude does not take a provider"
        );
        assert!(
            f.plan_with(
                Harness::Claude,
                AuthMode::Key,
                Some("anthropic"),
                "c5",
                &[],
                false,
                &[]
            )
            .is_ok(),
            "naming the harness's own provider is allowed"
        );
    }

    #[test]
    fn codex_subscription_moves_the_chatgpt_backend_to_the_egress() {
        let f = fixture();
        // without a host login the subscription mode is refused; with one, the ChatGPT
        // backend moves to the egress and the login file is copied in
        assert!(matches!(
            f.plan(Harness::Codex, AuthMode::Subscription, "c3", &[], false),
            Err(Error::Unsupported(_))
        ));
        let (home, plan) = f
            .plan(Harness::Codex, AuthMode::Subscription, "c5", &[], true)
            .unwrap();
        let cfg = std::fs::read_to_string(home.join(".codex/config.toml")).unwrap();
        assert!(
            !cfg.contains("chatgpt_base_url"),
            "the backend is reached through the tunnel: {cfg}"
        );
        assert!(
            !cfg.contains("model_provider"),
            "the built-in provider stays: {cfg}"
        );
        assert!(home.join(".codex/auth.json").is_file(), "login copied in");
        assert!(
            !home.join(".ssh").exists(),
            "nothing else from the host home"
        );
        assert_eq!(plan.env["HTTPS_PROXY"], "http://127.0.0.1:4100");
        assert!(!plan.env.contains_key("DM_EGRESS_TOKEN"));
    }

    #[test]
    fn gemini_settings_and_env() {
        let f = fixture();
        let (home, plan) = f
            .plan(Harness::Gemini, AuthMode::Key, "g1", &[], false)
            .unwrap();
        let s = json_file(&home.join(".gemini/settings.json"));
        assert_eq!(s["security"]["auth"]["selectedType"], "gemini-api-key");
        assert_eq!(
            s["mcpServers"]["memory"]["httpUrl"],
            "http://127.0.0.1:4100/mcp"
        );
        assert_eq!(s["general"]["enableAutoUpdate"], false);
        assert_eq!(
            s["hooks"]["PreCompress"][0]["hooks"][0]["command"],
            "/opt/memory/memory hook gemini"
        );
        assert!(s["hooks"]["SessionEnd"].is_array());
        assert_eq!(
            s["hooks"]["SessionEnd"][0]["hooks"][0]["timeout"], 10_000,
            "milliseconds, for Gemini"
        );
        // Gemini CLI's name for the event before a tool runs
        assert_eq!(
            s["hooks"]["BeforeTool"][0]["hooks"][0]["command"],
            "/opt/memory/memory hook gemini"
        );
        assert_eq!(
            plan.env["GOOGLE_GEMINI_BASE_URL"],
            "http://127.0.0.1:4100/gemini"
        );
        assert_eq!(plan.env["GEMINI_API_KEY"], "egress");
        assert_eq!(plan.env["GEMINI_CLI_TRUST_WORKSPACE"], "true");
        assert!(matches!(
            f.plan(Harness::Gemini, AuthMode::Subscription, "g2", &[], false),
            Err(Error::Unsupported(_))
        ));
    }

    #[test]
    fn gemini_subscription_moves_code_assist_to_the_egress() {
        let f = fixture();
        // a Google login: Code Assist moves to the egress, refresh goes through the tunnel
        let (home, plan) = f
            .plan(Harness::Gemini, AuthMode::Subscription, "g3", &[], true)
            .unwrap();
        let s = json_file(&home.join(".gemini/settings.json"));
        assert_eq!(s["security"]["auth"]["selectedType"], "oauth-personal");
        assert!(
            home.join(".gemini/oauth_creds.json").is_file(),
            "login copied in"
        );
        assert_eq!(
            plan.env["CODE_ASSIST_ENDPOINT"],
            "http://127.0.0.1:4100/google"
        );
        assert_eq!(plan.env["HTTPS_PROXY"], "http://127.0.0.1:4100");
        assert!(!plan.env.contains_key("GEMINI_API_KEY"));
        assert!(!plan.env.contains_key("GOOGLE_GEMINI_BASE_URL"));
    }

    #[test]
    #[cfg(unix)] // the program under test is the shell, which Windows runners lack
    fn a_command_harness_runs_a_program_with_the_egress_in_its_environment() {
        let f = fixture();
        let args = vec!["sh".to_string(), "-c".to_string(), "echo hi".to_string()];
        let (_home, plan) = f
            .plan_with(
                Harness::Command,
                AuthMode::Key,
                Some("openai"),
                "cmd1",
                &args,
                false,
                &[],
            )
            .unwrap();
        // `sh` resolves to whatever the host's shell is, dash on Debian and Ubuntu
        let sh_path = find_on_path("sh").unwrap().display().to_string();
        let sh = plan.argv.iter().position(|a| *a == sh_path).unwrap();
        assert_eq!(&plan.argv[sh + 1..], &["-c", "echo hi"]);
        assert_eq!(plan.env["MEMORY_MCP_URL"], "http://127.0.0.1:4100/mcp");
        assert_eq!(
            plan.env["OPENAI_BASE_URL"],
            "http://127.0.0.1:4100/openai/v1"
        );
        assert_eq!(plan.env["OPENAI_API_KEY"], "egress");
        assert!(matches!(
            f.plan(Harness::Command, AuthMode::Key, "cmd2", &[], false),
            Err(Error::Unsupported(_))
        ));
        let gw = f
            .plan_with(
                Harness::Command,
                AuthMode::Gateway,
                None,
                "cmd3",
                &["/usr/bin/python3".to_string(), "agent.py".to_string()],
                false,
                &[],
            )
            .unwrap()
            .1;
        assert!(gw.argv.contains(&"/usr/bin/python3".to_string()));
        assert_eq!(gw.env["MEMORY_PROVIDER"], "openai");
    }

    #[test]
    fn opencode_on_a_copilot_login_tunnels_like_copilot_cli() {
        let f = fixture();
        std::fs::create_dir_all(f.host_home.join(".local/share/opencode")).unwrap();
        std::fs::write(
            f.host_home.join(".local/share/opencode/auth.json"),
            r#"{"github-copilot":{"type":"oauth"}}"#,
        )
        .unwrap();
        let (home, plan) = f
            .plan_with(
                Harness::OpenCode,
                AuthMode::Subscription,
                Some("github-copilot"),
                "oc1",
                &[],
                true,
                &[],
            )
            .unwrap();
        assert!(home.join(".local/share/opencode/auth.json").is_file());
        assert_eq!(plan.env["HTTPS_PROXY"], "http://127.0.0.1:4100");
        let c = json_file(&home.join(".config/opencode/opencode.json"));
        assert!(
            c.get("provider").is_none(),
            "no base-URL override: the tunnel carries it"
        );
        assert_eq!(c["mcp"]["memory"]["type"], "remote");
        assert!(matches!(
            f.plan(Harness::OpenCode, AuthMode::Subscription, "oc2", &[], true),
            Err(Error::Unsupported(_))
        ));
    }

    #[test]
    fn copilot_tunnels_through_the_egress_with_its_github_login() {
        let f = fixture();
        let token = vec![("GH_TOKEN".to_string(), "gho_test".to_string())];
        let (home, plan) = f
            .plan_with(
                Harness::Copilot,
                AuthMode::Subscription,
                None,
                "p1",
                &[],
                false,
                &token,
            )
            .unwrap();
        let mcp = json_file(&home.join(".copilot/mcp-config.json"));
        assert_eq!(mcp["mcpServers"]["memory"]["type"], "http");
        assert_eq!(plan.env["COPILOT_HOME"], "/home/user/.copilot");
        assert_eq!(plan.env["HTTPS_PROXY"], "http://127.0.0.1:4100");
        assert_eq!(plan.env["NO_PROXY"], "127.0.0.1,localhost");
        assert_eq!(
            plan.env["GH_TOKEN"], "gho_test",
            "the harness holds its own login"
        );
        assert!(
            matches!(
                f.plan_with(
                    Harness::Copilot,
                    AuthMode::Subscription,
                    None,
                    "p2",
                    &[],
                    false,
                    &[]
                ),
                Err(Error::Unsupported(_))
            ),
            "no token, no launch"
        );
        assert!(matches!(
            f.plan(Harness::Copilot, AuthMode::Key, "p3", &[], false),
            Err(Error::Unsupported(_))
        ));
    }

    #[test]
    fn opencode_config_follows_the_provider() {
        let f = fixture();
        let (home, plan) = f
            .plan(Harness::OpenCode, AuthMode::Key, "o1", &[], false)
            .unwrap();
        let c = json_file(&home.join(".config/opencode/opencode.json"));
        assert_eq!(
            c["provider"]["anthropic"]["options"]["baseURL"],
            "http://127.0.0.1:4100/anthropic/v1"
        );
        assert_eq!(c["provider"]["anthropic"]["options"]["apiKey"], "egress");
        assert_eq!(c["mcp"]["memory"]["type"], "remote");
        assert_eq!(c["mcp"]["memory"]["url"], "http://127.0.0.1:4100/mcp");
        assert_eq!(c["autoupdate"], false);
        assert_eq!(plan.env["XDG_CONFIG_HOME"], "/home/user/.config");
        assert_eq!(plan.env["ANTHROPIC_API_KEY"], "egress");
        let (home, plan) = f
            .plan_with(
                Harness::OpenCode,
                AuthMode::Key,
                Some("xai"),
                "o2",
                &[],
                false,
                &[],
            )
            .unwrap();
        let c = json_file(&home.join(".config/opencode/opencode.json"));
        assert_eq!(
            c["provider"]["xai"]["options"]["baseURL"],
            "http://127.0.0.1:4100/xai/v1"
        );
        assert_eq!(plan.env["XAI_API_KEY"], "egress");
        assert!(matches!(
            f.plan(Harness::OpenCode, AuthMode::Subscription, "o3", &[], false),
            Err(Error::Unsupported(_))
        ));
    }

    #[test]
    fn node_installed_harnesses_mount_their_prefix() {
        let (m, path) = harness_mounts(Path::new(
            "/home/u/.local/node/lib/node_modules/@google/gemini-cli/bundle/gemini.js",
        ));
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].source, Path::new("/home/u/.local/node"));
        assert!(m[0].read_only);
        assert_eq!(path.as_deref(), Some(Path::new("/home/u/.local/node/bin")));
        let (m, path) = harness_mounts(Path::new("/usr/bin/claude"));
        assert!(m.is_empty());
        assert!(path.is_none());
        let (m, _) = harness_mounts(Path::new("/home/u/.local/bin/codex"));
        assert_eq!(m[0].source, Path::new("/home/u/.local/bin"));
    }

    #[test]
    fn a_missing_harness_is_a_clear_error() {
        let dir = tempfile::tempdir().unwrap();
        let empty = dir.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let _path = path_lock();
        let old = std::env::var_os("PATH").unwrap_or_default();
        std::env::set_var("PATH", &empty);
        let r = prepare(&Request {
            projection: None,
            harness: Harness::Codex,
            auth: AuthMode::Key,
            provider: None,
            workdir: dir.path(),
            home: &dir.path().join("h"),
            egress_socket: dir.path(),
            memory_binary: dir.path(),
            destination_ceiling: Label::Public,
            harness_args: &[],
            host_home: None,
            image: false,
            workdir_inside: None,
            rootfs: None,
            harness_env: &[],
        });
        std::env::set_var("PATH", old);
        assert!(matches!(r, Err(Error::NoHarness(_))));
    }

    /// agy's hooks file: a named group, tool events in matcher groups, loop events flat.
    fn assert_agy_hooks(home: &Path) {
        let hooks = json_file(&home.join(".gemini/config/hooks.json"));
        let group = &hooks["memory"];
        assert_eq!(
            group["Stop"][0]["command"], "/opt/memory/memory hook antigravity",
            "a loop event is a flat list of handlers"
        );
        assert_eq!(group["Stop"][0]["type"], "command");
        assert_eq!(group["PreToolUse"][0]["matcher"], "*");
        assert_eq!(
            group["PreToolUse"][0]["hooks"][0]["command"], "/opt/memory/memory hook antigravity",
            "a tool event is a matcher group"
        );
        assert_eq!(group["PreToolUse"][0]["hooks"][0]["timeout"], 30);
        assert!(
            hooks.get("Stop").is_none(),
            "events sit under a named group"
        );
        assert!(
            group.get("SessionEnd").is_none(),
            "agy's session end is Stop"
        );
    }

    #[test]
    fn antigravity_plan_key_and_login() {
        let f = fixture();
        let (home, plan) = f
            .plan(Harness::Antigravity, AuthMode::Key, "a1", &[], false)
            .unwrap();
        assert_eq!(
            plan.env["GOOGLE_GEMINI_BASE_URL"],
            "http://127.0.0.1:4100/gemini"
        );
        assert_eq!(plan.env["GEMINI_API_KEY"], "egress");
        assert_eq!(plan.env["AGY_CLI_DISABLE_AUTO_UPDATE"], "true");
        assert!(
            !plan.env.contains_key("HTTPS_PROXY"),
            "a key needs no tunnel"
        );
        let settings = json_file(&home.join(".gemini/antigravity-cli/settings.json"));
        assert_eq!(
            settings["modelProvider"], "gemini",
            "the key is read only when the settings name the Gemini API"
        );
        let mcp = json_file(&home.join(".gemini/config/mcp_config.json"));
        assert_eq!(
            mcp["mcpServers"]["memory"]["serverUrl"],
            "http://127.0.0.1:4100/mcp"
        );
        assert!(
            mcp["mcpServers"]["memory"].get("url").is_none(),
            "serverUrl, not url"
        );
        assert_agy_hooks(&home);
        assert_eq!(plan.env["MEMORY_MCP_URL"], "http://127.0.0.1:4100/mcp");
        assert!(
            plan.argv.iter().any(|a| a.ends_with("agy")),
            "{:?}",
            plan.argv
        );
    }

    #[test]
    fn antigravity_plan_login_and_shape() {
        let f = fixture();
        // a Google login needs the token file the host's login left
        assert!(matches!(
            f.plan(
                Harness::Antigravity,
                AuthMode::Subscription,
                "a2",
                &[],
                false
            ),
            Err(Error::Unsupported(_))
        ));
        std::fs::create_dir_all(f.host_home.join(".gemini/antigravity-cli")).unwrap();
        std::fs::write(
            f.host_home
                .join(".gemini/antigravity-cli/antigravity-oauth-token"),
            "tok",
        )
        .unwrap();
        let (home, plan) = f
            .plan(
                Harness::Antigravity,
                AuthMode::Subscription,
                "a3",
                &[],
                true,
            )
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(home.join(".gemini/antigravity-cli/antigravity-oauth-token"))
                .unwrap(),
            "tok"
        );
        assert_eq!(plan.env["HTTPS_PROXY"], "http://127.0.0.1:4100");
        assert!(
            !plan.env.contains_key("CODE_ASSIST_ENDPOINT"),
            "Gemini CLI's variable; nothing shows agy reads it"
        );
        assert!(!plan.env.contains_key("GEMINI_API_KEY"));
        let settings = json_file(&home.join(".gemini/antigravity-cli/settings.json"));
        assert!(settings.get("modelProvider").is_none());
        let headless = Harness::Antigravity.headless("hi");
        assert_eq!(headless[0], "-p");
        assert!(headless.contains(&"--dangerously-skip-permissions".to_string()));
        assert!(headless.windows(2).any(|w| w == ["--mode", "accept-edits"]));
        assert_eq!(Harness::Antigravity.model_flag("m"), vec!["--model", "m"]);
        assert_eq!(Harness::Antigravity.provider(), "gemini");
        assert_eq!(Harness::Antigravity.subscription_provider(), "google");
        assert!(!Harness::Antigravity.accepts_provider());
    }

    fn assert_qwen_hooks(settings: &serde_json::Value) {
        // Claude Code's shape, with timeouts in seconds, for all three events
        for event in ["PreCompact", "SessionEnd", "PreToolUse"] {
            let group = &settings["hooks"][event][0];
            assert_eq!(group["matcher"], "*", "{event}");
            assert_eq!(
                group["hooks"][0]["command"], "/opt/memory/memory hook qwen",
                "{event}"
            );
            assert_eq!(group["hooks"][0]["timeout"], 30, "{event}: seconds");
        }
    }

    #[test]
    fn qwen_plan_key_and_provider() {
        let f = fixture();
        let (home, plan) = f
            .plan(Harness::Qwen, AuthMode::Key, "q1", &[], false)
            .unwrap();
        // an OpenAI-compatible harness: DashScope's compatible endpoint by default,
        // behind the egress, with the egress holding the real key
        assert_eq!(
            plan.env["OPENAI_BASE_URL"],
            "http://127.0.0.1:4100/dashscope/v1"
        );
        assert_eq!(plan.env["OPENAI_API_KEY"], "egress");
        assert!(
            !plan.env.contains_key("HTTPS_PROXY"),
            "a key needs no tunnel"
        );
        let s = json_file(&home.join(".qwen/settings.json"));
        assert_eq!(s["security"]["auth"]["selectedType"], "openai");
        assert_eq!(
            s["mcpServers"]["memory"]["httpUrl"],
            "http://127.0.0.1:4100/mcp"
        );
        assert_eq!(s["general"]["enableAutoUpdate"], false);
        assert_eq!(s["privacy"]["usageStatisticsEnabled"], false);
        assert_qwen_hooks(&s);
        assert_eq!(plan.env["MEMORY_MCP_URL"], "http://127.0.0.1:4100/mcp");
        assert!(
            plan.argv.iter().any(|a| a.ends_with("qwen")),
            "{:?}",
            plan.argv
        );
    }

    #[test]
    fn qwen_takes_a_provider_and_keeps_gemini_s_headless_shape() {
        let f = fixture();
        // another OpenAI-compatible destination on request
        let (_, plan) = f
            .plan_with(
                Harness::Qwen,
                AuthMode::Key,
                Some("openai"),
                "q2",
                &[],
                false,
                &[],
            )
            .unwrap();
        assert_eq!(
            plan.env["OPENAI_BASE_URL"],
            "http://127.0.0.1:4100/openai/v1"
        );
        let headless = Harness::Qwen.headless("hi");
        assert_eq!(headless, vec!["-p", "hi", "--yolo"]);
        assert_eq!(Harness::Qwen.model_flag("m"), vec!["-m", "m"]);
        assert_eq!(Harness::Qwen.provider(), "dashscope");
        assert_eq!(Harness::Qwen.subscription_provider(), "qwen");
        assert!(Harness::Qwen.accepts_provider());
    }

    #[test]
    fn qwen_plan_login_is_the_credential_file_behind_a_tunnel() {
        let f = fixture();
        // a Qwen OAuth login needs the credential file the host's login left
        assert!(matches!(
            f.plan(Harness::Qwen, AuthMode::Subscription, "q3", &[], false),
            Err(Error::Unsupported(_))
        ));
        std::fs::create_dir_all(f.host_home.join(".qwen")).unwrap();
        std::fs::write(
            f.host_home.join(".qwen/oauth_creds.json"),
            r#"{"access_token":"x","resource_url":"portal.qwen.ai"}"#,
        )
        .unwrap();
        let (home, plan) = f
            .plan(Harness::Qwen, AuthMode::Subscription, "q4", &[], true)
            .unwrap();
        assert!(
            home.join(".qwen/oauth_creds.json").is_file(),
            "login copied in"
        );
        assert_eq!(plan.env["HTTPS_PROXY"], "http://127.0.0.1:4100");
        assert!(!plan.env.contains_key("OPENAI_API_KEY"));
        assert!(!plan.env.contains_key("OPENAI_BASE_URL"));
        let s = json_file(&home.join(".qwen/settings.json"));
        assert_eq!(s["security"]["auth"]["selectedType"], "qwen-oauth");
        assert_qwen_hooks(&s);
    }
}
