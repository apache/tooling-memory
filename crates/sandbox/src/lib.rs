//! Confinement for a harness: what it may see, and the one way out.
//!
//! A [`Plan`] says what goes into the sandbox: a root with the system's binaries read-only,
//! a private home, the working tree, the memory binary, the egress socket, and nothing
//! else. The network namespace is empty. The harness reaches the egress through a relay
//! on loopback that forwards to the socket, and the egress is the only process with the
//! network.
//!
//! Eight harnesses are known. Each needs three things from the launcher, and the adapter
//! for each is the `setup_*` function below:
//!
//! | Harness | Model calls | Memory over MCP | Config the launcher owns |
//! |---|---|---|---|
//! | Claude Code | `ANTHROPIC_BASE_URL` | `--mcp-config` file | `~/.claude.json` |
//! | Codex | `model_providers.<id>.base_url` | `mcp_servers.<id>.url` | `CODEX_HOME/config.toml` |
//! | Gemini CLI | `GOOGLE_GEMINI_BASE_URL` | `mcpServers.<id>.httpUrl` | `~/.gemini/settings.json` |
//! | Antigravity CLI | `GOOGLE_GEMINI_BASE_URL`; a login tunnels | `mcpServers.<id>.serverUrl` | `~/.gemini/antigravity-cli/settings.json`, `~/.gemini/config/` |
//! | Qwen Code | `OPENAI_BASE_URL`; key only | `mcpServers.<id>.httpUrl` | `~/.qwen/settings.json` |
//! | Grok Build | `GROK_XAI_API_BASE_URL`; a login tunnels | `mcp_servers.<id>.url` | `~/.grok/config.toml` |
//! | Copilot CLI | no override; `HTTPS_PROXY` to the egress, which tunnels to allowed hosts | `mcp-config.json` | `COPILOT_HOME` |
//! | OpenCode | `provider.<id>.options.baseURL` | `mcp.<id>.url` | `XDG_CONFIG_HOME/opencode/opencode.json` |
//!
//! Backends:
//! - Linux: user, mount, network, uts, ipc and pid namespaces, native, unprivileged, no
//!   bubblewrap or container runtime needed. The plan's mounts build a root of its own,
//!   so things sit at fixed paths inside ([`Layout::namespaces`]).
//! - macOS: Seatbelt, through `sandbox-exec`, with a profile made from the plan
//!   ([`seatbelt`]). There is no mount namespace, so everything sits at its host path
//!   ([`Layout::seatbelt`]) and the plan's mounts are what the profile grants. Dry-coded:
//!   the profile is unit-tested everywhere, but the backend has not yet run on a Mac.
//! - Windows: run under WSL2.
//!
//! Where things sit inside is the [`Layout`], a property of the backend: [`prepare`]
//! uses the namespaces layout, [`prepare_in`] the one it is given.

use dm_core::Label;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
pub mod linux;
pub mod measure;
pub mod seatbelt;

pub use measure::Resources;

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
    /// This platform has no backend for the plan: namespaces need Linux, Seatbelt macOS.
    #[error("no confinement backend for this plan on this platform; namespaces need Linux or WSL2, Seatbelt needs macOS")]
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
    /// xAI's Grok Build, `grok`.
    Grok,
    /// Meta's Muse Code, `muse`.
    Muse,
    /// The pi coding agent, `pi`: four tools and extensions.
    Pi,
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
            Harness::Grok => "grok",
            Harness::Muse => "muse",
            Harness::Pi => "pi",
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
            Harness::Codex | Harness::Pi | Harness::Command => "openai",
            Harness::Gemini | Harness::Antigravity => "gemini",
            Harness::Qwen => "dashscope",
            Harness::Grok => "xai",
            Harness::Muse => "meta",
            Harness::Copilot => "github",
        }
    }

    /// The destination a login, rather than a key, talks to. Differs where the
    /// subscription backend is not the public API: Codex's ChatGPT login talks to
    /// the ChatGPT backend, Gemini's Google login to Code Assist, Grok Build's login to
    /// Grok's own proxy and relays.
    #[must_use]
    pub fn subscription_provider(self) -> &'static str {
        match self {
            Harness::Codex => "chatgpt",
            Harness::Gemini | Harness::Antigravity => "google",
            Harness::Grok => "grok",
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
            // headless, Claude Code omits its thinking unless asked, which leaves a bare
            // "Thinking" in the transcript before each decision; summaries say why
            Harness::Claude => vec![
                "-p".into(),
                p,
                "--dangerously-skip-permissions".into(),
                "--thinking-display".into(),
                "summarized".into(),
            ],
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
            Harness::Qwen | Harness::Grok => vec!["-p".into(), p, "--yolo".into()],
            // Muse's own approval and bubblewrap sandbox are off, since ours is the
            // boundary; --json makes its run a readable record, since its session store
            // is binary journals, and the launcher keeps that stream as the transcript
            // and its reminder observers, background agents of its own that spend
            // model calls at Muse's default effort, are off: nobody is there to remind
            Harness::Muse => vec![
                "exec".into(),
                "--yolo".into(),
                "--json".into(),
                "--disable-reminders".into(),
                p,
            ],
            // pi has no permission system of its own, so nothing to turn off; project-local
            // extensions are ignored, so only the launcher's runs; stdin is closed by the
            // sandbox, which pi otherwise waits on
            Harness::Pi => vec!["-p".into(), "-na".into(), p],
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
            Harness::Claude
            | Harness::Copilot
            | Harness::Antigravity
            | Harness::Muse
            | Harness::Pi => {
                vec!["--model".into(), model.into()]
            }
            Harness::Codex
            | Harness::Gemini
            | Harness::Qwen
            | Harness::Grok
            | Harness::OpenCode => {
                vec!["-m".into(), model.into()]
            }
            Harness::Command => Vec::new(),
        }
    }

    /// Every harness, in the order the launcher lists them.
    pub const ALL: [Harness; 11] = [
        Harness::Claude,
        Harness::Codex,
        Harness::Gemini,
        Harness::Antigravity,
        Harness::Qwen,
        Harness::Grok,
        Harness::Muse,
        Harness::Copilot,
        Harness::OpenCode,
        Harness::Pi,
        Harness::Command,
    ];

    /// The file a subscription login leaves under the home, which subscription mode
    /// copies in, and how to make it. Copilot's login is `GH_TOKEN`, not a file; Qwen
    /// and pi have no login mode the launcher can carry, and a command has no login of
    /// its own.
    #[must_use]
    pub fn login(self) -> Option<Login> {
        let (file, how) = match self {
            Harness::Claude => (".claude/.credentials.json", "a Claude Code login"),
            Harness::Codex => (
                ".codex/auth.json",
                "`codex login` (or `codex login --device-auth`)",
            ),
            Harness::Gemini => (".gemini/oauth_creds.json", "a Gemini CLI Google login"),
            Harness::Antigravity => (
                ".gemini/antigravity-cli/antigravity-oauth-token",
                "an Antigravity CLI Google login (run `agy` once on the host)",
            ),
            Harness::Grok => (".grok/auth.json", "`grok login`"),
            Harness::Muse => (".config/muse/auth.json", "`muse login`"),
            Harness::OpenCode => (
                ".local/share/opencode/auth.json",
                "an OpenCode GitHub Copilot login (`opencode auth login`, GitHub Copilot)",
            ),
            Harness::Qwen | Harness::Copilot | Harness::Pi | Harness::Command => return None,
        };
        Some(Login { file, how })
    }

    /// Whether `--provider` may point this harness at another destination.
    #[must_use]
    pub fn accepts_provider(self) -> bool {
        matches!(
            self,
            Harness::Codex | Harness::Qwen | Harness::OpenCode | Harness::Pi | Harness::Command
        )
    }
}

/// Where a harness's login lives under the home, and what makes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Login {
    /// Relative to the home.
    pub file: &'static str,
    /// What to run, or do, on the host to leave it there.
    pub how: &'static str,
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
    /// Where things sit inside, and so which backend runs the plan.
    pub layout: Layout,
    /// The host's home, which no backend may hand the harness: Seatbelt's profile
    /// refuses a grant of it, or of anything above it.
    pub host_home: Option<PathBuf>,
}

/// Which backend a plan is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// Linux namespaces: a root of the sandbox's own, built from the plan's mounts.
    Namespaces,
    /// macOS Seatbelt: the host's own paths, with the plan's mounts as the grants of a
    /// deny-by-default profile.
    Seatbelt,
}

/// Where a plan puts things as the harness sees them. A property of the backend: one
/// with a mount namespace puts each at a fixed path of the sandbox's own; one without
/// has the harness see each where it is on the host, so the harness's configuration is
/// written against the host's paths.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    /// The backend this layout is for.
    pub backend: Backend,
    /// The private home.
    pub home: PathBuf,
    /// The egress socket.
    pub socket: PathBuf,
    /// The memory binary, which runs the relay and the hooks.
    pub memory: PathBuf,
    /// The loopback address the relay listens on, as `host:port`.
    pub relay: String,
    /// The projection, when there is one to mount.
    pub projection: Option<PathBuf>,
    /// The temporary directory, `TMPDIR`.
    pub tmp: PathBuf,
}

impl Layout {
    /// The namespaces backend's: the [`INSIDE_HOME`], [`INSIDE_SOCKET`],
    /// [`INSIDE_MEMORY`], [`INSIDE_RELAY`] and [`INSIDE_PROJECTION`] constants, and a
    /// `/tmp` of the sandbox's own.
    #[must_use]
    pub fn namespaces() -> Layout {
        Layout {
            backend: Backend::Namespaces,
            home: PathBuf::from(INSIDE_HOME),
            socket: PathBuf::from(INSIDE_SOCKET),
            memory: PathBuf::from(INSIDE_MEMORY),
            relay: INSIDE_RELAY.to_string(),
            projection: Some(PathBuf::from(INSIDE_PROJECTION)),
            tmp: PathBuf::from("/tmp"),
        }
    }

    /// Seatbelt's: everything at its host path, since macOS has no mount namespace. The
    /// relay listens on `relay`, a loopback port the launcher picked for this session,
    /// since the host's loopback is shared with every other session; the temporary
    /// directory is under the private home, since the host's `/tmp` is shared too.
    #[must_use]
    pub fn seatbelt(req: &Request<'_>, relay: &str) -> Layout {
        Layout {
            backend: Backend::Seatbelt,
            home: req.home.to_path_buf(),
            socket: req.egress_socket.to_path_buf(),
            memory: req.memory_binary.to_path_buf(),
            relay: relay.to_string(),
            projection: req.projection.map(Path::to_path_buf),
            tmp: req.home.join(".tmp"),
        }
    }

    /// A path under the private home, as the harness sees it. The box's paths are Unix
    /// ones whatever the host, Linux in a container launched from Windows among them, so
    /// `/` joins them rather than the host's separator.
    fn under_home(&self, rel: &str) -> String {
        let home = self.home.display().to_string();
        format!("{}/{rel}", home.trim_end_matches('/'))
    }

    /// The shell command that runs the memory's hook for `harness`.
    fn hook(&self, harness: &str) -> String {
        format!("{} hook {harness}", shell_word(&self.memory))
    }

    /// The memory binary as a JSON, and so JavaScript, string literal.
    fn memory_literal(&self) -> String {
        serde_json::to_string(&self.memory.display().to_string()).unwrap_or_default()
    }
}

/// A path as one shell word: as it is when nothing in it is special to a shell, single-
/// quoted otherwise. A hook command is run by the harness through a shell, and a host
/// path on macOS may hold a space.
fn shell_word(path: &Path) -> String {
    let text = path.display().to_string();
    let plain = |c: char| c.is_ascii_alphanumeric() || "/._-+:@%,=".contains(c);
    if !text.is_empty() && text.chars().all(plain) {
        text
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}

/// A string as the inside of a TOML basic string.
fn toml_escaped(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
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
    /// `.gemini/oauth_creds.json`, Grok Build's `.grok/auth.json`. Nothing else is read
    /// from it and it is never mounted.
    pub host_home: Option<&'a Path>,
    /// Credentials the harness itself must hold, passed into its environment. For
    /// subscription modes that are a token rather than a file, such as Copilot's
    /// `GH_TOKEN`.
    pub harness_env: &'a [(String, String)],
    /// The model the run was given, for an adapter whose config has to name it:
    /// OpenCode lists a custom provider's models, and cannot ask models.dev here.
    pub model: Option<&'a str>,
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
    /// mount read-only at the layout's projection; `None` when there is none.
    pub projection: Option<&'a Path>,
}

impl Request<'_> {
    /// The working tree's path as the harness sees it.
    #[must_use]
    pub fn workdir_inside(&self) -> &Path {
        self.workdir_inside.unwrap_or(self.workdir)
    }
}

/// Where things sit inside the sandbox under the namespaces backend, [`Layout::namespaces`].
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
/// one directory per session as `<harness>/<session>/`: the launcher's own, never under
/// the private home the harness can write.
pub const PROJECTION: &str = "state/projection";
/// Where hooks leave notes for the launcher, under the private home.
pub const OUTBOX: &str = ".memory-outbox";

fn write(path: &Path, text: &str) -> Result<(), Error> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).map_err(io(p))?;
    }
    std::fs::write(path, text).map_err(io(path))
}

/// Copy the harness's own login in, as [`Harness::login`] names it.
fn copy_own_login(req: &Request<'_>) -> Result<(), Error> {
    let login = req
        .harness
        .login()
        .ok_or_else(|| Error::Unsupported(format!("{} has no login file", req.harness.binary())))?;
    copy_login(req, login.file, login.how)
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
fn base_env(harness_env: &[(String, String)], at: &Layout) -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    env.insert("HOME".into(), at.home.display().to_string());
    // where every harness's hooks leave notes for the launcher
    env.insert("MEMORY_OUTBOX".into(), at.under_home(OUTBOX));
    env.insert("USER".into(), "user".into());
    env.insert("LOGNAME".into(), "user".into());
    env.insert("TMPDIR".into(), at.tmp.display().to_string());
    for k in [
        "TERM",
        "COLORTERM",
        "LANG",
        "LC_ALL",
        "NO_COLOR",
        "FORCE_COLOR",
        // the hooks log what they are sent, for an adapter written from a harness's docs
        "MEMORY_HOOK_DEBUG",
        "MEMORY_EGRESS_DEBUG",
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
fn projection_env(req: &Request<'_>, at: &Layout, env: &mut BTreeMap<String, String>) {
    if let Some(inside) = at.projection.as_ref() {
        if req.projection.is_some_and(Path::is_dir) {
            env.insert("MEMORY_PROJECTION".into(), inside.display().to_string());
        }
    }
}

/// Everything mounted into the sandbox, in order, and the directory the harness's own
/// binaries want on PATH. The host's system is mounted only where the backend builds a
/// root of its own; under Seatbelt the system is the profile's, and these are its grants.
fn plan_mounts(
    req: &Request<'_>,
    at: &Layout,
    harness_bin: &Path,
) -> (Vec<Mount>, Option<PathBuf>) {
    let mut mounts = match at.backend {
        Backend::Namespaces => system_mounts(),
        Backend::Seatbelt => Vec::new(),
    };
    mounts.push(Mount {
        source: req.memory_binary.to_path_buf(),
        target: at.memory.clone(),
        read_only: true,
    });
    let (harness_m, extra_path) = harness_mounts(harness_bin);
    mounts.extend(harness_m);
    mounts.push(Mount {
        source: req.home.to_path_buf(),
        target: at.home.clone(),
        read_only: false,
    });
    mounts.push(Mount {
        source: req.workdir.to_path_buf(),
        target: req.workdir_inside().to_path_buf(),
        read_only: false,
    });
    mounts.extend(projection_mount(req, at));
    mounts.push(Mount {
        source: req.egress_socket.to_path_buf(),
        target: at.socket.clone(),
        read_only: false,
    });
    (mounts, extra_path)
}

/// The projection, when the launcher rendered one: its own directory, mounted read-only,
/// so the harness reads it there and nowhere else and cannot edit it into lies. Every
/// backend that names the layout's projection in the environment mounts it; a container
/// backend does so itself, from the host's view of the path.
fn projection_mount(req: &Request<'_>, at: &Layout) -> Option<Mount> {
    let target = at.projection.as_ref()?;
    req.projection.filter(|p| p.is_dir()).map(|p| Mount {
        source: p.to_path_buf(),
        target: target.clone(),
        read_only: true,
    })
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

/// The destination this run's model calls go to: the harness's own, the one asked for
/// where the harness takes one, or the gateway's when the gateway is the route.
fn provider_of(req: &Request<'_>) -> Result<String, Error> {
    let own = [req.harness.provider(), req.harness.subscription_provider()];
    Ok(match req.provider {
        Some(p)
            if req.harness.accepts_provider()
                || own.contains(&p)
                || req.auth == AuthMode::Gateway =>
        {
            p.to_string()
        }
        Some(p) => {
            return Err(Error::Unsupported(format!(
                "{} does not take --provider ({p}); its model calls go to {}",
                req.harness.binary(),
                req.harness.provider()
            )))
        }
        None => req.harness.provider().to_string(),
    })
}

/// The working tree and the sandbox's own mounts must not overlap. One mounted after the
/// tree, under it or at it, hides that part of the tree; a tree at or above one of them
/// hides the sandbox's own, the home among them. A tree under the home stays: the home
/// is mounted first.
///
/// Where the layout is the host's own, as Seatbelt's is, nothing is mounted and nothing
/// hidden, but the tree is granted read-write: it may not sit in the read-only
/// projection, nor hold the private home, the projection, the memory binary that runs
/// the hooks, or the egress socket.
fn refuse_a_hidden_workdir(req: &Request<'_>, at: &Layout) -> Result<(), Error> {
    let inside = req.workdir_inside();
    let projection = at.projection.as_deref();
    let mounted = at.backend == Backend::Namespaces;
    let after_the_tree = [
        projection,
        mounted.then(|| at.memory.parent().unwrap_or(&at.memory)),
        mounted.then(|| at.socket.parent().unwrap_or(&at.socket)),
    ];
    let own = [
        Some(at.home.as_path()),
        projection,
        Some(at.memory.as_path()),
        Some(at.socket.as_path()),
    ];
    let hidden = after_the_tree
        .into_iter()
        .flatten()
        .find(|own| inside.starts_with(own))
        .or_else(|| {
            own.into_iter()
                .flatten()
                .find(|own| own.starts_with(inside))
        });
    match hidden {
        Some(own) => Err(Error::Unsupported(format!(
            "the working tree {} would sit under the sandbox's own {}; run from another directory",
            inside.display(),
            own.display()
        ))),
        None => Ok(()),
    }
}

/// Build the plan for the namespaces backend: write the harness's configuration into the
/// private home and decide mounts, environment, and command.
///
/// # Errors
///
/// A harness not on PATH, an unsupported harness, auth and provider combination, or a
/// file under the home that cannot be written.
pub fn prepare(req: &Request<'_>) -> Result<Plan, Error> {
    prepare_in(req, Layout::namespaces())
}

/// Build the plan for the backend `at` is the layout of: the harness's configuration is
/// written against the layout's paths, and the mounts put each thing there.
///
/// # Errors
///
/// As [`prepare`], and the harness image under a layout that is not the namespaces
/// backend's.
pub fn prepare_in(req: &Request<'_>, at: Layout) -> Result<Plan, Error> {
    refuse_a_hidden_workdir(req, &at)?;
    let image = req.image || req.rootfs.is_some();
    if image && at.backend != Backend::Namespaces {
        return Err(Error::Unsupported(
            "the harness image runs under Linux namespaces or in a container, not under Seatbelt"
                .into(),
        ));
    }
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
    if at.backend == Backend::Seatbelt {
        std::fs::create_dir_all(&at.tmp).map_err(io(&at.tmp))?;
    }
    let relay = format!("http://{}", at.relay);
    let provider = provider_of(req)?;

    let mut env = base_env(req.harness_env, &at);
    // the memory server's address, for the hooks as for the harness
    env.insert("MEMORY_MCP_URL".into(), format!("{relay}/mcp"));
    projection_env(req, &at, &mut env);
    let (mounts, extra_path) = mounts_of(req, &at, image, &harness_bin);
    let links = match at.backend {
        Backend::Namespaces => root_links(req.rootfs.unwrap_or(Path::new("/"))),
        Backend::Seatbelt => Vec::new(),
    };
    let memory_dir = at.memory.parent().unwrap_or(Path::new("/"));
    let mut path = format!("{}:/usr/local/bin:/usr/bin:/bin", memory_dir.display());
    if let Some(p) = extra_path {
        path = format!("{}:{path}", p.display());
    }
    env.insert("PATH".into(), path);
    let mut argv = vec![
        at.memory.display().to_string(),
        "relay".into(),
        "--socket".into(),
        at.socket.display().to_string(),
        "--listen".into(),
        at.relay.clone(),
        "--".into(),
        harness_bin.display().to_string(),
    ];
    // what an adapter adds before the harness's own arguments, and what it adds after
    // them: a harness whose flags belong to a subcommand, as Muse's do to `exec`,
    // takes them at the end, where every one of them accepts options after the prompt
    let mut tail = Vec::new();
    setup_harness(req, &at, &relay, &provider, &mut env, &mut argv, &mut tail)?;
    argv.extend(harness_args.iter().cloned());
    argv.extend(tail);

    Ok(Plan {
        mounts,
        env,
        argv,
        cwd: req.workdir_inside().to_path_buf(),
        hostname: "memory-sandbox".into(),
        handling_cap: req.destination_ceiling,
        links,
        layout: at,
        host_home: req.host_home.map(Path::to_path_buf),
    })
}

/// The plan's mounts, and the directory the harness's own binaries want on PATH: the
/// image's system with the session's own on top, the session's own alone in a harness
/// container, or this host's system and the harness's install.
fn mounts_of(
    req: &Request<'_>,
    at: &Layout,
    image: bool,
    harness_bin: &Path,
) -> (Vec<Mount>, Option<PathBuf>) {
    match req.rootfs {
        Some(rootfs) => {
            let mut m = rootfs_mounts(rootfs);
            // the launcher's own binary over the image's copy, so the relay and the
            // hooks inside are the launcher's build, not the image's; a container
            // backend has no such mount and runs what its image carries
            m.push(Mount {
                source: req.memory_binary.to_path_buf(),
                target: at.memory.clone(),
                read_only: true,
            });
            m.extend(image_mounts(req));
            m.extend(projection_mount(req, at));
            m.push(Mount {
                source: req.egress_socket.to_path_buf(),
                target: at.socket.clone(),
                read_only: false,
            });
            (m, None)
        }
        None if image => (image_mounts(req), None),
        None => plan_mounts(req, at, harness_bin),
    }
}

/// The adapter for the requested harness: each writes its config into the private home
/// and sets the environment that points it at the relay.
fn setup_harness(
    req: &Request<'_>,
    at: &Layout,
    relay: &str,
    provider: &str,
    env: &mut BTreeMap<String, String>,
    argv: &mut Vec<String>,
    tail: &mut Vec<String>,
) -> Result<(), Error> {
    match req.harness {
        Harness::Claude => setup_claude(req, at, relay, provider, env, argv),
        Harness::Codex => setup_codex(req, at, relay, provider, env),
        Harness::Gemini => setup_gemini(req, at, relay, provider, env),
        Harness::Antigravity => setup_antigravity(req, at, relay, provider, env),
        Harness::Qwen => setup_qwen(req, at, relay, provider, env),
        Harness::Grok => setup_grok(req, at, relay, provider, env),
        Harness::Muse => setup_muse(req, at, relay, provider, env, tail),
        Harness::Pi => setup_pi(req, at, relay, provider, env),
        Harness::Copilot => setup_copilot(req, at, relay, env),
        Harness::OpenCode => setup_opencode(req, at, relay, provider, env),
        Harness::Command => {
            setup_command(req, relay, provider, env);
            Ok(())
        }
    }
}

fn setup_claude(
    req: &Request<'_>,
    at: &Layout,
    relay: &str,
    provider: &str,
    env: &mut BTreeMap<String, String>,
    argv: &mut Vec<String>,
) -> Result<(), Error> {
    let mcp = serde_json::json!({
        "mcpServers": { "memory": { "type": "http", "url": format!("{relay}/mcp") } }
    });
    write(&req.home.join(".claude-mcp.json"), &mcp.to_string())?;
    claude_global_config(&req.home.join(".claude.json"), req.workdir_inside())?;
    claude_hooks(at, &req.home.join(".claude/settings.json"))?;
    claude_thinking_shown(&req.home.join(".claude/settings.json"))?;
    // anthropic, or the gateway's destination when that is the route
    env.insert("ANTHROPIC_BASE_URL".into(), format!("{relay}/{provider}"));
    env.insert("DISABLE_AUTOUPDATER".into(), "1".into());
    match req.auth {
        AuthMode::Subscription => {
            copy_own_login(req)?;
        }
        AuthMode::Gateway | AuthMode::Key => {
            // the egress injects the real credential; this one only has to exist
            env.insert("ANTHROPIC_AUTH_TOKEN".into(), "egress".into());
        }
    }
    argv.push("--mcp-config".into());
    argv.push(at.under_home(".claude-mcp.json"));
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
fn claude_hooks(at: &Layout, path: &Path) -> Result<(), Error> {
    merge_hooks(
        at,
        path,
        "claude",
        &["PreCompact", "SessionEnd", "PreToolUse"],
        30,
    )
}

/// Claude Code asks for its thinking as summaries, so its transcript says why before a
/// decision instead of a bare "Thinking" with nothing in it. Off by default in Claude
/// Code, where thinking comes back as a signature alone; the record is the reason to
/// turn it on.
fn claude_thinking_shown(path: &Path) -> Result<(), Error> {
    let mut settings: serde_json::Value = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    if !settings.is_object() {
        settings = serde_json::json!({});
    }
    settings["showThinkingSummaries"] = serde_json::Value::Bool(true);
    write(
        path,
        &serde_json::to_string_pretty(&settings).unwrap_or_default(),
    )
}

/// Merge the hooks for `events` into a JSON settings file that may hold other things:
/// Claude Code's `settings.json`, Codex's `hooks.json`, Qwen Code's `settings.json`.
/// All share the shape, `hooks → event → matcher groups → command handlers`; they differ
/// in the name of the compaction event and in how long a session-end hook may run.
fn merge_hooks(
    at: &Layout,
    path: &Path,
    harness: &str,
    events: &[&str],
    timeout: u32,
) -> Result<(), Error> {
    let mut settings: serde_json::Value = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    if !settings.is_object() {
        settings = serde_json::json!({});
    }
    let hook = serde_json::json!([{
        "matcher": "*",
        "hooks": [{ "type": "command", "command": at.hook(harness), "timeout": timeout }]
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
    at: &Layout,
    relay: &str,
    provider: &str,
    env: &mut BTreeMap<String, String>,
) -> Result<(), Error> {
    let config = if req.auth == AuthMode::Subscription {
        // a ChatGPT login talks to the ChatGPT backend, which refuses relayed requests, so
        // Codex's HTTPS goes through the egress as a tunnel to the ChatGPT hosts, as
        // Copilot CLI's does; the built-in provider and its backend stay as they are
        copy_own_login(req)?;
        proxy_via(relay, env);
        // the forward variant, kept for diagnosis: the backend behind the egress
        let base = if std::env::var_os("MEMORY_CHATGPT_FORWARD").is_some() {
            format!("chatgpt_base_url = \"{relay}/chatgpt/\"\n\n")
        } else {
            String::new()
        };
        format!(
            r#"# written by memory launch; edits are overwritten
{base}show_raw_agent_reasoning = true

[features]
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
# the reasoning an open model returns is kept in the rollout, so the record has the why
show_raw_agent_reasoning = true

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
    merge_hooks(at, &hooks, "codex", &["PreCompact", "SessionEnd"], 3)?;
    merge_hooks(at, &hooks, "codex", &["PreToolUse"], 30)?;
    env.insert("CODEX_HOME".into(), at.under_home(".codex"));
    Ok(())
}

fn setup_gemini(
    req: &Request<'_>,
    at: &Layout,
    relay: &str,
    provider: &str,
    env: &mut BTreeMap<String, String>,
) -> Result<(), Error> {
    let subscription = req.auth == AuthMode::Subscription;
    if subscription {
        // a Google login talks to Code Assist, not the Gemini API: its endpoint moves to
        // the egress, and the token refresh at oauth2.googleapis.com goes through the
        // egress as a tunnel
        copy_own_login(req)?;
        // which account and tier; optional, Gemini asks Google again without it
        let _ = copy_login(req, ".gemini/google_accounts.json", "");
        proxy_via(relay, env);
        env.insert("CODE_ASSIST_ENDPOINT".into(), format!("{relay}/google"));
        if let Some(project) = env_passthrough("GOOGLE_CLOUD_PROJECT") {
            env.insert("GOOGLE_CLOUD_PROJECT".into(), project);
        }
    } else {
        env.insert(
            "GOOGLE_GEMINI_BASE_URL".into(),
            format!("{relay}/{provider}"),
        );
        // the egress injects the real key as x-goog-api-key and drops any ?key= it sees
        env.insert("GEMINI_API_KEY".into(), "egress".into());
    }
    let auth_type = if subscription {
        "oauth-personal"
    } else {
        "gemini-api-key"
    };
    // Gemini's hook timeout is in milliseconds, unlike Claude Code's and Codex's seconds.
    // BeforeTool is Gemini's name for the event before a tool runs; the hook waits ten
    // seconds on the memory server and Gemini continues when it kills a hook, so this
    // one gets longer than that wait, or a stalled server would be a read let through
    let hook = |timeout: u32| {
        serde_json::json!([{
            "matcher": "*",
            "hooks": [{ "type": "command", "command": at.hook("gemini"), "timeout": timeout }]
        }])
    };
    let mut settings = serde_json::json!({
        "hooks": { "PreCompress": hook(10_000), "SessionEnd": hook(10_000), "BeforeTool": hook(30_000) },
        "security": { "auth": { "selectedType": auth_type } },
        "general": { "enableAutoUpdate": false },
        "privacy": { "usageStatisticsEnabled": false },
        "mcpServers": { "memory": { "httpUrl": format!("{relay}/mcp") } }
    });
    include_projection(req, at, &mut settings);
    write(
        &req.home.join(".gemini/settings.json"),
        &serde_json::to_string_pretty(&settings).unwrap_or_default(),
    )?;
    // the working tree is the one thing mounted read-write; it is trusted by construction
    env.insert("GEMINI_CLI_TRUST_WORKSPACE".into(), "true".into());
    Ok(())
}

/// Gemini CLI and its forks refuse their file tools a path outside the workspace, and a
/// model so refused falls back to a shell the hook does not see; when the projection is
/// mounted, the settings include it in the workspace, so a read of it goes through the
/// tool, and the hook, as a `get`.
fn include_projection(req: &Request<'_>, at: &Layout, settings: &mut serde_json::Value) {
    if let Some(inside) = at.projection.as_ref() {
        if req.projection.is_some_and(Path::is_dir) {
            settings["context"]["includeDirectories"] =
                serde_json::json!([inside.display().to_string()]);
        }
    }
}

/// Antigravity CLI keeps Gemini CLI's variables for a key, `GEMINI_API_KEY` behind
/// `GOOGLE_GEMINI_BASE_URL`, once its settings name the Gemini API as the model
/// provider; a Google login is a token file the host's login left, with its HTTPS through
/// the egress as a tunnel to Google's hosts, since the login's endpoint has no override
/// of its own that is known to work. MCP servers and hooks live in files of their own
/// under `.gemini/config`, not in the settings, and a remote server is `serverUrl`.
fn setup_antigravity(
    req: &Request<'_>,
    at: &Layout,
    relay: &str,
    provider: &str,
    env: &mut BTreeMap<String, String>,
) -> Result<(), Error> {
    let subscription = req.auth == AuthMode::Subscription;
    let mut settings = serde_json::json!({});
    if subscription {
        copy_own_login(req)?;
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
        env.insert(
            "GOOGLE_GEMINI_BASE_URL".into(),
            format!("{relay}/{provider}"),
        );
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
        "command": at.hook("antigravity"),
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
/// other OpenAI-compatible one on request. There is no login mode: Qwen's free OAuth
/// tier ended in April 2026 and the harness exits when it is selected. MCP servers and
/// hooks live in the settings, the hooks in Claude Code's shape with timeouts in
/// seconds.
fn setup_qwen(
    req: &Request<'_>,
    at: &Layout,
    relay: &str,
    provider: &str,
    env: &mut BTreeMap<String, String>,
) -> Result<(), Error> {
    if req.auth == AuthMode::Subscription {
        return Err(Error::Unsupported(
            "qwen has no login mode: Qwen's free OAuth tier ended in April 2026; use \
             --auth key with DASHSCOPE_API_KEY (or --provider openai with OPENAI_API_KEY)"
                .into(),
        ));
    }
    env.insert("OPENAI_BASE_URL".into(), format!("{relay}/{provider}/v1"));
    // the egress injects the real key; this one only has to exist
    env.insert("OPENAI_API_KEY".into(), "egress".into());
    let mut settings = serde_json::json!({
        "security": { "auth": { "selectedType": "openai" } },
        "general": { "enableAutoUpdate": false },
        "privacy": { "usageStatisticsEnabled": false },
        "mcpServers": { "memory": { "httpUrl": format!("{relay}/mcp") } }
    });
    include_projection(req, at, &mut settings);
    let path = req.home.join(".qwen/settings.json");
    write(
        &path,
        &serde_json::to_string_pretty(&settings).unwrap_or_default(),
    )?;
    merge_hooks(
        at,
        &path,
        "qwen",
        &["PreCompact", "SessionEnd", "PreToolUse"],
        30,
    )
}

/// Grok Build is a Rust binary that keeps its config in `~/.grok/config.toml`. Only
/// API-key inference reads `GROK_XAI_API_BASE_URL`, so a key is `XAI_API_KEY` behind it;
/// a login is the token file `grok login` left, copied in, and its calls go to Grok's
/// own proxy and websocket relays, through the egress as a tunnel to those hosts in this
/// first cut: the proxy has an override, `GROK_CLI_CHAT_PROXY_BASE_URL`, and speaks the
/// Responses API, so a forward through it is the metered route to prove on a real
/// login. A stored session token outranks a key and goes to the proxy, so a key run
/// removes one left in the home by an earlier login run. MCP servers and hooks live in
/// the same file; a hook group takes no matcher, since an absent one selects every
/// tool, and its timeout is in seconds with a default of five, too short for a `get`.
/// Grok checks a model id against its catalog, so a model of the destination's own,
/// a gateway's say, is declared as a `[model.<id>]` block of the config with its base,
/// key and chat-completions backend, and `-m <id>` then selects it.
fn setup_grok(
    req: &Request<'_>,
    at: &Layout,
    relay: &str,
    provider: &str,
    env: &mut BTreeMap<String, String>,
) -> Result<(), Error> {
    if req.auth == AuthMode::Subscription {
        copy_own_login(req)?;
        proxy_via(relay, env);
    } else {
        // xai on a key; the gateway's destination when that is the route
        env.insert(
            "GROK_XAI_API_BASE_URL".into(),
            format!("{relay}/{provider}/v1"),
        );
        // the egress injects the real key; this one only has to exist
        env.insert("XAI_API_KEY".into(), "egress".into());
        let stale = req.home.join(".grok/auth.json");
        if let Err(e) = std::fs::remove_file(&stale) {
            if e.kind() != std::io::ErrorKind::NotFound {
                return Err(io(&stale)(e));
            }
        }
    }
    let command = toml_escaped(&at.hook("grok"));
    let hook = |event: &str| {
        format!(
            r#"[[hooks.{event}]]
hooks = [{{ type = "command", command = "{command}", timeout = 30 }}]
"#
        )
    };
    // a model Grok's catalog does not know, declared as the destination's own
    let custom = match (req.auth, req.model) {
        (AuthMode::Subscription, _) | (_, None) => String::new(),
        (_, Some(m)) => {
            let id = m.replace(['"', '\\'], "");
            format!(
                r#"[model."{id}"]
name = "{id}"
model = "{id}"
base_url = "{relay}/{provider}/v1"
api_key = "egress"
api_backend = "chat_completions"
context_window = 131072

"#
            )
        }
    };
    let config = format!(
        r#"# written by memory launch; edits are overwritten
[cli]
auto_update = false

[features]
telemetry = false
feedback = false
remote_fetch = false

{custom}[mcp_servers.memory]
url = "{relay}/mcp"

{}{}{}"#,
        hook("PreToolUse"),
        hook("SessionEnd"),
        hook("PreCompact"),
    );
    write(&req.home.join(".grok/config.toml"), &config)?;
    env.insert("GROK_DISABLE_AUTOUPDATER".into(), "1".into());
    Ok(())
}

/// Muse Code is a statically linked binary behind a self-updating launcher. Its settings
/// are `~/.config/muse/settings.json`, which must carry `schema_version`, with MCP
/// servers under `mcp_servers` as `streamable_http` entries and hooks in Claude Code's
/// shape under `hooks`, which it reads with the same events and answers; it hands a hook
/// no transcript path, since its session store is binary journals. The model endpoint
/// is set with `--base-url`, which only its `exec` subcommand and the TUI take, so it
/// goes after the harness's arguments; a key is `META_API_KEY`, which outranks the
/// login, so a key run never sees a login the home kept. The first call on that base is
/// `GET /muse-code/models`, the model catalog, before any inference, so the destination
/// must forward the whole of `api.meta.ai`, not a path under it. A login is the file
/// `muse login` leaves, copied in; its calls go to the same host, so the same forward
/// carries them as a pass-through, which is the metered route to prove on a real login.
/// The launcher's update check is off, since the image pins what it carries.
fn setup_muse(
    req: &Request<'_>,
    at: &Layout,
    relay: &str,
    provider: &str,
    env: &mut BTreeMap<String, String>,
    tail: &mut Vec<String>,
) -> Result<(), Error> {
    if req.auth == AuthMode::Subscription {
        copy_own_login(req)?;
    } else {
        // the egress injects the real key; this one only has to exist
        env.insert("META_API_KEY".into(), "egress".into());
    }
    env.insert("MUSE_NO_AUTO_UPDATE".into(), "1".into());
    env.insert("MUSE_LOGIN".into(), "0".into());
    tail.extend(["--base-url".to_string(), format!("{relay}/{provider}/v1")]);
    if req.auth == AuthMode::Gateway {
        // Muse asks for `high` effort by default, which a gateway's open models refuse
        // (vLLM takes low, medium and xhigh); medium, as Claude Code gets there
        tail.extend(["--reasoning-effort".to_string(), "medium".to_string()]);
    }
    let path = req.home.join(".config/muse/settings.json");
    let mut settings: serde_json::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    if !settings.is_object() {
        settings = serde_json::json!({});
    }
    settings["schema_version"] = serde_json::json!(1);
    settings["mcp_servers"]["memory"] = serde_json::json!({
        "transport": "streamable_http",
        "url": format!("{relay}/mcp"),
        "mode": "required",
        "enabled": true
    });
    settings["telemetry"] = serde_json::json!({ "enabled": false });
    write(
        &path,
        &serde_json::to_string_pretty(&settings).unwrap_or_default(),
    )?;
    merge_hooks(
        at,
        &path,
        "muse",
        &["PreCompact", "SessionEnd", "PreToolUse"],
        30,
    )
}

/// The pi coding agent keeps its configuration under `~/.pi/agent`: `models.json` for
/// providers of one's own, `mcp.json` for MCP servers, `extensions/` for TypeScript it
/// loads directly. A destination is a provider in `models.json`, named after it, with
/// the egress as `baseUrl`, OpenAI's chat completions as the API and the model named,
/// since pi picks a model from the providers' lists and a provider with no models
/// offers none: a pi run wants `--model`. pi has no hooks; an extension of the
/// launcher's runs the hook binary instead, on `tool_call`, where it may block with a
/// reason the model sees, and on `session_shutdown`; there is no event before
/// compaction. Its sessions are JSON Lines under the agent directory. pi's logins are
/// its own built-in provider's and Anthropic's, which Anthropic prohibits for third-party
/// clients, so a login mode is refused.
fn setup_pi(
    req: &Request<'_>,
    at: &Layout,
    relay: &str,
    provider: &str,
    env: &mut BTreeMap<String, String>,
) -> Result<(), Error> {
    if req.auth == AuthMode::Subscription {
        return Err(Error::Unsupported(
            "pi has no login the launcher can carry: its Anthropic login is something Anthropic prohibits for third-party clients; use --auth key or --auth gateway with --model".into(),
        ));
    }
    let Some(model) = req.model.filter(|m| !m.trim().is_empty()) else {
        return Err(Error::Unsupported(
            "pi picks a model from its providers' lists, so a run needs --model".into(),
        ));
    };
    let agent = req.home.join(".pi/agent");
    let models = serde_json::json!({
        "providers": {
            provider: {
                "baseUrl": format!("{relay}/{provider}/v1"),
                "api": "openai-completions",
                "apiKey": "egress",
                "models": [{ "id": model.trim(), "name": model.trim() }]
            }
        }
    });
    write(
        &agent.join("models.json"),
        &serde_json::to_string_pretty(&models).unwrap_or_default(),
    )?;
    let mcp = serde_json::json!({
        "mcpServers": { "memory": { "url": format!("{relay}/mcp") } }
    });
    write(
        &agent.join("mcp.json"),
        &serde_json::to_string_pretty(&mcp).unwrap_or_default(),
    )?;
    let memory = at.memory_literal();
    let extension = format!(
        r#"// written by memory launch; edits are overwritten. The launcher's hooks, as a pi
// extension: pi has none of its own, so the hook binary is run from here.
import {{ spawnSync }} from "node:child_process";

function hook(event: string, payload: Record<string, unknown>): any {{
  const r = spawnSync({memory}, ["hook", "pi"], {{
    input: JSON.stringify({{ hook_event_name: event, ...payload }}),
    encoding: "utf8",
    timeout: 30000,
  }});
  if (!r.stdout) return null;
  try {{ return JSON.parse(r.stdout); }} catch {{ return null; }}
}}

function sessionOf(ctx: any): {{ session_id: string; transcript_path: string | null }} {{
  const m = ctx?.sessionManager;
  return {{
    session_id: typeof m?.getSessionId === "function" ? m.getSessionId() ?? "" : "",
    transcript_path: typeof m?.getSessionFile === "function" ? m.getSessionFile() ?? null : null,
  }};
}}

export default function (pi: any) {{
  pi.on("tool_call", async (e: any, ctx: any) => {{
    const out = hook("PreToolUse", {{ ...sessionOf(ctx), tool_name: e.toolName, tool_input: e.input }});
    const d = out?.hookSpecificOutput;
    if (d && d.permissionDecision === "deny") {{
      return {{ block: true, reason: d.permissionDecisionReason || "refused" }};
    }}
  }});
  pi.on("session_shutdown", async (_e: any, ctx: any) => {{
    hook("SessionEnd", {{ ...sessionOf(ctx), reason: "exit" }});
  }});
}}
"#
    );
    write(&agent.join("extensions/memory.ts"), &extension)?;
    // the egress injects the real key; this one only has to exist
    env.insert("OPENAI_API_KEY".into(), "egress".into());
    env.insert("PI_SKIP_VERSION_CHECK".into(), "1".into());
    env.insert("PI_OFFLINE".into(), "1".into());
    Ok(())
}

fn setup_copilot(
    req: &Request<'_>,
    at: &Layout,
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
    // hooks in Copilot's own file under COPILOT_HOME, with the PascalCase event names
    // so the payload is Claude's, hook_event_name, tool_name and tool_input in Claude's
    // spellings, and PreCompact's carries the transcript's path; the timeout is in
    // seconds, and a command hook that errors denies
    let hook = serde_json::json!([{
        "type": "command",
        "bash": at.hook("copilot"),
        "timeoutSec": 30
    }]);
    let hooks = serde_json::json!({
        "version": 1,
        "hooks": { "PreToolUse": hook, "PreCompact": hook, "SessionEnd": hook }
    });
    write(
        &req.home.join(".copilot/hooks/memory.json"),
        &serde_json::to_string_pretty(&hooks).unwrap_or_default(),
    )?;
    env.insert("COPILOT_HOME".into(), at.under_home(".copilot"));
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
    at: &Layout,
    relay: &str,
    env: &mut BTreeMap<String, String>,
) -> Result<(), Error> {
    copy_own_login(req)?;
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
    env.insert("XDG_CONFIG_HOME".into(), at.under_home(".config"));
    env.insert("XDG_DATA_HOME".into(), at.under_home(".local/share"));
    Ok(())
}

/// OpenCode's hooks are a plugin, a module OpenCode loads from its plugin directory,
/// so the launcher writes one that hands the three events to the memory's hook as the
/// other harnesses' hook files do: a tool call before it runs, with the decision's
/// reason thrown to refuse it; compaction before it starts; and the end of a turn,
/// which is OpenCode's nearest thing to a session's end. The transcript is kept from
/// the message events as they stream and written when the turn ends, one turn a line
/// in the shape the tail reader knows, with no call to OpenCode's server on that path:
/// a headless run exits on idle and would not wait for one.
const OPENCODE_PLUGIN: &str = r#"// written by amem launch; edits are overwritten
import { spawnSync } from "node:child_process";
import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const MEMORY = "__MEMORY__";

// one event to the memory's hook: JSON in, a decision or nothing out; a hook that
// cannot run is a refusal, as a server that does not answer is
function hook(payload) {
  const run = spawnSync(MEMORY, ["hook", "opencode"], {
    input: JSON.stringify(payload),
    encoding: "utf8",
    env: process.env,
    timeout: 30000,
  });
  if (run.error) return { decision: "deny", reason: `the memory's hook could not run: ${run.error.message}` };
  const out = (run.stdout || "").trim();
  if (!out) return null;
  try { return JSON.parse(out); } catch { return null; }
}

// what each session has said so far, from the events as they stream: a message's role
// from message.updated, each part's text, or the tool it used, from message.part.updated,
// which carries the whole part each time
const sessions = new Map();
function session(id) {
  let s = sessions.get(id);
  if (!s) { s = { order: [], messages: new Map() }; sessions.set(id, s); }
  return s;
}
function message(sessionID, messageID) {
  const s = session(sessionID);
  let m = s.messages.get(messageID);
  if (!m) { m = { role: null, parts: new Map() }; s.messages.set(messageID, m); s.order.push(messageID); }
  return m;
}
function remember(event) {
  if (event.type === "message.updated") {
    const info = event.properties.info;
    message(info.sessionID, info.id).role = info.role;
  } else if (event.type === "message.part.updated") {
    const p = event.properties.part;
    const m = message(p.sessionID, p.messageID);
    if (p.type === "text") m.parts.set(p.id, p.text || "");
    else if (p.type === "tool") m.parts.set(p.id, `[used tool ${p.tool}]`);
  }
}

// a session id as a file name: OpenCode's are letters, digits and underscores, and
// anything else becomes one rather than a path
function fileName(sessionID) {
  return String(sessionID).replace(/[^A-Za-z0-9_-]/g, "_");
}

// the session's turns, one JSON line each, in the shape the hook's tail reader knows
function transcript(sessionID) {
  const dir = join(process.env.HOME || "/home/user", ".local", "share", "opencode", "transcripts");
  mkdirSync(dir, { recursive: true });
  const file = join(dir, `${fileName(sessionID)}.jsonl`);
  const s = session(sessionID);
  const lines = s.order.map((id) => {
    const m = s.messages.get(id);
    const content = [...m.parts.values()].filter(Boolean).join("\n");
    return JSON.stringify({ type: m.role || "assistant", message: { content } });
  });
  writeFileSync(file, lines.length ? lines.join("\n") + "\n" : "");
  return file;
}

function note(event, sessionID, extra) {
  try {
    hook({ hook_event_name: event, session_id: sessionID, transcript_path: transcript(sessionID), ...extra });
  } catch (err) {
    console.warn(`memory: ${event} note not left: ${err.message}`);
  }
}

export const MemoryPlugin = async () => ({
  "tool.execute.before": async (input, output) => {
    const d = hook({ hook_event_name: "PreToolUse", session_id: input.sessionID, tool_name: input.tool, tool_input: output.args });
    if (d && d.decision === "deny") throw new Error(d.reason || "refused by the memory's hook");
  },
  "experimental.session.compacting": async (input) => {
    note("PreCompact", input.sessionID, { trigger: "auto" });
  },
  event: async ({ event }) => {
    remember(event);
    if (event.type === "session.idle") note("SessionEnd", event.properties.sessionID, { reason: "idle" });
  },
});
"#;

fn setup_opencode(
    req: &Request<'_>,
    at: &Layout,
    relay: &str,
    provider: &str,
    env: &mut BTreeMap<String, String>,
) -> Result<(), Error> {
    write(
        &req.home.join(".config/opencode/plugins/memory.js"),
        &OPENCODE_PLUGIN.replace("\"__MEMORY__\"", &at.memory_literal()),
    )?;
    if req.auth == AuthMode::Subscription && provider == "github-copilot" {
        return setup_opencode_copilot(req, at, relay, env);
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
                },
                // a model OpenCode's catalog does not know, such as a gateway's, has
                // to be declared, since models.dev is out of reach from here
                "models": req.model.map_or_else(
                    || serde_json::json!({}),
                    |m| serde_json::json!({ m: { "name": m } })
                )
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
    env.insert("XDG_CONFIG_HOME".into(), at.under_home(".config"));
    env.insert("XDG_DATA_HOME".into(), at.under_home(".local/share"));
    env.insert("XDG_CACHE_HOME".into(), at.under_home(".cache"));
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

/// Run a plan under the backend its layout is for, returning the harness's exit code
/// and what the confined process tree cost the host.
///
/// # Errors
///
/// [`Error::NoBackend`] for a namespaces plan off Linux or a Seatbelt plan off macOS,
/// or a confinement failure.
pub fn run(plan: &Plan) -> Result<(i32, Resources), Error> {
    match plan.layout.backend {
        Backend::Namespaces => {
            #[cfg(target_os = "linux")]
            {
                linux::run(plan)
            }
            #[cfg(not(target_os = "linux"))]
            {
                Err(Error::NoBackend)
            }
        }
        Backend::Seatbelt => {
            #[cfg(target_os = "macos")]
            {
                seatbelt::run(plan)
            }
            #[cfg(not(target_os = "macos"))]
            {
                Err(Error::NoBackend)
            }
        }
    }
}

/// Run a plan with the harness's standard output captured, for a non-interactive run
/// whose answer the launcher files. Standard error still reaches the terminal.
///
/// # Errors
///
/// As [`run`].
pub fn run_captured(plan: &Plan) -> Result<(i32, Vec<u8>, Resources), Error> {
    match plan.layout.backend {
        Backend::Namespaces => {
            #[cfg(target_os = "linux")]
            {
                linux::run_captured(plan)
            }
            #[cfg(not(target_os = "linux"))]
            {
                Err(Error::NoBackend)
            }
        }
        Backend::Seatbelt => {
            #[cfg(target_os = "macos")]
            {
                seatbelt::run_captured(plan)
            }
            #[cfg(not(target_os = "macos"))]
            {
                Err(Error::NoBackend)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_harness_is_in_the_list() {
        // a harness added to the enum and not here stops this compiling
        let listed = |h: Harness| match h {
            Harness::Claude
            | Harness::Codex
            | Harness::Gemini
            | Harness::Antigravity
            | Harness::Qwen
            | Harness::Grok
            | Harness::Muse
            | Harness::Copilot
            | Harness::OpenCode
            | Harness::Pi
            | Harness::Command => Harness::ALL.contains(&h),
        };
        for h in Harness::ALL {
            assert!(listed(h));
        }
        assert_eq!(Harness::ALL.len(), 11);
        assert!(Harness::ALL.contains(&Harness::Pi));
    }

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
            "claude", "codex", "gemini", "agy", "qwen", "grok", "muse", "pi", "copilot", "opencode",
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
                None,
            )
        }

        fn plan_model(
            &self,
            harness: Harness,
            auth: AuthMode,
            home: &str,
            model: &str,
        ) -> Result<(PathBuf, Plan), Error> {
            self.plan_full(
                harness,
                auth,
                None,
                home,
                &[],
                false,
                &[],
                None,
                Some(model),
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
            model: Option<&str>,
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
                model,
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
            model: None,
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
        assert_eq!(from_rootfs("/opt"), Some(true), "the image's /opt");
        // and over its copy of the binary, the launcher's own, after the rootfs mounts
        let opt = plan
            .mounts
            .iter()
            .position(|m| m.target == Path::new("/opt"))
            .unwrap();
        let own = plan
            .mounts
            .iter()
            .position(|m| m.target == Path::new(INSIDE_MEMORY))
            .unwrap();
        assert!(own > opt, "{:?}", plan.mounts);
        assert_eq!(plan.mounts[own].source, f.memory_bin);
        assert!(plan.mounts[own].read_only);
        assert_eq!(from_rootfs("/var"), Some(true));
        assert!(
            !plan.mounts.iter().any(|m| m.source == Path::new("/usr")),
            "nothing of the host's system"
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
    #[cfg(unix)] // rootfs mode is the Linux backend's
    fn an_unpacked_image_mounts_the_projection_it_names() {
        let f = fixture();
        let rootfs = f.root.join("rootfs");
        std::fs::create_dir_all(rootfs.join("usr/local/bin")).unwrap();
        let rendered = f.root.join(PROJECTION).join("gemini");
        std::fs::create_dir_all(&rendered).unwrap();
        let home = f.root.join("img-home");
        let plan = prepare(&Request {
            harness: Harness::Gemini,
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
            model: None,
            image: false,
            workdir_inside: None,
            rootfs: Some(&rootfs),
            projection: Some(&rendered),
        })
        .unwrap();
        // the environment and Gemini's workspace both name it, so it has to be there:
        // Gemini CLI exits on an include directory that does not exist
        assert_eq!(plan.env["MEMORY_PROJECTION"], INSIDE_PROJECTION);
        let settings: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(home.join(".gemini/settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            settings["context"]["includeDirectories"],
            serde_json::json!([INSIDE_PROJECTION])
        );
        let m = plan
            .mounts
            .iter()
            .find(|m| m.target == Path::new(INSIDE_PROJECTION))
            .expect("the projection the plan names is mounted");
        assert_eq!(m.source, rendered);
        assert!(
            m.read_only,
            "read-only: the files cannot be edited into lies"
        );
    }

    #[test]
    fn a_working_tree_under_the_sandbox_s_own_mounts_is_refused() {
        let f = fixture();
        let args = vec!["true".to_string()];
        let plan_at = |workdir: &str| {
            prepare(&Request {
                harness: Harness::Command,
                auth: AuthMode::Key,
                provider: None,
                workdir: Path::new(workdir),
                home: &f.root.join("h"),
                egress_socket: &f.sock,
                memory_binary: &f.memory_bin,
                destination_ceiling: Label::Public,
                harness_args: &args,
                host_home: None,
                harness_env: &[],
                model: None,
                image: false,
                workdir_inside: None,
                rootfs: None,
                projection: None,
            })
        };
        for under in [
            "/memory",
            "/memory/w",
            "/opt/memory/w",
            "/run/memory/w",
            "/home/user",
            "/home",
            "/opt",
            "/run",
            "/",
        ] {
            assert!(
                matches!(plan_at(under), Err(Error::Unsupported(ref m)) if m.contains("sandbox's own")),
                "{under} and the sandbox's own mounts overlap"
            );
        }
        // the home is mounted before the tree, so a tree under it stays; and a sibling
        // whose name only starts the same is not under it
        for fine in [
            "/home/user/proj",
            "/home/someone",
            "/memoryless/w",
            "/opt/memory-tools",
        ] {
            assert!(
                !matches!(plan_at(fine), Err(Error::Unsupported(ref m)) if m.contains("sandbox's own")),
                "{fine}"
            );
        }
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
                None,
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
        assert_eq!(settings["showThinkingSummaries"], true);
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
    fn claude_headless_asks_for_its_thinking() {
        let headless = Harness::Claude.headless("hi");
        assert!(
            headless
                .windows(2)
                .any(|w| w == ["--thinking-display", "summarized"]),
            "headless, Claude Code omits its thinking unless asked"
        );
    }

    #[test]
    fn codex_keeps_the_reasoning_an_open_model_returns() {
        let f = fixture();
        let (home, _plan) = f
            .plan(Harness::Codex, AuthMode::Gateway, "c1", &[], false)
            .unwrap();
        let cfg = std::fs::read_to_string(home.join(".codex/config.toml")).unwrap();
        assert!(cfg.contains("show_raw_agent_reasoning = true"), "{cfg}");
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
        // Gemini CLI's name for the event before a tool runs; it waits ten seconds on
        // the memory server, so its own timeout must be longer, or a stalled server
        // is a read let through, since Gemini continues when a hook is killed
        assert_eq!(
            s["hooks"]["BeforeTool"][0]["hooks"][0]["command"],
            "/opt/memory/memory hook gemini"
        );
        assert_eq!(s["hooks"]["BeforeTool"][0]["hooks"][0]["timeout"], 30_000);
        assert!(
            s.get("context").is_none(),
            "no projection, no directory to include"
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

    /// Hooks in Copilot's own file under COPILOT_HOME, with the PascalCase events so the
    /// payload is Claude's; a command hook that errors denies the call, and the timeout
    /// is in seconds.
    fn assert_copilot_hooks(home: &Path) {
        let hooks = json_file(&home.join(".copilot/hooks/memory.json"));
        assert_eq!(hooks["version"], 1);
        for event in ["PreToolUse", "PreCompact", "SessionEnd"] {
            let h = &hooks["hooks"][event][0];
            assert_eq!(h["type"], "command", "{event}");
            assert_eq!(h["bash"], "/opt/memory/memory hook copilot", "{event}");
            assert_eq!(h["timeoutSec"], 30, "{event}");
        }
    }

    /// OpenCode has no hook files: a plugin in the global plugin directory under the
    /// private home runs the memory's hook for the three events, and parses as a module
    /// where a node is on the PATH to say so.
    fn assert_opencode_plugin(home: &Path) {
        let path = home.join(".config/opencode/plugins/memory.js");
        let plugin = std::fs::read_to_string(&path).unwrap();
        for needle in [
            "\"/opt/memory/memory\"",
            "\"opencode\"",
            "\"tool.execute.before\"",
            "\"experimental.session.compacting\"",
            "\"session.idle\"",
            "PreToolUse",
            "PreCompact",
            "SessionEnd",
        ] {
            assert!(plugin.contains(needle), "{needle} in\n{plugin}");
        }
        // driving it takes a fake memory binary, which is a shell script
        #[cfg(unix)]
        if let Ok(node) = find_on_path("node") {
            drive_opencode_plugin(&node, &plugin);
        }
    }

    /// The plugin under a node, against a fake memory binary that records what it is
    /// sent and refuses a tool call: a denial throws its reason; the turns arrive as
    /// events and the idle event writes the transcript and leaves the session-end note
    /// without awaiting anything, since OpenCode's run exits on idle; a session id is a
    /// file name, not a path.
    #[cfg(unix)]
    fn drive_opencode_plugin(node: &Path, plugin: &str) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("memory");
        let record = dir.path().join("record");
        std::fs::write(
            &fake,
            format!(
                "#!/bin/sh\nin=$(cat)\nprintf '%s\\n---\\n' \"$in\" >> {}\ncase \"$in\" in *PreToolUse*) printf '%s' '{{\"decision\":\"deny\",\"reason\":\"no: projection\"}}';; esac\n",
                record.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let module = dir.path().join("memory.mjs");
        std::fs::write(
            &module,
            plugin.replace(INSIDE_MEMORY, &fake.display().to_string()),
        )
        .unwrap();
        let harness = dir.path().join("drive.mjs");
        std::fs::write(&harness, OPENCODE_HARNESS).unwrap();
        let out = std::process::Command::new(node)
            .arg(&harness)
            .arg(&module)
            .env("HOME", dir.path())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let transcript = std::fs::read_to_string(
            dir.path()
                .join(".local/share/opencode/transcripts/ses_abc.jsonl"),
        )
        .unwrap();
        assert_eq!(
            transcript,
            "{\"type\":\"user\",\"message\":{\"content\":\"Load T0.\"}}\n\
             {\"type\":\"assistant\",\"message\":{\"content\":\"Loading.\\n[used tool mcp__memory__t0]\"}}\n"
        );
        let sent = std::fs::read_to_string(&record).unwrap();
        assert!(
            sent.contains("\"hook_event_name\":\"PreToolUse\"")
                && sent.contains("\"tool_input\":{\"filePath\":\"/memory/x\"}"),
            "{sent}"
        );
        assert!(
            sent.contains("\"hook_event_name\":\"SessionEnd\"")
                && sent.contains("\"session_id\":\"ses_abc\"")
                && sent.contains("transcripts/ses_abc.jsonl"),
            "{sent}"
        );
        assert!(
            sent.contains("\"hook_event_name\":\"PreCompact\""),
            "{sent}"
        );
        assert!(
            !dir.path().join(".local/share/evil.jsonl").exists()
                && !dir.path().join(".local/share/opencode/evil.jsonl").exists(),
            "a session id walked out of the transcripts directory"
        );
    }

    /// Drives the plugin: a denied tool call, the events of one turn, a compaction and
    /// an idle, with a client whose messages call must never be awaited.
    #[cfg(unix)]
    const OPENCODE_HARNESS: &str = r#"
const [plugin] = process.argv.slice(2);
const mod = await import(plugin);
const client = { session: { messages: async () => { throw new Error("awaited the client on the idle path"); } } };
const hooks = await mod.MemoryPlugin({ client });
let threw = null;
try {
  await hooks["tool.execute.before"]({ tool: "read", sessionID: "ses_abc", callID: "c1" }, { args: { filePath: "/memory/x" } });
} catch (e) { threw = e.message; }
if (threw !== "no: projection") { console.error(`deny not thrown: ${threw}`); process.exit(1); }
const ev = (type, properties) => hooks.event({ event: { type, properties } });
await ev("message.updated", { info: { id: "m1", sessionID: "ses_abc", role: "user" } });
await ev("message.part.updated", { part: { id: "p1", messageID: "m1", sessionID: "ses_abc", type: "text", text: "Load" } });
await ev("message.part.updated", { part: { id: "p1", messageID: "m1", sessionID: "ses_abc", type: "text", text: "Load T0." } });
await ev("message.updated", { info: { id: "m2", sessionID: "ses_abc", role: "assistant" } });
await ev("message.part.updated", { part: { id: "p2", messageID: "m2", sessionID: "ses_abc", type: "text", text: "Loading." } });
await ev("message.part.updated", { part: { id: "p3", messageID: "m2", sessionID: "ses_abc", type: "tool", tool: "mcp__memory__t0" } });
await hooks["experimental.session.compacting"]({ sessionID: "ses_abc" }, { context: [] });
await ev("session.idle", { sessionID: "ses_abc" });
await ev("session.idle", { sessionID: "../../evil" });
"#;

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
        assert_copilot_hooks(&home);
        assert_eq!(plan.env["MEMORY_MCP_URL"], "http://127.0.0.1:4100/mcp");
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
        assert_opencode_plugin(&home);
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
            model: None,
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

    #[test]
    fn gemini_and_qwen_are_told_the_projection_is_theirs_to_read() {
        // both refuse their file tools a path outside the workspace, so the model would
        // fall back to a shell the hook does not see; the projection is included
        let f = fixture();
        for (harness, settings) in [
            (Harness::Gemini, ".gemini/settings.json"),
            (Harness::Qwen, ".qwen/settings.json"),
        ] {
            let rendered = f.root.join(PROJECTION).join(harness.binary());
            std::fs::create_dir_all(&rendered).unwrap();
            let (home, plan) = f
                .plan_full(
                    harness,
                    AuthMode::Key,
                    None,
                    harness.binary(),
                    &[],
                    false,
                    &[],
                    Some(&rendered),
                    None,
                )
                .unwrap();
            assert_eq!(plan.env["MEMORY_PROJECTION"], INSIDE_PROJECTION);
            let s = json_file(&home.join(settings));
            assert_eq!(
                s["context"]["includeDirectories"],
                serde_json::json!([INSIDE_PROJECTION]),
                "{}",
                harness.binary()
            );
        }
    }

    #[test]
    fn muse_plan_key_points_exec_at_the_egress_after_the_prompt() {
        let f = fixture();
        let args = Harness::Muse.headless("hi");
        let (home, plan) = f
            .plan(Harness::Muse, AuthMode::Key, "k1", &args, false)
            .unwrap();
        assert_eq!(plan.env["META_API_KEY"], "egress");
        assert_eq!(plan.env["MUSE_NO_AUTO_UPDATE"], "1");
        assert!(
            !plan.env.contains_key("HTTPS_PROXY"),
            "a key needs no tunnel"
        );
        // only exec takes --base-url, so it follows the prompt, where exec accepts it
        let argv = &plan.argv;
        let exec = argv.iter().position(|a| a == "exec").unwrap();
        let base = argv.iter().position(|a| a == "--base-url").unwrap();
        assert!(exec < base, "{argv:?}");
        assert_eq!(argv[base + 1], "http://127.0.0.1:4100/meta/v1");
        assert!(argv[exec..base].iter().any(|a| a == "hi"), "{argv:?}");
        assert!(
            !argv.iter().any(|a| a == "--reasoning-effort"),
            "Meta's own models take Muse's default effort"
        );
        let (_, gw) = f
            .plan_with(
                Harness::Muse,
                AuthMode::Gateway,
                Some("llmao"),
                "k1g",
                &args,
                false,
                &[],
            )
            .unwrap();
        let at = gw
            .argv
            .iter()
            .position(|a| a == "--reasoning-effort")
            .unwrap();
        assert_eq!(gw.argv[at + 1], "medium");
        assert_eq!(
            Harness::Muse.headless("hi"),
            vec!["exec", "--yolo", "--json", "--disable-reminders", "hi"]
        );
        assert_eq!(Harness::Muse.model_flag("m"), vec!["--model", "m"]);
        assert_eq!(Harness::Muse.provider(), "meta");
        assert_eq!(Harness::Muse.subscription_provider(), "meta");
        assert!(!Harness::Muse.accepts_provider());
        drop(home);
    }

    #[test]
    fn muse_settings_carry_the_schema_version_the_mcp_server_and_the_hooks() {
        let f = fixture();
        let (home, _) = f
            .plan(Harness::Muse, AuthMode::Key, "k1", &[], false)
            .unwrap();
        let settings: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(home.join(".config/muse/settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(settings["schema_version"], 1);
        assert_eq!(
            settings["mcp_servers"]["memory"]["transport"],
            "streamable_http"
        );
        assert_eq!(
            settings["mcp_servers"]["memory"]["url"],
            "http://127.0.0.1:4100/mcp"
        );
        for e in ["PreCompact", "SessionEnd", "PreToolUse"] {
            assert_eq!(
                settings["hooks"][e][0]["hooks"][0]["command"], "/opt/memory/memory hook muse",
                "{e}"
            );
        }
    }

    #[test]
    fn muse_plan_login_is_the_auth_file_forwarded() {
        let f = fixture();
        assert!(matches!(
            f.plan(Harness::Muse, AuthMode::Subscription, "k2", &[], false),
            Err(Error::Unsupported(_))
        ));
        std::fs::create_dir_all(f.host_home.join(".config/muse")).unwrap();
        std::fs::write(
            f.host_home.join(".config/muse/auth.json"),
            r#"{"session":"x"}"#,
        )
        .unwrap();
        let (home, plan) = f
            .plan(Harness::Muse, AuthMode::Subscription, "k3", &[], true)
            .unwrap();
        assert!(
            home.join(".config/muse/auth.json").is_file(),
            "login copied in"
        );
        assert!(
            !plan.env.contains_key("META_API_KEY"),
            "a key would outrank the login"
        );
        assert!(plan
            .argv
            .iter()
            .any(|a| a == "http://127.0.0.1:4100/meta/v1"));
    }

    #[test]
    fn pi_refuses_a_run_without_a_model_and_a_login() {
        let f = fixture();
        // no model, no provider list to pick from
        assert!(matches!(
            f.plan(Harness::Pi, AuthMode::Key, "p0", &[], false),
            Err(Error::Unsupported(_))
        ));
        assert!(matches!(
            f.plan(Harness::Pi, AuthMode::Subscription, "p0", &[], false),
            Err(Error::Unsupported(_))
        ));
        assert_eq!(Harness::Pi.headless("hi"), vec!["-p", "-na", "hi"]);
        assert_eq!(Harness::Pi.model_flag("m"), vec!["--model", "m"]);
        assert_eq!(Harness::Pi.provider(), "openai");
        assert!(Harness::Pi.accepts_provider());
    }

    #[test]
    fn pi_plan_declares_the_destination_as_a_provider_with_the_model_named() {
        let f = fixture();
        let (home, plan) = f
            .plan_model(Harness::Pi, AuthMode::Key, "p1", "gpt-5-mini")
            .unwrap();
        let models = json_file(&home.join(".pi/agent/models.json"));
        assert_eq!(
            models["providers"]["openai"]["baseUrl"],
            "http://127.0.0.1:4100/openai/v1"
        );
        assert_eq!(models["providers"]["openai"]["api"], "openai-completions");
        assert_eq!(
            models["providers"]["openai"]["models"][0]["id"],
            "gpt-5-mini"
        );
        let mcp = json_file(&home.join(".pi/agent/mcp.json"));
        assert_eq!(
            mcp["mcpServers"]["memory"]["url"],
            "http://127.0.0.1:4100/mcp"
        );
        let ext = std::fs::read_to_string(home.join(".pi/agent/extensions/memory.ts")).unwrap();
        assert!(
            ext.contains("\"/opt/memory/memory\", [\"hook\", \"pi\"]"),
            "{ext}"
        );
        assert!(ext.contains("tool_call") && ext.contains("session_shutdown"));
        assert_eq!(plan.env["OPENAI_API_KEY"], "egress");
        assert_eq!(plan.env["PI_OFFLINE"], "1");
    }

    #[test]
    fn grok_plan_key_points_the_api_at_the_egress() {
        let f = fixture();
        let (home, plan) = f
            .plan(Harness::Grok, AuthMode::Key, "k1", &[], false)
            .unwrap();
        // only API-key inference reads this base; the egress holds the real key
        assert_eq!(
            plan.env["GROK_XAI_API_BASE_URL"],
            "http://127.0.0.1:4100/xai/v1"
        );
        assert_eq!(plan.env["XAI_API_KEY"], "egress");
        assert!(
            !plan.env.contains_key("HTTPS_PROXY"),
            "a key needs no tunnel"
        );
        let cfg = std::fs::read_to_string(home.join(".grok/config.toml")).unwrap();
        for line in [
            "[mcp_servers.memory]",
            "url = \"http://127.0.0.1:4100/mcp\"",
            "auto_update = false",
            "telemetry = false",
            "[[hooks.PreToolUse]]",
            "[[hooks.SessionEnd]]",
            "[[hooks.PreCompact]]",
        ] {
            assert!(cfg.contains(line), "{line} in\n{cfg}");
        }
        assert_eq!(
            cfg.matches("command = \"/opt/memory/memory hook grok\", timeout = 30")
                .count(),
            3,
            "{cfg}"
        );
        assert!(
            !cfg.contains("matcher"),
            "no matcher: an absent one selects every tool"
        );
        assert_eq!(plan.env["MEMORY_MCP_URL"], "http://127.0.0.1:4100/mcp");
        assert!(
            plan.argv.iter().any(|a| a.ends_with("grok")),
            "{:?}",
            plan.argv
        );
        assert_eq!(Harness::Grok.headless("hi"), vec!["-p", "hi", "--yolo"]);
        assert_eq!(Harness::Grok.model_flag("m"), vec!["-m", "m"]);
        assert_eq!(Harness::Grok.provider(), "xai");
        assert_eq!(Harness::Grok.subscription_provider(), "grok");
        assert!(!Harness::Grok.accepts_provider());
    }

    #[test]
    fn grok_plan_declares_a_model_of_the_destination_s_own() {
        let f = fixture();
        let (home, _) = f
            .plan_model(Harness::Grok, AuthMode::Key, "k4", "qwen3.8-27b")
            .unwrap();
        let cfg = std::fs::read_to_string(home.join(".grok/config.toml")).unwrap();
        for line in [
            "[model.\"qwen3.8-27b\"]",
            "model = \"qwen3.8-27b\"",
            "base_url = \"http://127.0.0.1:4100/xai/v1\"",
            "api_backend = \"chat_completions\"",
        ] {
            assert!(cfg.contains(line), "{line} in\n{cfg}");
        }
    }

    #[test]
    fn grok_plan_login_is_the_auth_file_behind_a_tunnel() {
        let f = fixture();
        // a login is the token file `grok login` left; without it, nothing to copy
        assert!(matches!(
            f.plan(Harness::Grok, AuthMode::Subscription, "k2", &[], false),
            Err(Error::Unsupported(_))
        ));
        std::fs::create_dir_all(f.host_home.join(".grok")).unwrap();
        std::fs::write(
            f.host_home.join(".grok/auth.json"),
            r#"{"access_token":"x"}"#,
        )
        .unwrap();
        let (home, plan) = f
            .plan(Harness::Grok, AuthMode::Subscription, "k3", &[], true)
            .unwrap();
        assert!(home.join(".grok/auth.json").is_file(), "login copied in");
        assert_eq!(plan.env["HTTPS_PROXY"], "http://127.0.0.1:4100");
        assert!(!plan.env.contains_key("XAI_API_KEY"));
        assert!(!plan.env.contains_key("GROK_XAI_API_BASE_URL"));
        let cfg = std::fs::read_to_string(home.join(".grok/config.toml")).unwrap();
        assert!(cfg.contains("[mcp_servers.memory]"), "{cfg}");
        // a stored session token outranks XAI_API_KEY and goes to Grok's own proxy,
        // not the key's base, so a key run into the same persisted home removes it
        let (home, plan) = f
            .plan(Harness::Grok, AuthMode::Key, "k3", &[], true)
            .unwrap();
        assert_eq!(plan.env["XAI_API_KEY"], "egress");
        assert!(
            !home.join(".grok/auth.json").exists(),
            "a login left behind would carry the key run past the egress"
        );
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
        assert_eq!(Harness::Qwen.subscription_provider(), "dashscope");
        assert!(Harness::Qwen.accepts_provider());
    }

    #[test]
    fn qwen_has_no_login_mode_since_its_free_tier_ended() {
        let f = fixture();
        // Qwen OAuth was discontinued in April 2026 and the harness exits on it; a
        // cached credential on the host changes nothing, and the refusal says what to
        // use instead
        std::fs::create_dir_all(f.host_home.join(".qwen")).unwrap();
        std::fs::write(
            f.host_home.join(".qwen/oauth_creds.json"),
            r#"{"access_token":"x"}"#,
        )
        .unwrap();
        for creds in [false, true] {
            match f.plan(Harness::Qwen, AuthMode::Subscription, "q3", &[], creds) {
                Err(Error::Unsupported(why)) => assert!(
                    why.contains("--auth key") && why.contains("DASHSCOPE_API_KEY"),
                    "{why}"
                ),
                other => panic!("a login mode: {other:?}"),
            }
        }
        assert!(
            !f.root.join("q3/.qwen/oauth_creds.json").exists(),
            "nothing copied in"
        );
    }

    /// A plan for Seatbelt's layout, from the fixture: everything at its host path. Unix
    /// only, as the profile's paths are: a Windows path is never one Seatbelt is given.
    #[cfg(unix)]
    fn seatbelt_plan(
        f: &Fixture,
        harness: Harness,
        home: &str,
        projection: Option<&Path>,
        model: Option<&str>,
    ) -> Result<(PathBuf, Plan), Error> {
        let home = f.root.join(home);
        let _path = path_lock();
        let old = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![f.bins.clone()];
        paths.extend(std::env::split_paths(&old));
        std::env::set_var("PATH", std::env::join_paths(paths).unwrap());
        let req = Request {
            harness,
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
            model,
            image: false,
            workdir_inside: None,
            rootfs: None,
            projection,
        };
        let r = prepare_in(&req, Layout::seatbelt(&req, "127.0.0.1:51234"));
        std::env::set_var("PATH", old);
        r.map(|p| (home, p))
    }

    /// Every file under `dir`, with its text.
    #[cfg(unix)]
    fn files_under(dir: &Path) -> Vec<(PathBuf, String)> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(files_under(&p));
            } else {
                out.push((p.clone(), std::fs::read_to_string(&p).unwrap_or_default()));
            }
        }
        out
    }

    #[test]
    fn the_namespaces_layout_is_the_inside_constants() {
        let at = Layout::namespaces();
        assert_eq!(at.backend, Backend::Namespaces);
        assert_eq!(at.home, Path::new(INSIDE_HOME));
        assert_eq!(at.socket, Path::new(INSIDE_SOCKET));
        assert_eq!(at.memory, Path::new(INSIDE_MEMORY));
        assert_eq!(at.relay, INSIDE_RELAY);
        assert_eq!(at.projection.as_deref(), Some(Path::new(INSIDE_PROJECTION)));
        assert_eq!(at.tmp, Path::new("/tmp"));
        assert_eq!(at.hook("claude"), format!("{INSIDE_MEMORY} hook claude"));
        assert_eq!(at.memory_literal(), format!("\"{INSIDE_MEMORY}\""));
    }

    #[test]
    fn a_hook_command_quotes_a_path_a_shell_would_split() {
        assert_eq!(
            shell_word(Path::new("/opt/memory/memory")),
            "/opt/memory/memory"
        );
        assert_eq!(
            shell_word(Path::new("/Users/me/My Tools/amem")),
            "'/Users/me/My Tools/amem'"
        );
        assert_eq!(shell_word(Path::new("/a/it's")), r"'/a/it'\''s'");
        assert_eq!(toml_escaped(r#"'/a/"b"\c' hook"#), r#"'/a/\"b\"\\c' hook"#);
    }

    #[test]
    #[cfg(unix)]
    fn under_seatbelt_the_harness_sees_its_host_paths() {
        let f = fixture();
        let projection = f.root.join("projection");
        std::fs::create_dir_all(&projection).unwrap();
        let (home, plan) =
            seatbelt_plan(&f, Harness::Claude, "sb", Some(&projection), None).unwrap();
        let shown = |p: &Path| p.display().to_string();
        assert_eq!(plan.layout.backend, Backend::Seatbelt);
        assert_eq!(plan.env["HOME"], shown(&home));
        assert_eq!(plan.env["MEMORY_OUTBOX"], shown(&home.join(OUTBOX)));
        assert_eq!(plan.env["TMPDIR"], shown(&home.join(".tmp")));
        assert!(home.join(".tmp").is_dir());
        assert_eq!(plan.env["MEMORY_PROJECTION"], shown(&projection));
        assert!(plan.env["PATH"].contains(&shown(f.memory_bin.parent().unwrap())));
        assert_eq!(plan.argv[0], shown(&f.memory_bin));
        assert_eq!(plan.argv[3], shown(&f.sock));
        assert_eq!(plan.argv[5], "127.0.0.1:51234");
        assert!(plan.argv.contains(&shown(&home.join(".claude-mcp.json"))));
        assert_eq!(plan.cwd, f.work);
        let settings = json_file(&home.join(".claude/settings.json"));
        assert_eq!(
            settings["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            format!("{} hook claude", f.memory_bin.display())
        );
    }

    #[test]
    #[cfg(unix)]
    fn under_seatbelt_the_mounts_are_the_profile_grants() {
        let f = fixture();
        let projection = f.root.join("projection");
        std::fs::create_dir_all(&projection).unwrap();
        let (home, plan) =
            seatbelt_plan(&f, Harness::Claude, "sb", Some(&projection), None).unwrap();
        let shown = |p: &Path| p.display().to_string();
        // the mounts are grants at the host's paths, and the system is not among them
        assert!(plan.mounts.iter().all(|m| m.source == m.target));
        assert!(!plan.mounts.iter().any(|m| m.target.starts_with("/usr")));
        assert!(plan.links.is_empty());
        let profile = seatbelt::profile(&plan).unwrap();
        assert!(profile.contains(&format!("(subpath \"{}\")", shown(&home))));
        assert!(profile.contains(&format!("(subpath \"{}\")", shown(&projection))));
    }

    #[test]
    #[cfg(unix)]
    fn under_seatbelt_no_adapter_names_a_namespaces_path() {
        let f = fixture();
        let projection = f.root.join("projection");
        std::fs::create_dir_all(&projection).unwrap();
        for (i, h) in [
            Harness::Claude,
            Harness::Codex,
            Harness::Gemini,
            Harness::Antigravity,
            Harness::Qwen,
            Harness::Grok,
            Harness::Muse,
            Harness::Pi,
            Harness::OpenCode,
        ]
        .into_iter()
        .enumerate()
        {
            let (home, plan) =
                seatbelt_plan(&f, h, &format!("h{i}"), Some(&projection), Some("m")).unwrap();
            let memory = f.memory_bin.display().to_string();
            let mut hooked = false;
            for (p, text) in files_under(&home) {
                // OpenCode's plugin falls back on the namespaces home only where HOME
                // is unset, which no plan leaves it
                let text = text.replace(r#"process.env.HOME || "/home/user""#, "");
                for inside in [INSIDE_HOME, INSIDE_MEMORY, "/run/memory"] {
                    assert!(
                        !text.contains(inside),
                        "{h:?}: {} names {inside}",
                        p.display()
                    );
                }
                hooked |= text.contains(&memory);
            }
            assert!(hooked, "{h:?}: no file runs the memory's hook");
            for (k, v) in &plan.env {
                for inside in [INSIDE_HOME, INSIDE_MEMORY, INSIDE_SOCKET, INSIDE_RELAY] {
                    assert!(!v.contains(inside), "{h:?}: {k}={v}");
                }
            }
            assert!(!plan.argv.iter().any(|a| a.contains(INSIDE_HOME)), "{h:?}");
        }
    }

    #[test]
    fn seatbelt_refuses_the_image_and_a_tree_over_its_own() {
        let f = fixture();
        let req = |workdir: &'static str, image: bool| {
            let workdir = if workdir.is_empty() {
                f.root.clone()
            } else {
                f.work.clone()
            };
            (workdir, image)
        };
        for (workdir, image) in [req("", false), req("work", true)] {
            let home = f.root.join("sb");
            let r = Request {
                harness: Harness::Claude,
                auth: AuthMode::Key,
                provider: None,
                workdir: &workdir,
                home: &home,
                egress_socket: &f.sock,
                memory_binary: &f.memory_bin,
                destination_ceiling: Label::Public,
                harness_args: &[],
                host_home: None,
                harness_env: &[],
                model: None,
                image,
                workdir_inside: None,
                rootfs: None,
                projection: None,
            };
            match prepare_in(&r, Layout::seatbelt(&r, "127.0.0.1:51234")) {
                Err(Error::Unsupported(why)) => {
                    assert!(
                        why.contains("Seatbelt") || why.contains("working tree"),
                        "{why}"
                    );
                }
                other => panic!("{} image {image}: {other:?}", workdir.display()),
            }
        }
    }
}
