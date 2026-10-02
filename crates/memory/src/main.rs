//! The `memory` binary: the per-session memory server, the egress, the launcher, and the
//! command line.
//!
//! Every subcommand is a thin call into the library crates; the MCP server and the command
//! line see the same operations. The binary resolves its mounts at startup (`--root`, or
//! `MEMORY_ROOT`) and, for `launch`, reads the one or two credentials it needs from its own
//! environment and never passes them inside.

mod image;
mod requests;
mod work;
mod wslc;

use clap::{Parser, Subcommand, ValueEnum};
use dm_core::{EntryType, Label};
use dm_egress::{Auth, Destination, Egress, Telemetry};
use dm_ledger::{Status, TaskSpec};
use dm_sandbox::{AuthMode, Harness};
use dm_server::{mcp, Memory, WriteRequest};
use std::collections::HashMap;
use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use time::Duration;

#[derive(Parser)]
#[command(
    name = "memory",
    version,
    about = "Durable memory for disposable agents: one root, served over MCP, with a confined launcher for the harness."
)]
struct Cli {
    /// The memory root: grants.toml, record/, state/. The only path this process touches.
    #[arg(long, env = "MEMORY_ROOT", global = true)]
    root: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
enum Kind {
    Decision,
    Fact,
    Convention,
    Method,
    Handoff,
}

impl From<Kind> for EntryType {
    fn from(k: Kind) -> EntryType {
        match k {
            Kind::Decision => EntryType::Decision,
            Kind::Fact => EntryType::Fact,
            Kind::Convention => EntryType::Convention,
            Kind::Method => EntryType::Method,
            Kind::Handoff => EntryType::Handoff,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum LabelArg {
    Public,
    Internal,
    Restricted,
}

impl From<LabelArg> for Label {
    fn from(l: LabelArg) -> Label {
        match l {
            LabelArg::Public => Label::Public,
            LabelArg::Internal => Label::Internal,
            LabelArg::Restricted => Label::Restricted,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum StatusArg {
    Proposed,
    Admitted,
    Claimed,
    Completed,
    Invalidated,
}

impl From<StatusArg> for Status {
    fn from(s: StatusArg) -> Status {
        match s {
            StatusArg::Proposed => Status::Proposed,
            StatusArg::Admitted => Status::Admitted,
            StatusArg::Claimed => Status::Claimed,
            StatusArg::Completed => Status::Completed,
            StatusArg::Invalidated => Status::Invalidated,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum HarnessArg {
    Claude,
    Codex,
    Gemini,
    Copilot,
    Opencode,
}

impl From<HarnessArg> for Harness {
    fn from(h: HarnessArg) -> Harness {
        match h {
            HarnessArg::Claude => Harness::Claude,
            HarnessArg::Codex => Harness::Codex,
            HarnessArg::Gemini => Harness::Gemini,
            HarnessArg::Copilot => Harness::Copilot,
            HarnessArg::Opencode => Harness::OpenCode,
        }
    }
}

#[derive(Subcommand)]
enum ImageOp {
    /// Export the image from the runtime that holds it (wslc, podman or docker) and
    /// unpack it under the record, for the namespace backend to pivot into.
    Unpack {
        #[arg(long, env = "MEMORY_IMAGE", default_value = wslc::DEFAULT_IMAGE)]
        image: String,
    },
    /// Where the unpacked image is, if it is there.
    Path,
}

/// Where the harness runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum BackendArg {
    /// Namespaces on Linux; WSL containers on Windows.
    Auto,
    /// Native Linux namespaces, from this host's binaries.
    Namespaces,
    /// Native Linux namespaces, pivoted into the unpacked harness image (`memory image
    /// unpack` first). What `auto` picks on Linux once the image is unpacked.
    Image,
    /// WSL containers: the harness image in a session VM, through `wslc`.
    Wslc,
    /// No confinement: the egress still proxies and meters, nothing else.
    None,
}

#[derive(Clone, Copy, ValueEnum)]
enum AuthArg {
    Gateway,
    Key,
    Subscription,
}

impl From<AuthArg> for AuthMode {
    fn from(a: AuthArg) -> AuthMode {
        match a {
            AuthArg::Gateway => AuthMode::Gateway,
            AuthArg::Key => AuthMode::Key,
            AuthArg::Subscription => AuthMode::Subscription,
        }
    }
}

#[derive(Subcommand)]
enum Command {
    /// Create a root for one owner and one domain.
    Init {
        /// The owner, system-qualified. Defaults to human:local:<user>.
        #[arg(long)]
        owner: Option<String>,
        /// The first domain, such as dom-me.
        #[arg(long, default_value = "dom-me")]
        domain: String,
    },
    /// Serve the root over MCP on stdin and stdout until the client goes away.
    Serve {
        /// Attribute writes to this author instead of the owner, e.g. "agent:claude-code (model: x)".
        #[arg(long)]
        author: Option<String>,
        /// Capabilities this session carries, matched against task requirements.
        #[arg(long = "cap")]
        manifest: Vec<String>,
    },
    /// Run a harness confined: no network except the egress, the memory server attached,
    /// model calls forwarded and metered. Credentials come from this process's
    /// environment: ANTHROPIC_API_KEY or OPENAI_API_KEY for --auth key, LLMAO_KEY (and
    /// LLMAO_URL) for --auth gateway, the harness's own login for --auth subscription.
    Launch {
        #[arg(long, value_enum, default_value = "claude")]
        harness: HarnessArg,
        #[arg(long, value_enum, default_value = "subscription")]
        auth: AuthArg,
        /// Send model calls to another destination: openai, xai (Grok), or a name from
        /// grants.toml. Codex and OpenCode only.
        #[arg(long)]
        provider: Option<String>,
        /// The working tree the harness may read and write. Defaults to the current directory.
        #[arg(long)]
        workdir: Option<PathBuf>,
        /// Run without namespaces: the egress still proxies and meters, but nothing is
        /// confined. For platforms without a backend, and for debugging.
        #[arg(long)]
        unconfined: bool,
        /// Where the harness runs. `wslc` uses the harness image under WSL containers.
        #[arg(long, value_enum, default_value = "auto")]
        backend: BackendArg,
        /// The harness image for the `wslc` backend.
        #[arg(long, env = "MEMORY_IMAGE", default_value = wslc::DEFAULT_IMAGE)]
        image: String,
        /// Non-interactive: run this prompt in the harness's headless form, print its
        /// answer, and exit when it does. The run is expected to leave a handoff.
        #[arg(long, conflicts_with = "prompt_file")]
        prompt: Option<String>,
        /// As --prompt, read from a file.
        #[arg(long)]
        prompt_file: Option<PathBuf>,
        /// With --prompt: a run that leaves no handoff is fine.
        #[arg(long)]
        no_handoff: bool,
        /// Arguments passed to the harness.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Claim the next task from the ledger and run the harness on it, non-interactively.
    /// The launcher holds the lease and heartbeats while the harness works. A task the
    /// harness completed stays complete; anything else goes back to the pool with the
    /// harness's output filed as a handoff. One invocation, one task; --loop keeps going.
    Work {
        #[arg(long, value_enum, default_value = "claude")]
        harness: HarnessArg,
        #[arg(long, value_enum, default_value = "subscription")]
        auth: AuthArg,
        /// Send model calls to another destination. Codex and OpenCode only.
        #[arg(long)]
        provider: Option<String>,
        /// The working tree the harness may read and write. Defaults to the current directory.
        #[arg(long)]
        workdir: Option<PathBuf>,
        /// Run without namespaces.
        #[arg(long)]
        unconfined: bool,
        /// Where the harness runs. `wslc` uses the harness image under WSL containers.
        #[arg(long, value_enum, default_value = "auto")]
        backend: BackendArg,
        /// The harness image for the `wslc` backend.
        #[arg(long, env = "MEMORY_IMAGE", default_value = wslc::DEFAULT_IMAGE)]
        image: String,
        /// Claim from this domain; the only granted domain when omitted.
        #[arg(long)]
        domain: Option<String>,
        /// Lease length in seconds; the launcher heartbeats at a third of it.
        #[arg(long, default_value_t = 900)]
        lease_seconds: i64,
        /// Keep claiming until interrupted; wait --idle-seconds when nothing is claimable.
        #[arg(long = "loop")]
        keep_going: bool,
        /// With --loop: how long to wait when the queue is empty.
        #[arg(long, default_value_t = 30)]
        idle_seconds: u64,
    },
    /// Inside the egress container of the `wslc` backend: open the memory, serve the
    /// egress on the shared volume, and run `launch` or `work` by writing run requests
    /// the host turns into harness containers.
    #[command(hide = true)]
    Inside {
        #[arg(long, value_enum)]
        harness: HarnessArg,
        #[arg(long, value_enum)]
        auth: AuthArg,
        #[arg(long)]
        provider: Option<String>,
        /// The shared volume.
        #[arg(long)]
        requests: PathBuf,
        /// The host's login directories, read-only.
        #[arg(long)]
        host_home: Option<PathBuf>,
        /// Where the working tree appears in the harness container.
        #[arg(long, default_value = "/work")]
        workdir_inside: PathBuf,
        #[arg(long, conflicts_with = "prompt_file")]
        prompt: Option<String>,
        #[arg(long)]
        prompt_file: Option<PathBuf>,
        #[arg(long)]
        no_handoff: bool,
        /// Task mode.
        #[arg(long)]
        work: bool,
        #[arg(long)]
        domain: Option<String>,
        #[arg(long, default_value_t = 900)]
        lease_seconds: i64,
        #[arg(long = "loop")]
        keep_going: bool,
        #[arg(long, default_value_t = 30)]
        idle_seconds: u64,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Inside the sandbox: forward a loopback port to the egress socket, then run the
    /// harness in its own pid namespace.
    #[command(hide = true)]
    Relay {
        #[arg(long)]
        socket: PathBuf,
        #[arg(long)]
        listen: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        argv: Vec<OsString>,
    },
    /// Token usage recorded by the egress, by destination and model.
    Usage,
    /// The harness image on this host.
    Image {
        #[command(subcommand)]
        op: ImageOp,
    },
    /// Write an entry. The body comes from --body or standard input.
    Write {
        #[arg(long, value_enum, default_value = "fact")]
        kind: Kind,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        domain: Option<String>,
        #[arg(long)]
        body: Option<String>,
        #[arg(long, value_enum)]
        classification: Option<LabelArg>,
        #[arg(long)]
        source: Vec<String>,
        #[arg(long)]
        supersedes: Option<String>,
    },
    /// Ranked search, summaries only.
    Search {
        query: String,
        #[arg(long)]
        domain: Option<String>,
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// One entry, in full.
    Get { id: String },
    /// Entries of one kind, newest first.
    List {
        #[arg(long, value_enum)]
        kind: Option<Kind>,
        #[arg(long)]
        domain: Option<String>,
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// The T0 for a domain.
    T0 {
        #[arg(long)]
        domain: Option<String>,
    },
    /// The task ledger.
    Task {
        #[command(subcommand)]
        op: TaskOp,
    },
    /// Export a domain to a bundle directory, at or below a label.
    Export {
        dest: PathBuf,
        #[arg(long)]
        domain: Option<String>,
        #[arg(long, value_enum, default_value = "public")]
        max_label: LabelArg,
    },
    /// Import a bundle directory into a domain.
    Import {
        src: PathBuf,
        #[arg(long)]
        domain: Option<String>,
    },
    /// Rebuild the index from the record and the ledger from its events.
    Reindex,
}

#[derive(Subcommand)]
enum TaskOp {
    /// File a task. It is born proposed.
    Create {
        title: String,
        #[arg(long, default_value = "")]
        body: String,
        #[arg(long)]
        domain: Option<String>,
        #[arg(long = "require")]
        requirements: Vec<String>,
        #[arg(long = "after")]
        depends_on: Vec<String>,
        #[arg(long, default_value_t = 0)]
        priority: i64,
    },
    /// Admit a proposed task.
    Admit { id: String },
    /// Tasks with a status.
    List {
        #[arg(long, value_enum, default_value = "admitted")]
        status: StatusArg,
        #[arg(long)]
        domain: Option<String>,
    },
    /// Claim the highest-ranked claimable task.
    ClaimNext {
        #[arg(long)]
        domain: Option<String>,
        #[arg(long, default_value_t = 1800)]
        lease_seconds: i64,
    },
    /// Renew a lease.
    Heartbeat {
        id: String,
        #[arg(long, default_value_t = 1800)]
        lease_seconds: i64,
    },
    /// Complete a task with a handoff entry id.
    Complete { id: String, handoff: String },
    /// Give a task back, optionally with a handoff for the next claimant.
    Release {
        id: String,
        #[arg(long)]
        handoff: Option<String>,
    },
    /// Close a task without doing it.
    Invalidate {
        id: String,
        #[arg(long, default_value = "obsolete")]
        reason: String,
    },
    /// One task.
    Get { id: String },
}

type Fallible = Result<(), Box<dyn std::error::Error>>;

fn json<T: serde::Serialize>(v: &T) -> Fallible {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

fn hits_json(hits: &[dm_index::Hit]) -> Vec<serde_json::Value> {
    hits.iter()
        .map(|h| {
            serde_json::json!({
                "id": h.id, "domain": h.domain, "type": h.entry_type.to_string(),
                "trust_tier": h.trust_tier.to_string(),
                "classification": h.classification.to_string(),
                "title": h.title, "summary": h.summary,
            })
        })
        .collect()
}

fn default_owner() -> String {
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "me".into());
    format!("human:local:{user}")
}

fn body_or_stdin(body: Option<String>) -> std::io::Result<String> {
    match body {
        Some(b) => Ok(b),
        None => {
            let mut s = String::new();
            std::io::stdin().read_to_string(&mut s)?;
            Ok(s)
        }
    }
}

fn main() {
    // The relay unshares a pid namespace, which only a single-threaded process may do,
    // so it runs before any runtime starts.
    let args: Vec<OsString> = std::env::args_os().collect();
    if args.get(1).is_some_and(|a| a == "relay") || args.get(3).is_some_and(|a| a == "relay") {
        let cli = Cli::parse();
        if let Command::Relay {
            socket,
            listen,
            argv,
        } = cli.command
        {
            match relay(&socket, &listen, &argv) {
                Ok(code) => std::process::exit(code),
                Err(e) => {
                    eprintln!("relay: {e}");
                    std::process::exit(1);
                }
            }
        }
    }
    if let Err(e) = run_cli() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

#[tokio::main]
async fn run_cli() -> Fallible {
    let cli = Cli::parse();
    let root = cli.root.ok_or("give --root, or set MEMORY_ROOT")?;
    match cli.command {
        Command::Init { owner, domain } => {
            let m = Memory::init(&root, &owner.unwrap_or_else(default_owner), &domain)?;
            println!(
                "initialised {} for {} with domain {domain}",
                m.root().display(),
                m.author()
            );
            Ok(())
        }
        Command::Serve { author, manifest } => {
            let mut m = Memory::open(&root)?.with_manifest(manifest);
            if let Some(a) = author {
                m = m.with_author(a);
            }
            mcp::serve_stdio(Arc::new(m)).await?;
            Ok(())
        }
        cmd @ (Command::Launch { .. } | Command::Work { .. } | Command::Inside { .. }) => {
            launch_cmd(&root, cmd).await
        }
        Command::Image { op } => match op {
            ImageOp::Unpack { image } => {
                image::unpack(&root, &image)?;
                Ok(())
            }
            ImageOp::Path => match image::present(&root) {
                Some(p) => {
                    println!("{}", p.display());
                    Ok(())
                }
                None => Err("no unpacked image; run `memory image unpack`".into()),
            },
        },
        Command::Relay { .. } => unreachable!("handled in main"),
        Command::Usage => {
            let t = Telemetry::open(&root.join("state/telemetry.db"))?;
            let rows = t.summary()?;
            if rows.is_empty() {
                println!("no model calls recorded");
                return Ok(());
            }
            println!(
                "{:<12} {:<28} {:>6} {:>10} {:>10} {:>10} {:>10}",
                "destination", "model", "calls", "input", "output", "cache_rd", "cache_wr"
            );
            for r in rows {
                println!(
                    "{:<12} {:<28} {:>6} {:>10} {:>10} {:>10} {:>10}",
                    r.destination,
                    r.model,
                    r.calls,
                    r.input_tokens,
                    r.output_tokens,
                    r.cache_read_tokens,
                    r.cache_write_tokens
                );
            }
            Ok(())
        }
        Command::Task { op } => task(&Memory::open(&root)?, op).await,
        other => entries(&Memory::open(&root)?, other).await,
    }
}

/// What one launch needs for its model calls: the destination name as the harness will
/// address it, the destination itself, the grants entry whose ceiling caps the session,
/// and any credential the harness must hold itself.
struct Route {
    name: String,
    destination: Destination,
    ceiling_of: String,
    harness_env: Vec<(String, String)>,
}

fn need(var: &str, why: &str) -> Result<String, Box<dyn std::error::Error>> {
    std::env::var(var)
        .map_err(|_| format!("{why} needs {var} in the launcher's environment").into())
}

fn bearer(value: String) -> Auth {
    Auth::Inject {
        header: axum_header("authorization"),
        value: format!("Bearer {value}"),
    }
}

/// The provider-shaped destination for a key: where it goes and how the key travels.
fn keyed_provider(name: &str) -> Result<Destination, Box<dyn std::error::Error>> {
    Ok(match name {
        "anthropic" => Destination::forward(
            "https://api.anthropic.com",
            Auth::Inject {
                header: axum_header("x-api-key"),
                value: need("ANTHROPIC_API_KEY", "--auth key to anthropic")?,
            },
        ),
        "openai" => Destination::forward(
            "https://api.openai.com",
            bearer(need("OPENAI_API_KEY", "--auth key to openai")?),
        ),
        "xai" => Destination::forward(
            "https://api.x.ai",
            bearer(need("XAI_API_KEY", "--auth key to xai")?),
        ),
        "gemini" => Destination::forward(
            "https://generativelanguage.googleapis.com",
            Auth::Inject {
                header: axum_header("x-goog-api-key"),
                value: need("GEMINI_API_KEY", "--auth key to gemini")?,
            },
        ),
        other => return Err(format!("no key-mode destination named {other}").into()),
    })
}

fn pass_through_provider(name: &str) -> Result<Destination, Box<dyn std::error::Error>> {
    Ok(match name {
        "anthropic" => Destination::forward("https://api.anthropic.com", Auth::PassThrough),
        "openai" => Destination::forward("https://api.openai.com", Auth::PassThrough),
        "github" => Destination::tunnel(["api.github.com", "github.com", ".githubcopilot.com"]),
        // the two logins whose backend is not the public API: forwarded in the clear and
        // metered like any other, with a tunnel only for the token refresh
        "chatgpt" => Destination::forward("https://chatgpt.com/backend-api", Auth::PassThrough)
            .with_tunnels(["auth.openai.com"]),
        "google" => Destination::forward("https://cloudcode-pa.googleapis.com", Auth::PassThrough)
            .with_tunnels(["oauth2.googleapis.com", "accounts.google.com"]),
        other => return Err(format!("no subscription-mode destination named {other}").into()),
    })
}

fn route_for(
    harness: Harness,
    auth: AuthMode,
    provider: Option<&str>,
) -> Result<Route, Box<dyn std::error::Error>> {
    let default = if auth == AuthMode::Subscription {
        harness.subscription_provider()
    } else {
        harness.provider()
    };
    let name = provider.unwrap_or(default).to_string();
    let mut harness_env = Vec::new();
    let (destination, ceiling_of) = match auth {
        AuthMode::Subscription => {
            if harness == Harness::Copilot {
                harness_env.push(("GH_TOKEN".to_string(), need("GH_TOKEN", "copilot")?));
            }
            (pass_through_provider(&name)?, name.clone())
        }
        AuthMode::Key => (keyed_provider(&name)?, name.clone()),
        AuthMode::Gateway => {
            let key = need("LLMAO_KEY", "--auth gateway")?;
            let url = std::env::var("LLMAO_URL").unwrap_or_else(|_| "http://127.0.0.1:4000".into());
            (Destination::forward(url, bearer(key)), "llmao".to_string())
        }
    };
    Ok(Route {
        name,
        destination,
        ceiling_of,
        harness_env,
    })
}

fn axum_header(name: &'static str) -> axum::http::HeaderName {
    axum::http::HeaderName::from_static(name)
}

/// Everything a launch needs before the harness starts: the memory with its handling
/// capped, the egress with its one destination, and the socket it serves on.
struct Prepared {
    root: PathBuf,
    memory: Arc<Memory>,
    egress: Egress,
    socket: PathBuf,
    home: PathBuf,
    ceiling: Label,
    ceiling_of: String,
    provider: String,
    harness_env: Vec<(String, String)>,
}

async fn prepare_launch(
    root: &Path,
    harness: Harness,
    auth: AuthMode,
    provider: Option<&str>,
    socket: Option<PathBuf>,
) -> Result<Prepared, Box<dyn std::error::Error>> {
    let Route {
        name,
        destination,
        ceiling_of,
        harness_env,
    } = route_for(harness, auth, provider)?;
    let memory = Memory::open(root)?.with_author(format!("agent:{}", harness.binary()));
    let ceiling = memory.grants().ceiling(&ceiling_of).ok_or_else(|| {
        format!("grants.toml has no [destinations.{ceiling_of}]; add one with a ceiling")
    })?;
    let memory = Arc::new(memory.with_handling_cap(ceiling));
    let mut destinations = HashMap::new();
    destinations.insert(name.clone(), destination);
    let egress = Egress::new(
        memory.clone(),
        destinations,
        &root.join("state/telemetry.db"),
    )?;
    let run_dir = root.join("state/run");
    std::fs::create_dir_all(&run_dir)?;
    Ok(Prepared {
        memory,
        egress,
        socket: socket
            .unwrap_or_else(|| run_dir.join(format!("egress-{}.sock", std::process::id()))),
        root: root.to_path_buf(),
        home: root.join("state/sandbox").join(harness.binary()),
        ceiling,
        ceiling_of,
        provider: name,
        harness_env,
    })
}

fn report_usage(egress: &Egress) {
    let Ok(rows) = egress.telemetry().summary() else {
        return;
    };
    if rows.is_empty() {
        return;
    }
    eprintln!("memory: usage this root, by destination and model:");
    for r in rows {
        eprintln!(
            "  {:<10} {:<26} calls {:>4}  in {:>8}  out {:>8}  cache {:>8}",
            r.destination, r.model, r.calls, r.input_tokens, r.output_tokens, r.cache_read_tokens
        );
    }
}

async fn run_confined(
    p: &Prepared,
    harness: Harness,
    auth: AuthMode,
    workdir: &Path,
    args: &[String],
    capture: bool,
    rootfs: Option<&Path>,
) -> Result<(i32, Option<String>), Box<dyn std::error::Error>> {
    let host_home = home_dir();
    let plan = dm_sandbox::prepare(&dm_sandbox::Request {
        harness,
        auth,
        provider: Some(p.provider.as_str()),
        workdir,
        home: &p.home,
        egress_socket: &p.socket,
        memory_binary: &std::env::current_exe()?,
        destination_ceiling: p.ceiling,
        harness_args: args,
        host_home: host_home.as_deref(),
        harness_env: &p.harness_env,
        image: false,
        workdir_inside: None,
        rootfs,
    })?;
    Ok(tokio::task::spawn_blocking(move || {
        if capture {
            dm_sandbox::run_captured(&plan)
                .map(|(code, out)| (code, Some(String::from_utf8_lossy(&out).into_owned())))
        } else {
            dm_sandbox::run(&plan).map(|code| (code, None))
        }
    })
    .await??)
}

/// `launch`, `work` and `inside`: start a session, run it once or per task, exit with
/// the code. On the `wslc` backend the host hands the whole thing to the egress
/// container, which runs this same code as `inside`.
async fn launch_cmd(root: &Path, command: Command) -> Fallible {
    match command {
        Command::Launch { .. } => cmd_launch(root, command).await,
        Command::Work { .. } => cmd_work(root, command).await,
        Command::Inside { .. } => cmd_inside(root, command).await,
        _ => unreachable!("only launch, work and inside are routed here"),
    }
}

async fn cmd_launch(root: &Path, command: Command) -> Fallible {
    let Command::Launch {
        harness,
        auth,
        provider,
        workdir,
        unconfined,
        backend,
        image,
        prompt,
        prompt_file,
        no_handoff,
        args,
    } = command
    else {
        unreachable!("routed by launch_cmd")
    };
    let workdir = resolve_workdir(workdir)?;
    if backend_of(backend, unconfined, root) == Backend::Wslc {
        let mut inside_args = Vec::new();
        if let Some(p) = &prompt {
            inside_args.extend(["--prompt".to_string(), p.clone()]);
        }
        if let Some(f) = &prompt_file {
            inside_args.extend(["--prompt".to_string(), std::fs::read_to_string(f)?]);
        }
        if no_handoff {
            inside_args.push("--no-handoff".into());
        }
        inside_args.push("--".into());
        inside_args.extend(args);
        let code = wslc::run(&wslc::Launch {
            root: root.to_path_buf(),
            harness: harness.into(),
            auth: auth.into(),
            provider,
            workdir,
            image,
            inside_args,
        })?;
        std::process::exit(code);
    }
    let prompt = match (prompt, prompt_file) {
        (Some(p), _) => Some(p),
        (None, Some(f)) => Some(std::fs::read_to_string(&f)?),
        (None, None) => None,
    };
    let session = Session::start(
        root,
        harness.into(),
        auth.into(),
        provider.as_deref(),
        workdir,
        runner_for(backend_of(backend, unconfined, root), root)?,
    )
    .await?;
    let code = match prompt {
        Some(prompt) => work::prompted(&session, &prompt, no_handoff).await?,
        None => session.run(&args, false).await?.0,
    };
    session.finish(code);
    std::process::exit(code);
}

async fn cmd_work(root: &Path, command: Command) -> Fallible {
    let Command::Work {
        harness,
        auth,
        provider,
        workdir,
        unconfined,
        backend,
        image,
        domain,
        lease_seconds,
        keep_going,
        idle_seconds,
    } = command
    else {
        unreachable!("routed by launch_cmd")
    };
    let workdir = resolve_workdir(workdir)?;
    if backend_of(backend, unconfined, root) == Backend::Wslc {
        let mut inside_args = vec!["--work".to_string()];
        if let Some(d) = &domain {
            inside_args.extend(["--domain".to_string(), d.clone()]);
        }
        inside_args.extend(["--lease-seconds".to_string(), lease_seconds.to_string()]);
        if keep_going {
            inside_args.push("--loop".into());
        }
        inside_args.extend(["--idle-seconds".to_string(), idle_seconds.to_string()]);
        let code = wslc::run(&wslc::Launch {
            root: root.to_path_buf(),
            harness: harness.into(),
            auth: auth.into(),
            provider,
            workdir,
            image,
            inside_args,
        })?;
        std::process::exit(code);
    }
    let session = Session::start(
        root,
        harness.into(),
        auth.into(),
        provider.as_deref(),
        workdir,
        runner_for(backend_of(backend, unconfined, root), root)?,
    )
    .await?;
    let options = work::Options {
        domain,
        lease_seconds,
        keep_going,
        idle: std::time::Duration::from_secs(idle_seconds),
    };
    let code = work::run(&session, &options).await?;
    session.finish(code);
    std::process::exit(code);
}

async fn cmd_inside(root: &Path, command: Command) -> Fallible {
    let Command::Inside {
        harness,
        auth,
        provider,
        requests,
        host_home,
        workdir_inside,
        prompt,
        prompt_file,
        no_handoff,
        work,
        domain,
        lease_seconds,
        keep_going,
        idle_seconds,
        args,
    } = command
    else {
        unreachable!("routed by launch_cmd")
    };
    let runner = Runner::Requests(RequestsRunner {
        dir: requests,
        host_home,
        workdir_inside,
        next: std::sync::atomic::AtomicUsize::new(0),
    });
    // the harness container's working tree is at the inside path; this process
    // never touches it, so any existing directory serves as the host-side value
    let session = Session::start(
        root,
        harness.into(),
        auth.into(),
        provider.as_deref(),
        PathBuf::from("/"),
        runner,
    )
    .await?;
    let code = if work {
        let options = work::Options {
            domain,
            lease_seconds,
            keep_going,
            idle: std::time::Duration::from_secs(idle_seconds),
        };
        work::run(&session, &options).await?
    } else {
        let prompt = match (prompt, prompt_file) {
            (Some(p), _) => Some(p),
            (None, Some(f)) => Some(std::fs::read_to_string(&f)?),
            (None, None) => None,
        };
        match prompt {
            Some(prompt) => work::prompted(&session, &prompt, no_handoff).await?,
            None => session.run(&args, false).await?.0,
        }
    };
    session.finish(code);
    std::process::exit(code);
}

fn resolve_workdir(workdir: Option<PathBuf>) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let w = match workdir {
        Some(w) => w,
        None => std::env::current_dir()?,
    };
    Ok(std::fs::canonicalize(&w)?)
}

/// Which backend a launch resolves to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Backend {
    Namespaces,
    Image,
    Wslc,
    None,
}

/// Which backend a launch resolves to. `auto` on Linux is the unpacked image when there
/// is one, so homogeneity is the default once you have it, and the host's binaries
/// otherwise.
fn backend_of(arg: BackendArg, unconfined: bool, root: &Path) -> Backend {
    if unconfined {
        return Backend::None;
    }
    match arg {
        BackendArg::Namespaces => Backend::Namespaces,
        BackendArg::Image => Backend::Image,
        BackendArg::Wslc => Backend::Wslc,
        BackendArg::None => Backend::None,
        BackendArg::Auto => {
            if cfg!(target_os = "linux") {
                if image::present(root).is_some() {
                    Backend::Image
                } else {
                    Backend::Namespaces
                }
            } else if cfg!(windows) {
                Backend::Wslc
            } else {
                Backend::None
            }
        }
    }
}

fn runner_for(backend: Backend, root: &Path) -> Result<Runner, Box<dyn std::error::Error>> {
    Ok(match backend {
        Backend::Namespaces => Runner::Namespaces,
        Backend::Image => Runner::Image(
            image::present(root).ok_or("no unpacked image; run `memory image unpack` first")?,
        ),
        Backend::None | Backend::Wslc => Runner::Unconfined(None),
    })
}

/// A started session: the memory opened, the egress serving, the harness not yet run.
/// `launch` runs it once; `work` runs it once per task.
pub struct Session {
    p: Prepared,
    harness: Harness,
    auth: AuthMode,
    workdir: PathBuf,
    runner: Runner,
    #[cfg(unix)]
    serve_unix: tokio::task::JoinHandle<Result<(), dm_egress::Error>>,
}

/// How a session runs the harness.
pub enum Runner {
    /// Native Linux namespaces, from this host's binaries.
    Namespaces,
    /// Native Linux namespaces, pivoted into this unpacked image.
    Image(PathBuf),
    /// On the host, with the egress on a loopback TCP port.
    Unconfined(
        Option<(
            std::net::SocketAddr,
            tokio::task::JoinHandle<Result<(), dm_egress::Error>>,
        )>,
    ),
    /// Inside the egress container: each run is a request the host turns into a harness
    /// container.
    Requests(RequestsRunner),
}

/// The `Requests` runner's state.
pub struct RequestsRunner {
    /// The shared volume.
    dir: PathBuf,
    /// The host's login directories, read-only, for the subscription modes.
    host_home: Option<PathBuf>,
    /// Where the working tree appears in the harness container.
    workdir_inside: PathBuf,
    /// The next request number.
    next: std::sync::atomic::AtomicUsize,
}

impl Session {
    async fn start(
        root: &Path,
        harness: Harness,
        auth: AuthMode,
        provider: Option<&str>,
        workdir: PathBuf,
        mut runner: Runner,
    ) -> Result<Session, Box<dyn std::error::Error>> {
        let socket = match &runner {
            Runner::Requests(r) => Some(r.dir.join("egress.sock")),
            _ => None,
        };
        let p = prepare_launch(root, harness, auth, provider, socket).await?;

        #[cfg(unix)]
        let serve_unix = {
            let egress = p.egress.clone();
            let socket = p.socket.clone();
            tokio::spawn(async move { egress.serve_unix(&socket).await })
        };
        #[cfg(unix)]
        for _ in 0..100 {
            if p.socket.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        if let Runner::Requests(r) = &runner {
            // the harness container runs as uid 1000 and must reach the socket and leave
            // its exit code beside it; this process is root in the egress container
            open_to_all(&r.dir, 0o1777)?;
            open_to_all(&p.socket, 0o666)?;
        }
        #[cfg(not(unix))]
        if matches!(
            runner,
            Runner::Namespaces | Runner::Image(_) | Runner::Requests(_)
        ) {
            return Err(
                "confinement needs Linux; run under WSL2, or pass --backend wslc or --unconfined"
                    .into(),
            );
        }
        if let Runner::Unconfined(slot) = &mut runner {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let addr = listener.local_addr()?;
            drop(listener);
            let egress = p.egress.clone();
            *slot = Some((
                addr,
                tokio::spawn(async move { egress.serve_tcp(addr).await }),
            ));
        }
        let how = match &runner {
            Runner::Namespaces => "confined to",
            Runner::Image(_) => "confined, in the harness image, to",
            Runner::Unconfined(_) => "unconfined in",
            Runner::Requests(_) => "in the harness image, working tree at",
        };
        let shown = match &runner {
            Runner::Requests(r) => r.workdir_inside.clone(),
            _ => workdir.clone(),
        };
        eprintln!(
            "memory: {} {how} {} · model calls via {} at ceiling {} · session {}",
            harness.binary(),
            shown.display(),
            p.ceiling_of,
            p.ceiling,
            p.memory.session_id()
        );
        Ok(Session {
            p,
            harness,
            auth,
            workdir,
            runner,
            #[cfg(unix)]
            serve_unix,
        })
    }

    /// The opened memory, for a client in the same process.
    pub fn memory(&self) -> Arc<Memory> {
        self.p.memory.clone()
    }

    /// Which harness.
    pub fn harness(&self) -> Harness {
        self.harness
    }

    /// Run the harness once with these arguments. With `capture`, its standard output
    /// comes back instead of reaching the terminal.
    ///
    /// # Errors
    ///
    /// When the sandbox cannot be built or the harness cannot be spawned.
    pub async fn run(
        &self,
        args: &[String],
        capture: bool,
    ) -> Result<(i32, Option<String>), Box<dyn std::error::Error>> {
        match &self.runner {
            Runner::Unconfined(Some((addr, _))) => run_unconfined(
                self.harness,
                self.auth,
                &self.workdir,
                &self.p.home,
                &format!("http://{addr}"),
                args,
                capture,
            ),
            Runner::Unconfined(None) => Err("the unconfined runner was not started".into()),
            Runner::Namespaces => {
                run_confined(
                    &self.p,
                    self.harness,
                    self.auth,
                    &self.workdir,
                    args,
                    capture,
                    None,
                )
                .await
            }
            Runner::Image(rootfs) => {
                run_confined(
                    &self.p,
                    self.harness,
                    self.auth,
                    &self.workdir,
                    args,
                    capture,
                    Some(rootfs),
                )
                .await
            }
            Runner::Requests(r) => self.run_requested(r, args, capture).await,
        }
    }

    /// Write a run request for the host and wait for the harness container's exit code.
    async fn run_requested(
        &self,
        r: &RequestsRunner,
        args: &[String],
        capture: bool,
    ) -> Result<(i32, Option<String>), Box<dyn std::error::Error>> {
        let plan = dm_sandbox::prepare(&dm_sandbox::Request {
            harness: self.harness,
            auth: self.auth,
            provider: Some(self.p.provider.as_str()),
            workdir: &self.workdir,
            home: &self.p.home,
            egress_socket: &self.p.socket,
            memory_binary: Path::new(dm_sandbox::INSIDE_MEMORY),
            destination_ceiling: self.p.ceiling,
            harness_args: args,
            host_home: r.host_home.as_deref(),
            harness_env: &self.p.harness_env,
            image: true,
            workdir_inside: Some(&r.workdir_inside),
            rootfs: None,
        })?;
        let n = r.next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let home_rel = self
            .p
            .home
            .strip_prefix(&self.p.root)
            .map(|h| h.display().to_string())
            .unwrap_or_default();
        let req = requests::RunRequest {
            argv: plan.argv,
            env: plan.env,
            cwd: plan.cwd.display().to_string(),
            home: home_rel,
            interactive: !capture,
        };
        let tmp = r.dir.join(format!("{}.tmp", requests::RunRequest::file(n)));
        std::fs::write(&tmp, serde_json::to_vec(&req)?)?;
        std::fs::rename(&tmp, r.dir.join(requests::RunRequest::file(n)))?;
        let exit = PathBuf::from(requests::RunRequest::exit_file(n));
        let exit = r.dir.join(exit.file_name().unwrap_or_default());
        loop {
            if let Ok(text) = std::fs::read_to_string(&exit) {
                if let Ok(code) = text.trim().parse::<i32>() {
                    let out = if capture {
                        let out = PathBuf::from(requests::RunRequest::out_file(n));
                        std::fs::read_to_string(r.dir.join(out.file_name().unwrap_or_default()))
                            .ok()
                    } else {
                        None
                    };
                    return Ok((code, out.or_else(|| capture.then(String::new))));
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }

    /// Stop serving, report usage, and tell the host the final code where there is one.
    pub fn finish(self, code: i32) {
        if let Runner::Unconfined(Some((_, serve))) = &self.runner {
            serve.abort();
        }
        #[cfg(unix)]
        {
            self.serve_unix.abort();
            std::fs::remove_file(&self.p.socket).ok();
        }
        report_usage(&self.p.egress);
        if let Runner::Requests(r) = &self.runner {
            std::fs::write(r.dir.join(requests::DONE), code.to_string()).ok();
        }
    }
}

#[cfg(unix)]
fn open_to_all(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn open_to_all(_path: &Path, _mode: u32) -> std::io::Result<()> {
    Ok(())
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Run the harness on the host with the egress on a TCP port. The same configuration as
/// the confined path, minus the namespaces.
#[allow(clippy::too_many_arguments)] // the host-side twin of dm_sandbox::Request
fn run_unconfined(
    harness: Harness,
    auth: AuthMode,
    workdir: &Path,
    home: &Path,
    egress_url: &str,
    args: &[String],
    capture: bool,
) -> Result<(i32, Option<String>), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(home)?;
    let mut cmd = std::process::Command::new(harness.binary());
    cmd.current_dir(workdir);
    match harness {
        Harness::Claude => {
            let mcp = serde_json::json!({
                "mcpServers": { "memory": { "type": "http", "url": format!("{egress_url}/mcp") } }
            });
            let mcp_path = home.join("mcp.json");
            std::fs::write(&mcp_path, mcp.to_string())?;
            cmd.env("ANTHROPIC_BASE_URL", format!("{egress_url}/anthropic"));
            if auth != AuthMode::Subscription {
                cmd.env("ANTHROPIC_AUTH_TOKEN", "egress");
            }
            cmd.arg("--mcp-config")
                .arg(&mcp_path)
                .arg("--strict-mcp-config");
        }
        Harness::Gemini | Harness::Copilot | Harness::OpenCode => {
            return Err(format!(
                "--unconfined is wired for claude and codex only so far; {} runs confined",
                harness.binary()
            )
            .into());
        }
        Harness::Codex => {
            let codex_home = home.join("codex-home");
            std::fs::create_dir_all(&codex_home)?;
            std::fs::write(
                codex_home.join("config.toml"),
                format!(
                    "model_provider = \"egress\"\n\n[model_providers.egress]\nname = \"egress\"\nbase_url = \"{egress_url}/openai/v1\"\nenv_key = \"DM_EGRESS_TOKEN\"\nwire_api = \"responses\"\n\n[mcp_servers.memory]\nurl = \"{egress_url}/mcp\"\n"
                ),
            )?;
            cmd.env("CODEX_HOME", &codex_home);
            cmd.env("DM_EGRESS_TOKEN", "egress");
        }
    }
    cmd.args(args);
    if capture {
        let out = cmd
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit())
            .output()?;
        return Ok((
            out.status.code().unwrap_or(1),
            Some(String::from_utf8_lossy(&out.stdout).into_owned()),
        ));
    }
    let status = cmd.status()?;
    Ok((status.code().unwrap_or(1), None))
}

/// Inside the sandbox, as pid 1 of the harness's namespace: forward the loopback port to
/// the socket and run the harness.
#[cfg(not(unix))]
fn relay(
    _socket: &Path,
    _listen: &str,
    _argv: &[OsString],
) -> Result<i32, Box<dyn std::error::Error>> {
    Err("the relay runs inside a Linux sandbox only".into())
}

#[cfg(unix)]
fn relay(
    socket: &Path,
    listen: &str,
    argv: &[OsString],
) -> Result<i32, Box<dyn std::error::Error>> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let socket = socket.to_path_buf();
    let listen = listen.to_string();
    rt.spawn(async move {
        let Ok(listener) = tokio::net::TcpListener::bind(&listen).await else {
            eprintln!("relay: cannot listen on {listen}");
            return;
        };
        loop {
            let Ok((mut inbound, _)) = listener.accept().await else {
                continue;
            };
            let socket = socket.clone();
            tokio::spawn(async move {
                match tokio::net::UnixStream::connect(&socket).await {
                    Ok(mut outbound) => {
                        let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                    }
                    Err(e) => eprintln!("relay: egress socket: {e}"),
                }
            });
        }
    });
    let argv = argv.to_vec();
    let code =
        rt.block_on(
            async move { tokio::task::spawn_blocking(move || spawn_harness(&argv)).await },
        )??;
    Ok(code)
}

#[cfg(unix)]
fn spawn_harness(argv: &[OsString]) -> Result<i32, String> {
    #[cfg(target_os = "linux")]
    {
        dm_sandbox::linux::spawn_in_pid_namespace(argv).map_err(|e| e.to_string())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let status = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .status()
            .map_err(|e| e.to_string())?;
        Ok(status.code().unwrap_or(1))
    }
}

async fn entries(m: &Memory, command: Command) -> Fallible {
    match command {
        Command::Write {
            kind,
            title,
            domain,
            body,
            classification,
            source,
            supersedes,
        } => {
            let w = m
                .write(WriteRequest {
                    domain,
                    entry_type: kind.into(),
                    title,
                    body: body_or_stdin(body)?,
                    source,
                    confidence: None,
                    classification: classification.map(Into::into),
                    supersedes,
                    review_by: None,
                    summary: None,
                })
                .await?;
            json(&w)
        }
        Command::Search {
            query,
            domain,
            limit,
        } => json(&hits_json(
            &m.search(&query, domain.as_deref(), limit).await?,
        )),
        Command::Get { id } => {
            print!("{}", m.get(&id).await?.render());
            Ok(())
        }
        Command::List {
            kind,
            domain,
            limit,
        } => json(&hits_json(
            &m.list(domain.as_deref(), kind.map(Into::into), limit)
                .await?,
        )),
        Command::T0 { domain } => {
            print!("{}", m.t0(domain.as_deref()).await?);
            Ok(())
        }
        other => maintain(m, other).await,
    }
}

async fn maintain(m: &Memory, command: Command) -> Fallible {
    match command {
        Command::Export {
            dest,
            domain,
            max_label,
        } => {
            let ex = m.export(domain.as_deref(), max_label.into(), &dest).await?;
            println!(
                "exported {} entries from {} to {}, highest label {}",
                ex.manifest.entries.len(),
                ex.manifest.domain,
                dest.display(),
                ex.manifest.highest_label
            );
            Ok(())
        }
        Command::Import { src, domain } => {
            let im = m.import(&src, domain.as_deref()).await?;
            println!(
                "imported {} entries, skipped {} already present",
                im.written.len(),
                im.skipped.len()
            );
            Ok(())
        }
        Command::Reindex => {
            let (n, events) = m.reindex().await?;
            println!("indexed {n} entries, replayed {events} ledger events");
            Ok(())
        }
        Command::Init { .. }
        | Command::Serve { .. }
        | Command::Launch { .. }
        | Command::Work { .. }
        | Command::Inside { .. }
        | Command::Image { .. }
        | Command::Relay { .. }
        | Command::Usage
        | Command::Task { .. }
        | Command::Write { .. }
        | Command::Search { .. }
        | Command::Get { .. }
        | Command::List { .. }
        | Command::T0 { .. } => unreachable!("dispatched before reaching maintain"),
    }
}

async fn task(m: &Memory, op: TaskOp) -> Fallible {
    match op {
        TaskOp::Create {
            title,
            body,
            domain,
            requirements,
            depends_on,
            priority,
        } => {
            let c = m
                .create_task(TaskSpec {
                    domain: domain.unwrap_or_default(),
                    title,
                    body,
                    requirements,
                    depends_on,
                    origin: None,
                    upstream_event: None,
                    created_by: String::new(),
                    classification: Label::Public,
                    priority,
                })
                .await?;
            json(&c.task)
        }
        TaskOp::Admit { id } => json(&m.admit(&id).await?),
        TaskOp::List { status, domain } => json(&m.tasks(domain.as_deref(), status.into()).await?),
        TaskOp::ClaimNext {
            domain,
            lease_seconds,
        } => match m
            .claim_next(domain.as_deref(), Duration::seconds(lease_seconds))
            .await?
        {
            Some(c) => json(&c.task),
            None => {
                println!("null");
                Ok(())
            }
        },
        TaskOp::Heartbeat { id, lease_seconds } => {
            json(&m.heartbeat(&id, Duration::seconds(lease_seconds)).await?)
        }
        TaskOp::Complete { id, handoff } => json(&m.complete(&id, &handoff).await?),
        TaskOp::Release { id, handoff } => json(&m.release(&id, handoff.as_deref()).await?),
        TaskOp::Invalidate { id, reason } => json(&m.invalidate(&id, &reason, None).await?),
        TaskOp::Get { id } => json(&m.get_task(&id).await?),
    }
}
