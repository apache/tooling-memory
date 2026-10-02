//! The `memory` binary: the per-session memory server, the egress, the launcher, and the
//! command line.
//!
//! Every subcommand is a thin call into the library crates; the MCP server and the command
//! line see the same operations. The binary resolves its mounts at startup (`--root`, or
//! `MEMORY_ROOT`) and, for `launch`, reads the one or two credentials it needs from its own
//! environment and never passes them inside.

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
        /// Arguments passed to the harness.
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
    /// Give a task back.
    Release { id: String },
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
        Command::Launch {
            harness,
            auth,
            provider,
            workdir,
            unconfined,
            args,
        } => {
            launch(
                &root,
                harness.into(),
                auth.into(),
                provider.as_deref(),
                workdir,
                unconfined,
                args,
            )
            .await
        }
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
        other => return Err(format!("no subscription-mode destination named {other}").into()),
    })
}

fn route_for(
    harness: Harness,
    auth: AuthMode,
    provider: Option<&str>,
) -> Result<Route, Box<dyn std::error::Error>> {
    let name = provider.unwrap_or(harness.provider()).to_string();
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
        socket: run_dir.join(format!("egress-{}.sock", std::process::id())),
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
) -> Result<i32, Box<dyn std::error::Error>> {
    let creds = home_dir().map(|h| h.join(".claude/.credentials.json"));
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
        claude_credentials: creds.as_deref().filter(|c| c.is_file()),
        harness_env: &p.harness_env,
    })?;
    Ok(tokio::task::spawn_blocking(move || dm_sandbox::run(&plan)).await??)
}

async fn launch(
    root: &Path,
    harness: Harness,
    auth: AuthMode,
    provider: Option<&str>,
    workdir: Option<PathBuf>,
    unconfined: bool,
    args: Vec<String>,
) -> Fallible {
    let workdir = match workdir {
        Some(w) => w,
        None => std::env::current_dir()?,
    };
    let workdir = std::fs::canonicalize(&workdir)?;
    let p = prepare_launch(root, harness, auth, provider).await?;

    #[cfg(unix)]
    let serve = {
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
    #[cfg(not(unix))]
    if !unconfined {
        return Err("confinement needs Linux; run under WSL2, or pass --unconfined".into());
    }
    eprintln!(
        "memory: {} confined to {} · model calls via {} at ceiling {} · session {}",
        harness.binary(),
        workdir.display(),
        p.ceiling_of,
        p.ceiling,
        p.memory.session_id()
    );

    let code = if unconfined {
        let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = tcp.local_addr()?;
        drop(tcp);
        let serve_tcp = {
            let egress = p.egress.clone();
            tokio::spawn(async move { egress.serve_tcp(addr).await })
        };
        let code = run_unconfined(
            harness,
            auth,
            &workdir,
            &p.home,
            &format!("http://{addr}"),
            &args,
        )?;
        serve_tcp.abort();
        code
    } else {
        run_confined(&p, harness, auth, &workdir, &args).await?
    };

    #[cfg(unix)]
    {
        serve.abort();
        std::fs::remove_file(&p.socket).ok();
    }
    report_usage(&p.egress);
    std::process::exit(code);
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Run the harness on the host with the egress on a TCP port. The same configuration as
/// the confined path, minus the namespaces.
fn run_unconfined(
    harness: Harness,
    auth: AuthMode,
    workdir: &Path,
    home: &Path,
    egress_url: &str,
    args: &[String],
) -> Result<i32, Box<dyn std::error::Error>> {
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
    let status = cmd.status()?;
    Ok(status.code().unwrap_or(1))
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
        TaskOp::Release { id } => json(&m.release(&id).await?),
        TaskOp::Invalidate { id, reason } => json(&m.invalidate(&id, &reason, None).await?),
        TaskOp::Get { id } => json(&m.get_task(&id).await?),
    }
}
