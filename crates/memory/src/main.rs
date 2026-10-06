//! The `memory` binary: the per-session memory server, the egress, the launcher, and the
//! command line.
//!
//! Every subcommand is a thin call into the library crates; the MCP server and the command
//! line see the same operations. The binary resolves its mounts at startup (`--root`, or
//! `MEMORY_ROOT`) and, for `launch`, reads the one or two credentials it needs from its own
//! environment and never passes them inside.

mod board;
mod deputy;
mod golden;
mod harness_usage;
mod hooks;
mod image;
mod mcp_http;
mod mine;
mod model;
mod requests;
mod savings;
mod taps;
mod work;
mod wslc;
mod wslc_api;

use clap::{Parser, Subcommand, ValueEnum};
use dm_core::{EntryType, Label};
use dm_egress::{Auth, Destination, Egress, Telemetry};
use dm_ledger::{Status, TaskSpec};
use dm_sandbox::{AuthMode, Harness};
use dm_server::recall::{self, Budget, Depth, Deputy};
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

/// How much of each hit a search or a recall carries.
#[derive(Clone, Copy, ValueEnum)]
enum DepthArg {
    /// Title and summary.
    Summaries,
    /// Bodies too, best first, within a budget.
    Sections,
}

impl From<DepthArg> for Depth {
    fn from(d: DepthArg) -> Depth {
        match d {
            DepthArg::Summaries => Depth::Summaries,
            DepthArg::Sections => Depth::Sections,
        }
    }
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

/// Where a subagent the harness would spawn runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
enum SubagentsArg {
    /// In the harness, as it would without the launcher.
    #[default]
    Harness,
    /// Filed on the task board as a task of its own; the call is refused with its id.
    Board,
}

#[derive(Clone, Copy, ValueEnum)]
enum HarnessArg {
    Claude,
    Codex,
    Gemini,
    /// Google's Antigravity CLI, `agy`.
    Antigravity,
    /// Alibaba's Qwen Code.
    Qwen,
    /// xAI's Grok Build, `grok`.
    Grok,
    /// Meta's Muse Code, `muse`.
    Muse,
    Copilot,
    Opencode,
    /// A program: the first argument after `--` is run as given, with the egress and
    /// the memory server in its environment.
    Command,
}

impl From<HarnessArg> for Harness {
    fn from(h: HarnessArg) -> Harness {
        match h {
            HarnessArg::Claude => Harness::Claude,
            HarnessArg::Codex => Harness::Codex,
            HarnessArg::Gemini => Harness::Gemini,
            HarnessArg::Antigravity => Harness::Antigravity,
            HarnessArg::Qwen => Harness::Qwen,
            HarnessArg::Grok => Harness::Grok,
            HarnessArg::Muse => Harness::Muse,
            HarnessArg::Copilot => Harness::Copilot,
            HarnessArg::Opencode => Harness::OpenCode,
            HarnessArg::Command => Harness::Command,
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

/// How mining reaches a model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum MineVia {
    /// Through the harness on a subscription login, directly through the egress otherwise.
    Auto,
    /// A prompted run of the harness itself, which is the client the login belongs to.
    Harness,
    /// A direct call through the egress, with a key or the gateway.
    Api,
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
    /// environment: ANTHROPIC_API_KEY, OPENAI_API_KEY, GEMINI_API_KEY, XAI_API_KEY or
    /// DASHSCOPE_API_KEY for --auth key, LLMAO_KEY (and LLMAO_URL) for --auth gateway,
    /// the harness's own login for --auth subscription.
    Launch {
        /// Which harness. No default: say which one.
        #[arg(long, value_enum)]
        harness: HarnessArg,
        #[arg(long, value_enum, default_value = "subscription")]
        auth: AuthArg,
        /// Send model calls to another destination: openai, xai (Grok), dashscope, or a
        /// name from grants.toml. Codex, Qwen Code and OpenCode only.
        #[arg(long)]
        provider: Option<String>,
        /// A credential of their own for the memory's calls, the recall deputy and
        /// mining, so a harness on a personal login can leave them to an institution's
        /// gateway: key or gateway. They follow --auth otherwise, and a login gives
        /// them nothing.
        #[arg(long, value_enum)]
        server_auth: Option<AuthArg>,
        /// Where the memory's own calls go under --server-auth key; the gateway needs
        /// no name.
        #[arg(long)]
        server_provider: Option<String>,
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
        /// How the `wslc` backend drives WSL containers: the C API on Windows where it
        /// can serve (not interactive, not the gateway), or the command line.
        #[arg(long, value_enum, env = "MEMORY_WSLC_VIA", default_value = "auto")]
        wslc_via: wslc::Via,
        /// Where the C API keeps its session disks; under the root's state by default,
        /// and needed when the root is a `\\wsl.localhost` path.
        #[arg(long, env = "MEMORY_WSLC_STORAGE")]
        wslc_storage: Option<PathBuf>,
        /// Non-interactive: run this prompt in the harness's headless form, print its
        /// answer, and exit when it does. The run is expected to leave a handoff.
        #[arg(long, conflicts_with = "prompt_file")]
        prompt: Option<String>,
        /// As --prompt, read from a file.
        #[arg(long)]
        prompt_file: Option<PathBuf>,
        /// The model the harness runs, in its own flag; for Claude Code also every
        /// role's default, so a gateway that serves one model is asked for that one.
        #[arg(long)]
        model: Option<String>,
        /// Where a subagent the harness would spawn runs: in the harness, or filed on
        /// the task board as a task of its own and refused here.
        #[arg(long, value_enum, default_value = "harness")]
        subagents: SubagentsArg,
        /// With --prompt: a run that leaves no handoff is fine.
        #[arg(long)]
        no_handoff: bool,
        /// After the run, mine the session's transcript for entries with a small model
        /// through the same destination; they land raw, low confidence, with the
        /// transcript as source.
        #[arg(long)]
        mine: bool,
        /// The model mining uses; a small default per destination otherwise.
        #[arg(long)]
        mine_model: Option<String>,
        /// How mining reaches the model.
        #[arg(long, value_enum, default_value = "auto")]
        mine_via: MineVia,
        /// Arguments passed to the harness.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Claim the next task from the ledger and run the harness on it, non-interactively.
    /// The launcher holds the lease and heartbeats while the harness works. A task the
    /// harness completed stays complete; anything else goes back to the pool with the
    /// harness's output filed as a handoff. One invocation, one task; --loop keeps going.
    Work {
        /// Which harness. No default: say which one.
        #[arg(long, value_enum)]
        harness: HarnessArg,
        #[arg(long, value_enum, default_value = "subscription")]
        auth: AuthArg,
        /// Send model calls to another destination. Codex, Qwen Code and OpenCode only.
        #[arg(long)]
        provider: Option<String>,
        /// A credential of their own for the memory's calls, the recall deputy and
        /// mining, so a harness on a personal login can leave them to an institution's
        /// gateway: key or gateway. They follow --auth otherwise, and a login gives
        /// them nothing.
        #[arg(long, value_enum)]
        server_auth: Option<AuthArg>,
        /// Where the memory's own calls go under --server-auth key; the gateway needs
        /// no name.
        #[arg(long)]
        server_provider: Option<String>,
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
        /// How the `wslc` backend drives WSL containers: the C API on Windows where it
        /// can serve (not interactive, not the gateway), or the command line.
        #[arg(long, value_enum, env = "MEMORY_WSLC_VIA", default_value = "auto")]
        wslc_via: wslc::Via,
        /// Where the C API keeps its session disks; under the root's state by default,
        /// and needed when the root is a `\\wsl.localhost` path.
        #[arg(long, env = "MEMORY_WSLC_STORAGE")]
        wslc_storage: Option<PathBuf>,
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
        /// The model the harness runs, in its own flag.
        #[arg(long)]
        model: Option<String>,
        /// Where a subagent the harness would spawn runs. On the board by default here:
        /// a task's fan-out is more tasks, which the board claims and runs.
        #[arg(long, value_enum, default_value = "board")]
        subagents: SubagentsArg,
        /// After each task, mine the session's transcript for entries.
        #[arg(long)]
        mine: bool,
        #[arg(long)]
        mine_model: Option<String>,
        #[arg(long, value_enum, default_value = "auto")]
        mine_via: MineVia,
    },
    /// The models a destination lists, as JSON: what the web form offers to pick from.
    /// Routed as a launch would be, with the destination's credential; a login mode has
    /// no list, since the login is the harness's own.
    Models {
        #[arg(long, value_enum, default_value = "key")]
        auth: AuthArg,
        /// The destination; the gateway's with --auth gateway, openai otherwise.
        #[arg(long)]
        provider: Option<String>,
    },
    /// Mine one transcript for entries with a small model through a destination.
    Mine {
        /// The transcript, JSON Lines, from any of the harnesses.
        transcript: PathBuf,
        #[arg(long, value_enum, default_value = "key")]
        auth: AuthArg,
        /// The destination; openai by default.
        #[arg(long, default_value = "openai")]
        provider: String,
        #[arg(long)]
        model: Option<String>,
    },
    /// Prove the WSL containers C API end to end: take a session, provision the image
    /// into its store, and run a container from it. Windows.
    #[command(hide = true)]
    WslcApiCheck {
        /// The image to provision and run.
        #[arg(long, env = "MEMORY_IMAGE", default_value = wslc::DEFAULT_IMAGE)]
        image: String,
        /// Where the session disks live; under the root's state by default.
        #[arg(long, env = "MEMORY_WSLC_STORAGE")]
        wslc_storage: Option<PathBuf>,
        /// Provision again from the command line's copy, or the registry's, even when
        /// the session's store has the image: after `make image`.
        #[arg(long)]
        refresh: bool,
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
        #[arg(long, value_enum)]
        server_auth: Option<AuthArg>,
        #[arg(long)]
        server_provider: Option<String>,
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
        model: Option<String>,
        #[arg(long, value_enum, default_value = "harness")]
        subagents: SubagentsArg,
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
    /// The paper's read-path claims measured on this record: recovery from T0 and a
    /// handoff against transcript replay, a page of hits against bodies and corpus,
    /// known-item lookups in the top three, and tokens served per session beside what
    /// its model calls ingested.
    Savings {
        /// The query for the retrieval section; the newest entry's title when omitted.
        #[arg(long)]
        query: Option<String>,
        /// Numbers as JSON instead of the text report.
        #[arg(long)]
        json: bool,
    },
    /// Score a golden suite: questions and the entries that answer them, each question
    /// asked as written, stripped of stopwords, and by its keywords, through the pack
    /// with no model; with `--provider`, the deputy's brief too.
    Golden {
        /// The suite, TOML; see golden/docs.toml.
        suite: PathBuf,
        /// Hits count within the top k.
        #[arg(long, default_value_t = 8)]
        k: usize,
        /// Numbers as JSON instead of the text report.
        #[arg(long)]
        json: bool,
        /// A destination to run the deputy through, so the brief is scored too.
        #[arg(long)]
        provider: Option<String>,
        #[arg(long, value_enum, default_value = "key", requires = "provider")]
        auth: AuthArg,
        /// The deputy's model; a small default per destination otherwise.
        #[arg(long, requires = "provider")]
        model: Option<String>,
    },
    /// Inside the sandbox, run by the harness's hooks: read the event from standard
    /// input and leave a note in the outbox for the launcher to file as a handoff.
    #[command(hide = true)]
    Hook {
        /// Which harness's hook format: claude, codex, gemini, antigravity, qwen or grok.
        harness: String,
    },
    /// A web terminal on the launcher: sessions in pseudo-terminals streamed to the
    /// browser, with a reading pane over the record. Loopback only.
    Web {
        /// Where to listen.
        #[arg(long, default_value = "127.0.0.1:4200")]
        listen: String,
        /// The harness the form preselects; each session names its own.
        #[arg(long, value_enum)]
        harness: Option<HarnessArg>,
        #[arg(long, value_enum, default_value = "subscription")]
        auth: AuthArg,
        #[arg(long, value_enum, default_value = "auto")]
        backend: BackendArg,
        /// The working tree sessions start in. Defaults to the current directory.
        #[arg(long)]
        workdir: Option<PathBuf>,
    },
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
        /// Summaries, or the best hits with their bodies within a budget.
        #[arg(long, value_enum, default_value = "summaries")]
        depth: DepthArg,
    },
    /// Delegated recall: a cited brief when a model is behind it, else the pack the
    /// brief would be written from, with the synthesis owed to the caller.
    Recall {
        question: String,
        #[arg(long)]
        domain: Option<String>,
        /// How much of each entry comes back when synthesis is owed; the session's
        /// default otherwise: sections, or summaries once it has fetched in parallel.
        #[arg(long, value_enum)]
        depth: Option<DepthArg>,
        /// A destination to run the deputy through, with `--auth` and `--model`. With
        /// none, the pack comes back with synthesis owed.
        #[arg(long)]
        provider: Option<String>,
        #[arg(long, value_enum, default_value = "key", requires = "provider")]
        auth: AuthArg,
        /// The deputy's model; a small default per destination otherwise.
        #[arg(long, requires = "provider")]
        model: Option<String>,
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
    /// A tap: a connector run as a WASI component with exactly the access its grant
    /// allows.
    Tap {
        #[command(subcommand)]
        op: TapOp,
    },
    /// Import a bundle directory into a domain.
    Import {
        src: PathBuf,
        #[arg(long)]
        domain: Option<String>,
    },
    /// Rebuild the index from the record and the ledger from its events.
    Reindex,
    /// Erase an entry: the body leaves every version and the index, a stub and a receipt
    /// remain, and entries that cite it are flagged for review. Irreversible; the owner
    /// only.
    Erase {
        id: String,
        /// Why, as a class: the receipt keeps it.
        #[arg(long, value_enum)]
        reason: ReasonArg,
    },
}

/// Why an entry is erased, on the command line.
#[derive(Clone, Copy, ValueEnum)]
enum ReasonArg {
    /// The person it concerns, or its author, asked.
    Requested,
    /// It should never have been written.
    Mistaken,
    /// It carries material above what the record may hold.
    Sensitive,
    /// A legal obligation.
    Legal,
}

impl From<ReasonArg> for dm_core::erasure::Reason {
    fn from(r: ReasonArg) -> Self {
        match r {
            ReasonArg::Requested => Self::Requested,
            ReasonArg::Mistaken => Self::Mistaken,
            ReasonArg::Sensitive => Self::Sensitive,
            ReasonArg::Legal => Self::Legal,
        }
    }
}

/// Tap operations.
#[derive(Subcommand)]
enum TapOp {
    /// Copy a component under the root as `name`; `[taps.<name>]` in grants.toml says
    /// what it may do.
    Install { name: String, file: PathBuf },
    /// Run a tap once, or on a schedule. Its outcome is printed as JSON.
    Run {
        name: String,
        /// The tap's own configuration, JSON.
        #[arg(long)]
        config: Option<String>,
        /// Run again every this many seconds until stopped.
        #[arg(long)]
        every: Option<u64>,
    },
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
    // A hook needs no record and no runtime: it reads one event and writes one file.
    if args.get(1).is_some_and(|a| a == "hook") {
        let outbox = std::env::var_os("MEMORY_OUTBOX")
            .map(PathBuf::from)
            .or_else(|| home_dir().map(|h| h.join(hooks::OUTBOX)));
        let harness = args.get(2).and_then(|a| a.to_str()).unwrap_or("claude");
        match outbox {
            Some(outbox) => {
                if let Err(e) = hooks::run(&outbox, harness) {
                    eprintln!("memory hook: {e}");
                }
            }
            None => eprintln!("memory hook: no MEMORY_OUTBOX and no HOME"),
        }
        std::process::exit(0);
    }
    // The command futures are large and Windows gives the main thread one megabyte, so
    // the CLI runs on a thread with room; the runtime starts there.
    let outcome = std::thread::Builder::new()
        .name("memory".into())
        .stack_size(64 << 20)
        .spawn(|| run_cli().map_err(|e| e.to_string()))
        .and_then(|h| {
            h.join()
                .map_err(|_| std::io::Error::other("the CLI thread panicked"))
        });
    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
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
        Command::Hook { .. } => unreachable!("handled in main before the root is needed"),
        cmd @ Command::Web { .. } => web_cmd(&root, cmd).await,
        cmd @ (Command::Mine { .. }
        | Command::Models { .. }
        | Command::WslcApiCheck { .. }
        | Command::Savings { .. }
        | Command::Golden { .. }
        | Command::Recall {
            provider: Some(_), ..
        }) => tools_cmd(&root, cmd).await,
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
            print_usage(&t.summary()?);
            Ok(())
        }
        Command::Task { op } => task(&Memory::open(&root)?, op).await,
        Command::Tap { op } => tap_cmd(&root, op).await,
        other => entries(&Memory::open(&root)?, other).await,
    }
}

/// What one launch needs for its model calls: the destination name as the harness will
/// address it, the destination itself, the grants entry whose ceiling caps the session,
/// and any credential the harness must hold itself.
#[derive(Debug)]
struct Route {
    name: String,
    destination: Destination,
    ceiling_of: String,
    harness_env: Vec<(String, String)>,
}

/// The route the memory's own model calls take: the recall deputy and mining. By default
/// the harness's; `--server-auth` gives them a credential of their own, so a harness on
/// a personal login can leave them to an institution's gateway.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ServerRoute {
    /// How those calls authenticate; a login means they are not made at all.
    auth: AuthMode,
    /// The egress destination they go to.
    provider: String,
}

/// The server's route beside the harness's, and the destination to register for it when
/// it is not the harness's own. The gateway's route is named for the gateway, so it
/// never shares the harness's name; a key to a provider the harness's login already
/// names would, with a different destination behind it, and is refused rather than
/// guessed at.
fn server_route(
    harness: &Route,
    auth: AuthMode,
    server_auth: Option<AuthMode>,
    server_provider: Option<&str>,
) -> Result<(ServerRoute, Option<Route>), Box<dyn std::error::Error>> {
    let follows = ServerRoute {
        auth,
        provider: harness.name.clone(),
    };
    let Some(mode) = server_auth else {
        return Ok((follows, None));
    };
    if mode == AuthMode::Subscription {
        return Err(
            "a subscription login is the harness's alone and cannot stand in for the \
                    memory's own calls; --server-auth takes key or gateway"
                .into(),
        );
    }
    if mode == AuthMode::Gateway && server_provider.is_some() {
        return Err(
            "--server-provider names a destination for --server-auth key; the gateway \
                    is one destination and takes no name"
                .into(),
        );
    }
    let route = route_for(Harness::Command, mode, server_provider)?;
    let server = ServerRoute {
        auth: mode,
        provider: route.name.clone(),
    };
    if route.name != harness.name {
        return Ok((server, Some(route)));
    }
    // the same name under the same mode is the same route, built by the same arm; under
    // another mode it is another destination, a login's pass-through beside a key's
    // injection say, which one name cannot hold
    if mode == auth {
        return Ok((server, None));
    }
    Err(format!(
        "the memory's own route would share the name {} with the harness's while going \
         somewhere else; use --server-auth gateway, or --server-provider with another name",
        route.name
    )
    .into())
}

/// What a session authenticates with: the harness's route, and the memory's own when it
/// has one.
#[derive(Clone, Copy, Debug)]
struct Credentials<'a> {
    auth: AuthMode,
    provider: Option<&'a str>,
    server_auth: Option<AuthMode>,
    server_provider: Option<&'a str>,
}

impl<'a> Credentials<'a> {
    /// One route for everything: a tool run on the host, where the harness is the
    /// launcher itself.
    fn keyed(auth: AuthMode, provider: &'a str) -> Credentials<'a> {
        Credentials {
            auth,
            provider: Some(provider),
            server_auth: None,
            server_provider: None,
        }
    }
}

/// Whether mining runs as a prompted run of the harness: asked for, or by default when
/// the memory's own calls have no credential of their own.
fn via_harness(via: MineVia, server_auth: AuthMode) -> bool {
    match via {
        MineVia::Harness => true,
        MineVia::Api => false,
        MineVia::Auto => server_auth == AuthMode::Subscription,
    }
}

/// A variable's value, if it carries one: a variable set to nothing is not set.
fn present(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.trim().is_empty())
}

fn need(var: &str, why: &str) -> Result<String, Box<dyn std::error::Error>> {
    present(std::env::var(var).ok())
        .ok_or_else(|| format!("{why} needs {var} in the launcher's environment").into())
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
        // Alibaba's models over OpenAI's API, at the international endpoint; the
        // harness appends /v1 as it does at OpenAI
        "dashscope" => Destination::forward(
            "https://dashscope-intl.aliyuncs.com/compatible-mode",
            bearer(need("DASHSCOPE_API_KEY", "--auth key to dashscope")?),
        ),
        // Meta's Model API: OpenAI's shapes under /v1 and Anthropic's at the root, and
        // Muse Code's own model catalog at /v1/muse-code/models, so the whole host
        "meta" => Destination::forward(
            "https://api.meta.ai",
            bearer(need("META_API_KEY", "--auth key to meta")?),
        )
        .with_origin_paths(),
        other => return Err(format!("no key-mode destination named {other}").into()),
    })
}

fn pass_through_provider(name: &str) -> Result<Destination, Box<dyn std::error::Error>> {
    Ok(match name {
        "anthropic" => Destination::forward("https://api.anthropic.com", Auth::PassThrough),
        "openai" => Destination::forward("https://api.openai.com", Auth::PassThrough),
        // GitHub Copilot, from Copilot CLI or from OpenCode's supported Copilot login
        "github" | "github-copilot" => {
            Destination::tunnel(["api.github.com", "github.com", ".githubcopilot.com"])
        }
        // the two logins whose backend is not the public API: forwarded in the clear and
        // metered like any other, with a tunnel only for the token refresh
        // the ChatGPT backend refuses relayed requests (Cloudflare, and Codex speaks
        // websockets first), so a Codex login is a tunnel, like Copilot's: pinned egress,
        // no metering
        "chatgpt" if std::env::var_os("MEMORY_CHATGPT_FORWARD").is_some() => {
            Destination::forward("https://chatgpt.com/backend-api", Auth::PassThrough)
                .with_tunnels(["auth.openai.com"])
        }
        "chatgpt" => Destination::tunnel(["chatgpt.com", ".chatgpt.com", "auth.openai.com"]),
        // Gemini CLI's login is forwarded and metered through its endpoint override;
        // Antigravity CLI's has none, so its model host is tunnelled, pinned without
        // metering, as Copilot's is
        // a Muse login talks to the same host a key does, through --base-url, so it is
        // forwarded and metered, with the account's login host tunnelled for a refresh;
        // written from the docs and a run on a key, not from a login: the first launch
        // on a real login is the test
        "meta" => Destination::forward("https://api.meta.ai", Auth::PassThrough)
            .with_tunnels(["auth.meta.com"])
            .with_origin_paths(),
        "google" => Destination::forward("https://cloudcode-pa.googleapis.com", Auth::PassThrough)
            .with_tunnels([
                "oauth2.googleapis.com",
                "accounts.google.com",
                "cloudcode-pa.googleapis.com",
                "daily-cloudcode-pa.googleapis.com",
            ]),
        // a Grok Build login talks to Grok's cli-chat-proxy and websocket relays and
        // refreshes at auth.x.ai; a tunnel, pinned without metering, in this first
        // cut. The proxy has an override, GROK_CLI_CHAT_PROXY_BASE_URL, and speaks the
        // Responses API, so a forward with pass-through auth and tunnels only for the
        // websocket and login hosts is the metered route to prove on a real login
        "grok" => Destination::tunnel([
            "cli-chat-proxy.grok.com",
            "code.grok.com",
            "grok.com",
            ".grok.com",
            "auth.x.ai",
            "api.x.ai",
        ]),
        // Qwen's free OAuth tier ended in April 2026; the harness exits on it
        "dashscope" | "qwen" => {
            return Err(
                "qwen has no login mode: Qwen's free OAuth tier ended in April 2026; \
                        use --auth key with DASHSCOPE_API_KEY"
                    .into(),
            )
        }
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
    // through the gateway there is one destination, whatever the harness, and its
    // ceiling is the gateway's: the harness's base path and the egress's check agree
    let name = if auth == AuthMode::Gateway {
        "llmao".to_string()
    } else {
        provider.unwrap_or(default).to_string()
    };
    let mut harness_env = Vec::new();
    let (destination, ceiling_of) = match auth {
        AuthMode::Subscription => {
            if harness == Harness::Copilot {
                harness_env.push(("GH_TOKEN".to_string(), need("GH_TOKEN", "copilot")?));
            }
            let ceiling_of = if name == "github-copilot" {
                "github".to_string()
            } else {
                name.clone()
            };
            (pass_through_provider(&name)?, ceiling_of)
        }
        AuthMode::Key => (keyed_provider(&name)?, name.clone()),
        AuthMode::Gateway => {
            let key = need("LLMAO_KEY", "--auth gateway")?;
            let url = std::env::var("LLMAO_URL").unwrap_or_else(|_| "http://127.0.0.1:4000".into());
            // a gateway translates Anthropic's shape for other models, whose chat
            // templates refuse Claude Code's trailing system-role message
            // and Muse Code asks for 128k output tokens whatever the model's window,
            // so a cap that leaves a 131k-context model room for the prompt
            (
                Destination::forward(url, bearer(key))
                    .with_folded_system_messages()
                    .with_max_output_tokens(32_768),
                "llmao".to_string(),
            )
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
    /// The recall deputy, when the route gives the server a model of its own. Held
    /// here because the memory holds it weakly: it lives as long as the session.
    #[allow(dead_code)] // kept alive, not read
    deputy: Option<Arc<dyn Deputy>>,
    socket: PathBuf,
    home: PathBuf,
    /// Where the record is projected as files for the session, the launcher's own.
    projection: PathBuf,
    ceiling: Label,
    ceiling_of: String,
    provider: String,
    /// Where the memory's own calls go, and under what.
    server: ServerRoute,
    harness_env: Vec<(String, String)>,
    /// The model the run was given, for the adapters whose config names it.
    model: Option<String>,
    /// Whether the egress meters this route. A tunnel is pinned without metering, so
    /// the launcher reads what the harness reports about itself afterwards instead.
    metered: bool,
}

/// `projected` says whether this invocation mounts the projection for a harness; a tool
/// run on the host, `recall` or `golden` say, mounts nothing and renders nothing.
#[allow(clippy::too_many_arguments)] // one argument per thing a launch is given
async fn prepare_launch(
    root: &Path,
    harness: Harness,
    creds: Credentials<'_>,
    socket: Option<PathBuf>,
    recall_model: Option<&str>,
    projected: bool,
) -> Result<Prepared, Box<dyn std::error::Error>> {
    let auth = creds.auth;
    let route = route_for(harness, auth, creds.provider)?;
    let (server, extra) = server_route(&route, auth, creds.server_auth, creds.server_provider)?;
    let Route {
        name,
        destination,
        ceiling_of,
        harness_env,
    } = route;
    let memory = Memory::open(root)?.with_author(format!("agent:{}", harness.binary()));
    let ceiling = memory.grants().ceiling(&ceiling_of).ok_or_else(|| {
        format!("grants.toml has no [destinations.{ceiling_of}]; add one with a ceiling")
    })?;
    let home = root.join("state/sandbox").join(harness.binary());
    let projection = root.join(dm_sandbox::PROJECTION).join(harness.binary());
    let memory = memory.with_handling_cap(ceiling);
    let memory = Arc::new(if projected {
        memory.with_projection(projection.clone())
    } else {
        memory
    });
    let metered = !destination.upstream.is_empty();
    let mut destinations = HashMap::new();
    destinations.insert(name.clone(), destination);
    if let Some(extra) = extra {
        destinations.insert(extra.name, extra.destination);
    }
    let egress = Egress::new(
        memory.clone(),
        destinations,
        &root.join("state/telemetry.db"),
    )?;
    // a key or the gateway is the launcher's credential, so the server may spend a
    // small model on recall; a subscription login is the harness's alone
    let deputy: Option<Arc<dyn Deputy>> = (server.auth != AuthMode::Subscription).then(|| {
        Arc::new(deputy::EgressDeputy::new(
            egress.clone(),
            &server.provider,
            recall_model,
        )) as Arc<dyn Deputy>
    });
    if let Some(d) = &deputy {
        memory.set_deputy(d);
    }
    let run_dir = root.join("state/run");
    std::fs::create_dir_all(&run_dir)?;
    Ok(Prepared {
        memory,
        egress,
        deputy,
        socket: socket
            .unwrap_or_else(|| run_dir.join(format!("egress-{}.sock", std::process::id()))),
        root: root.to_path_buf(),
        home,
        projection,
        ceiling,
        ceiling_of,
        provider: name,
        server,
        harness_env,
        model: None,
        metered,
    })
}

/// What a tunnelled harness reported about itself, read from the transcript it left and
/// recorded beside what the egress observed, marked as the harness's own count.
fn record_reported_usage(p: &Prepared) {
    let Some(t) = mine::newest_transcript(&p.home) else {
        return;
    };
    let Ok(text) = std::fs::read_to_string(&t) else {
        return;
    };
    let reported = harness_usage::from_transcript(&text);
    for usage in reported.by_model.values() {
        if let Err(e) =
            p.egress
                .telemetry()
                .record_reported(p.memory.session_id(), &p.provider, usage)
        {
            eprintln!("memory: could not record reported usage: {e}");
        }
    }
    if let Some((pct, window)) = reported.plan_used_percent {
        eprintln!(
            "memory: the harness reports {pct:.1}% of its plan's {}-hour window used",
            window / 60
        );
    }
}

/// `memory usage`: the telemetry by destination, model and who counted.
fn print_usage(rows: &[dm_egress::telemetry::Totals]) {
    if rows.is_empty() {
        println!("no model calls recorded");
        return;
    }
    println!(
        "{:<12} {:<28} {:<8} {:>6} {:>10} {:>10} {:>10} {:>10}",
        "destination", "model", "source", "calls", "input", "output", "cache_rd", "cache_wr"
    );
    for r in rows {
        println!(
            "{:<12} {:<28} {:<8} {:>6} {:>10} {:>10} {:>10} {:>10}",
            r.destination,
            r.model,
            r.source,
            r.calls,
            r.input_tokens,
            r.output_tokens,
            r.cache_read_tokens,
            r.cache_write_tokens
        );
    }
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
            "  {:<10} {:<26} {:<8} calls {:>4}  in {:>8}  out {:>8}  cache {:>8}",
            r.destination,
            r.model,
            r.source,
            r.calls,
            r.input_tokens,
            r.output_tokens,
            r.cache_read_tokens
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
    p.memory.project_into(&p.projection).await?;
    let mut plan = dm_sandbox::prepare(&dm_sandbox::Request {
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
        model: p.model.as_deref(),
        image: false,
        workdir_inside: None,
        rootfs,
        projection: Some(&p.projection),
    })?;
    // the hook frees a recall page of this session only
    plan.env
        .insert("MEMORY_SESSION".into(), p.memory.session_id().to_string());
    if std::env::var_os("MEMORY_EGRESS_DEBUG").is_some() {
        eprintln!("launch debug: argv {:?}", plan.argv);
    }
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

/// `web`: the web terminal.
async fn web_cmd(root: &Path, command: Command) -> Fallible {
    let Command::Web {
        listen,
        harness,
        auth,
        backend,
        workdir,
    } = command
    else {
        unreachable!("routed by run_cli")
    };
    let backend_name = match backend {
        BackendArg::Auto => "auto",
        BackendArg::Namespaces => "namespaces",
        BackendArg::Image => "image",
        BackendArg::Wslc => "wslc",
        BackendArg::None => "none",
    };
    let config = dm_web::Config {
        root: root.to_path_buf(),
        memory_binary: std::env::current_exe()?,
        workdir: resolve_workdir(root, workdir)?,
        harness: harness.map(|h| Harness::from(h).binary().to_string()),
        auth: match AuthMode::from(auth) {
            AuthMode::Subscription => "subscription",
            AuthMode::Key => "key",
            AuthMode::Gateway => "gateway",
        }
        .to_string(),
        backend: backend_name.to_string(),
        command_override: None,
    };
    dm_web::serve(config, &listen).await?;
    Ok(())
}

/// The auth mode a deputy may run under. A subscription login is the harness's alone,
/// so naming a provider with it would quietly give no deputy; refuse it instead.
fn deputy_auth(auth: AuthArg) -> Result<AuthMode, Box<dyn std::error::Error>> {
    match AuthMode::from(auth) {
        AuthMode::Subscription => Err("a subscription login is the harness's alone and cannot \
             stand in for the deputy; use --auth key or --auth gateway with --provider"
            .into()),
        mode => Ok(mode),
    }
}

/// `golden`: the suite against this root, with the deputy's brief when a provider is
/// named.
async fn golden_cmd(
    root: &Path,
    suite: &Path,
    k: usize,
    as_json: bool,
    provider: Option<String>,
    auth: AuthArg,
    model: Option<&str>,
) -> Fallible {
    let suite = golden::Suite::load(suite)?;
    let r = match provider {
        Some(provider) => {
            let p = prepare_launch(
                root,
                Harness::Command,
                Credentials::keyed(deputy_auth(auth)?, &provider),
                None,
                model,
                false,
            )
            .await?;
            // the suite's reads are a measurement, not a session's
            p.memory.disable_read_log();
            golden::score(&p.memory, &suite, k).await?
        }
        None => golden::score(&Memory::open(root)?.without_read_log(), &suite, k).await?,
    };
    if as_json {
        println!("{}", serde_json::to_string_pretty(&r)?);
    } else {
        print!("{}", golden::render(&r));
    }
    Ok(())
}

/// `tap install` and `tap run`.
async fn tap_cmd(root: &Path, op: TapOp) -> Fallible {
    match op {
        TapOp::Install { name, file } => {
            let dest = taps::install(root, &name, &file)?;
            println!("{}", dest.display());
            Ok(())
        }
        TapOp::Run {
            name,
            config,
            every,
        } => taps::run(root, &name, config.as_deref(), every).await,
    }
}

/// `mine` and the C API check.
/// `memory mine`: a transcript read by a small model through the session's destination.
async fn mine_cmd(
    root: &Path,
    transcript: &Path,
    auth: AuthArg,
    provider: &str,
    model: Option<String>,
) -> Fallible {
    let p = prepare_launch(
        root,
        Harness::Command,
        Credentials::keyed(auth.into(), provider),
        None,
        None,
        false,
    )
    .await?;
    let client = work::client(p.memory.clone()).await?;
    let model = model.unwrap_or_else(|| mine::default_model(&p.provider).to_string());
    let ids = mine::mine(&p.egress, &client, &p.provider, &model, transcript).await?;
    println!("{}", serde_json::to_string_pretty(&ids)?);
    Ok(())
}

/// `memory models`: the destination's own model list, as the web form's picker reads it.
async fn models_cmd(root: &Path, auth: AuthArg, provider: Option<String>) -> Fallible {
    let auth = AuthMode::from(auth);
    if auth == AuthMode::Subscription {
        return Err("a login is the harness's own and lists no models here; name the model, or leave it to the harness".into());
    }
    let provider = provider.unwrap_or_else(|| {
        if auth == AuthMode::Gateway {
            "llmao".to_string()
        } else {
            "openai".to_string()
        }
    });
    let p = prepare_launch(
        root,
        Harness::Command,
        Credentials::keyed(auth, &provider),
        None,
        None,
        false,
    )
    .await?;
    // Google lists under v1beta; everyone else speaks OpenAI's list, Anthropic included
    let path = if matches!(p.provider.as_str(), "gemini" | "google") {
        "v1beta/models"
    } else {
        "v1/models"
    };
    let v = p.egress.get(&p.provider, path).await?;
    let models = model_ids(&v);
    println!(
        "{}",
        serde_json::json!({ "destination": p.provider, "models": models })
    );
    Ok(())
}

/// Model ids out of a listing in OpenAI's shape (`data[].id`), Google's
/// (`models[].name`, prefixed `models/`) or a plain `models[].id`.
fn model_ids(v: &serde_json::Value) -> Vec<String> {
    let items = v
        .get("data")
        .or_else(|| v.get("models"))
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut ids: Vec<String> = items
        .iter()
        .filter_map(|m| {
            m.get("id")
                .or_else(|| m.get("name"))
                .and_then(serde_json::Value::as_str)
                .map(|s| s.trim_start_matches("models/").to_string())
        })
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

async fn tools_cmd(root: &Path, command: Command) -> Fallible {
    match command {
        Command::Models { auth, provider } => models_cmd(root, auth, provider).await,
        Command::Mine {
            transcript,
            auth,
            provider,
            model,
        } => mine_cmd(root, &transcript, auth, &provider, model).await,
        Command::Recall {
            question,
            domain,
            depth,
            provider: Some(provider),
            auth,
            model,
        } => {
            let p = prepare_launch(
                root,
                Harness::Command,
                Credentials::keyed(deputy_auth(auth)?, &provider),
                None,
                model.as_deref(),
                false,
            )
            .await?;
            let r = p
                .memory
                .recall(
                    &question,
                    domain.as_deref(),
                    depth.map(Into::into),
                    Budget::default(),
                )
                .await?;
            json(&recall::to_json(&r))
        }
        Command::WslcApiCheck {
            image,
            wslc_storage,
            refresh,
        } => wslc_api_check(root, &image, wslc_storage.as_deref(), refresh),
        Command::Golden {
            suite,
            k,
            json: as_json,
            provider,
            auth,
            model,
        } => golden_cmd(root, &suite, k, as_json, provider, auth, model.as_deref()).await,
        Command::Savings {
            query,
            json: as_json,
        } => {
            let r = savings::measure(root, query.as_deref()).await?;
            if as_json {
                println!("{}", serde_json::to_string_pretty(&r)?);
            } else {
                print!("{}", savings::render(&r));
            }
            Ok(())
        }
        _ => unreachable!("routed by run_cli"),
    }
}

/// `wslc-api-check`: the C API end to end, on Windows.
fn wslc_api_check(root: &Path, image: &str, storage: Option<&Path>, refresh: bool) -> Fallible {
    #[cfg(windows)]
    {
        let storage = wslc_api::storage_for(root, storage)?;
        println!("{}", wslc_api::check(&storage, image, refresh)?);
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = (root, image, storage, refresh);
        Err(dm_wslc_api::WINDOWS_ONLY.into())
    }
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

/// A `launch` the `wslc` backend runs, when that is where it runs: the egress container
/// is told what to run, and everything else happens inside it.
fn wslc_for_launch(
    root: &Path,
    command: &Command,
) -> Result<Option<wslc::Launch>, Box<dyn std::error::Error>> {
    let Command::Launch {
        harness,
        auth,
        provider,
        server_auth,
        server_provider,
        workdir,
        unconfined,
        backend,
        image,
        wslc_via,
        wslc_storage,
        prompt,
        prompt_file,
        model,
        subagents,
        no_handoff,
        args,
        ..
    } = command
    else {
        return Ok(None);
    };
    if backend_of(*backend, *unconfined, root) != Backend::Wslc {
        return Ok(None);
    }
    let mut inside_args = run_options_args(model.as_deref(), *subagents);
    inside_args.extend(inside_args_for_launch(
        prompt.as_deref(),
        prompt_file.as_deref(),
        *no_handoff,
        args,
    )?);
    Ok(Some(wslc::Launch {
        root: root.to_path_buf(),
        harness: (*harness).into(),
        auth: (*auth).into(),
        provider: provider.clone(),
        server_auth: server_auth.map(AuthMode::from),
        server_provider: server_provider.clone(),
        workdir: resolve_workdir(root, workdir.clone())?,
        image: image.clone(),
        inside_args,
        interactive: prompt.is_none() && prompt_file.is_none(),
        via: *wslc_via,
        storage: wslc_storage.clone(),
    }))
}

/// A `work` the `wslc` backend runs, when that is where it runs.
fn wslc_for_work(
    root: &Path,
    command: &Command,
) -> Result<Option<wslc::Launch>, Box<dyn std::error::Error>> {
    let Command::Work {
        harness,
        auth,
        provider,
        server_auth,
        server_provider,
        workdir,
        unconfined,
        backend,
        image,
        wslc_via,
        wslc_storage,
        domain,
        lease_seconds,
        keep_going,
        idle_seconds,
        model,
        subagents,
        ..
    } = command
    else {
        return Ok(None);
    };
    if backend_of(*backend, *unconfined, root) != Backend::Wslc {
        return Ok(None);
    }
    Ok(Some(wslc::Launch {
        root: root.to_path_buf(),
        harness: (*harness).into(),
        auth: (*auth).into(),
        provider: provider.clone(),
        server_auth: server_auth.map(AuthMode::from),
        server_provider: server_provider.clone(),
        workdir: resolve_workdir(root, workdir.clone())?,
        image: image.clone(),
        inside_args: work_inside_args(
            run_options_args(model.as_deref(), *subagents),
            domain.as_deref(),
            *lease_seconds,
            *keep_going,
            *idle_seconds,
        ),
        interactive: false,
        via: *wslc_via,
        storage: wslc_storage.clone(),
    }))
}

async fn cmd_launch(root: &Path, command: Command) -> Fallible {
    if let Some(l) = wslc_for_launch(root, &command)? {
        std::process::exit(wslc::run(&l)?);
    }
    let Command::Launch {
        harness,
        auth,
        provider,
        server_auth,
        server_provider,
        workdir,
        unconfined,
        backend,
        prompt,
        prompt_file,
        model,
        subagents,
        no_handoff,
        mine: mine_after,
        mine_model,
        mine_via,
        args,
        ..
    } = command
    else {
        unreachable!("routed by launch_cmd")
    };
    let workdir = resolve_workdir(root, workdir)?;
    let prompt = match (prompt, prompt_file) {
        (Some(p), _) => Some(p),
        (None, Some(f)) => Some(std::fs::read_to_string(&f)?),
        (None, None) => None,
    };
    let (program, args) = split_program(harness.into(), args);
    let session = Session::start(
        root,
        harness.into(),
        Credentials {
            auth: auth.into(),
            provider: provider.as_deref(),
            server_auth: server_auth.map(AuthMode::from),
            server_provider: server_provider.as_deref(),
        },
        workdir,
        runner_for(backend_of(backend, unconfined, root), root)?,
        RunOptions {
            model,
            subagents,
            program,
        },
    )
    .await?;
    let code = match prompt {
        Some(prompt) => work::prompted(&session, &prompt, &args, no_handoff).await?,
        None => work::interactive(&session, &args).await?,
    };
    if mine_after {
        session.mine(mine_model.as_deref(), mine_via).await?;
    }
    session.finish(code);
    std::process::exit(code);
}

/// The `memory inside` flags for a task run, after the run options.
fn work_inside_args(
    mut args: Vec<String>,
    domain: Option<&str>,
    lease_seconds: i64,
    keep_going: bool,
    idle_seconds: u64,
) -> Vec<String> {
    args.push("--work".to_string());
    if let Some(d) = domain {
        args.extend(["--domain".to_string(), d.to_string()]);
    }
    args.extend(["--lease-seconds".to_string(), lease_seconds.to_string()]);
    if keep_going {
        args.push("--loop".into());
    }
    args.extend(["--idle-seconds".to_string(), idle_seconds.to_string()]);
    args
}

async fn cmd_work(root: &Path, command: Command) -> Fallible {
    if let Some(l) = wslc_for_work(root, &command)? {
        std::process::exit(wslc::run(&l)?);
    }
    let Command::Work {
        harness,
        auth,
        provider,
        server_auth,
        server_provider,
        workdir,
        unconfined,
        backend,
        domain,
        lease_seconds,
        keep_going,
        idle_seconds,
        model,
        subagents,
        mine: mine_after,
        mine_model,
        mine_via,
        ..
    } = command
    else {
        unreachable!("routed by launch_cmd")
    };
    let workdir = resolve_workdir(root, workdir)?;
    let session = Session::start(
        root,
        harness.into(),
        Credentials {
            auth: auth.into(),
            provider: provider.as_deref(),
            server_auth: server_auth.map(AuthMode::from),
            server_provider: server_provider.as_deref(),
        },
        workdir,
        runner_for(backend_of(backend, unconfined, root), root)?,
        RunOptions {
            model,
            subagents,
            program: Vec::new(),
        },
    )
    .await?;
    let options = work::Options {
        domain,
        lease_seconds,
        keep_going,
        idle: std::time::Duration::from_secs(idle_seconds),
        mine: mine_after.then(|| work::MinePlan {
            model: mine_model,
            via_harness: session.mines_via_harness(mine_via),
        }),
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
        server_auth,
        server_provider,
        requests,
        host_home,
        workdir_inside,
        prompt,
        prompt_file,
        no_handoff,
        work,
        model,
        subagents,
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
    let (program, args) = split_program(harness.into(), args);
    let session = Session::start(
        root,
        harness.into(),
        Credentials {
            auth: auth.into(),
            provider: provider.as_deref(),
            server_auth: server_auth.map(AuthMode::from),
            server_provider: server_provider.as_deref(),
        },
        PathBuf::from("/"),
        runner,
        RunOptions {
            model,
            subagents,
            program,
        },
    )
    .await?;
    let code = if work {
        let options = work::Options {
            domain,
            lease_seconds,
            keep_going,
            idle: std::time::Duration::from_secs(idle_seconds),
            mine: None,
        };
        work::run(&session, &options).await?
    } else {
        let prompt = match (prompt, prompt_file) {
            (Some(p), _) => Some(p),
            (None, Some(f)) => Some(std::fs::read_to_string(&f)?),
            (None, None) => None,
        };
        match prompt {
            Some(prompt) => work::prompted(&session, &prompt, &args, no_handoff).await?,
            None => work::interactive(&session, &args).await?,
        }
    };
    session.finish(code);
    std::process::exit(code);
}

/// A command harness's program is fixed for the session and everything else is a run's
/// arguments; for the other harnesses all of it is the run's.
/// The run options as arguments for the `inside` process of the `wslc` backend.
fn run_options_args(model: Option<&str>, subagents: SubagentsArg) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(m) = model {
        out.extend(["--model".to_string(), m.to_string()]);
    }
    if subagents == SubagentsArg::Board {
        out.extend(["--subagents".to_string(), "board".to_string()]);
    }
    out
}

/// What a run is given beyond the harness and its auth: a model, where subagents go,
/// and, for a `command` harness, the program and its fixed arguments.
#[derive(Clone, Debug, Default)]
struct RunOptions {
    model: Option<String>,
    subagents: SubagentsArg,
    program: Vec<String>,
}

/// The environment the run options add: the board for subagents, and for Claude Code
/// the model as every role's default, since it picks a model per role and a gateway
/// that serves one model answers only for that one.
fn apply_run_options(p: &mut Prepared, harness: Harness, auth: AuthMode, options: &RunOptions) {
    p.model.clone_from(&options.model);
    if harness == Harness::Claude && auth == AuthMode::Gateway {
        // Claude Code asks for `high` effort by default, which only Anthropic's models
        // take; a gateway's open models take low, medium and xhigh, so medium goes in
        // the environment, and `-- --effort <level>` still overrides it
        p.harness_env
            .push(("CLAUDE_CODE_EFFORT_LEVEL".to_string(), "medium".to_string()));
        // Claude Code assumes a 200k window and compacts too late for the open models
        // behind a gateway; llmao's own notes say 120k against a 131k window
        p.harness_env.push((
            "CLAUDE_CODE_MAX_CONTEXT_TOKENS".to_string(),
            "120000".to_string(),
        ));
    }
    if options.subagents == SubagentsArg::Board {
        // the pre-tool hook files a subagent call on the board and refuses it
        p.harness_env
            .push(("MEMORY_SUBAGENTS".to_string(), "board".to_string()));
    }
    if let (Some(m), Harness::Claude) = (&options.model, harness) {
        for k in [
            "ANTHROPIC_MODEL",
            "ANTHROPIC_DEFAULT_OPUS_MODEL",
            "ANTHROPIC_DEFAULT_SONNET_MODEL",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL",
            "ANTHROPIC_SMALL_FAST_MODEL",
            "CLAUDE_CODE_SUBAGENT_MODEL",
        ] {
            p.harness_env.push((k.to_string(), m.clone()));
        }
    }
}

fn split_program(harness: Harness, args: Vec<String>) -> (Vec<String>, Vec<String>) {
    if harness == Harness::Command {
        (args, Vec::new())
    } else {
        (Vec::new(), args)
    }
}

/// The flags `memory inside` gets for a launch handed to the wslc backend.
fn inside_args_for_launch(
    prompt: Option<&str>,
    prompt_file: Option<&Path>,
    no_handoff: bool,
    args: &[String],
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let mut inside_args = Vec::new();
    if let Some(p) = prompt {
        inside_args.extend(["--prompt".to_string(), p.to_string()]);
    }
    if let Some(f) = prompt_file {
        inside_args.extend(["--prompt".to_string(), std::fs::read_to_string(f)?]);
    }
    if no_handoff {
        inside_args.push("--no-handoff".into());
    }
    inside_args.push("--".into());
    inside_args.extend(args.iter().cloned());
    Ok(inside_args)
}

/// The working tree, canonical. A tree that holds the record root, or sits inside it,
/// is refused: the tree is mounted read-write, and the root, its record, its state and
/// its projection are the launcher's alone.
fn resolve_workdir(
    root: &Path,
    workdir: Option<PathBuf>,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let w = match workdir {
        Some(w) => w,
        None => std::env::current_dir()?,
    };
    let w = std::fs::canonicalize(&w)?;
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    if root.starts_with(&w) || w.starts_with(&root) {
        return Err(format!(
            "the working tree {} and the record root {} overlap; run from another \
             directory or pass --workdir",
            w.display(),
            root.display()
        )
        .into());
    }
    Ok(w)
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
    /// For a `command` harness: the program and its fixed arguments, which every run
    /// starts with; a prompt or task comes after them.
    program: Vec<String>,
    /// The model, in the harness's own flag, after every run's arguments: a flag after
    /// the prompt is where every harness, subcommand or not, accepts one.
    model: Option<String>,
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
        creds: Credentials<'_>,
        workdir: PathBuf,
        mut runner: Runner,
        options: RunOptions,
    ) -> Result<Session, Box<dyn std::error::Error>> {
        let socket = match &runner {
            Runner::Requests(r) => Some(r.dir.join("egress.sock")),
            _ => None,
        };
        let auth = creds.auth;
        let mut p = prepare_launch(root, harness, creds, socket, None, true).await?;
        apply_run_options(&mut p, harness, auth, &options);

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
            program: options.program,
            model: options.model,
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

    /// Where the harness's hooks leave notes.
    pub fn outbox(&self) -> PathBuf {
        self.p.home.join(hooks::OUTBOX)
    }

    /// The destination the session's model calls go to.
    pub fn provider(&self) -> String {
        self.p.provider.clone()
    }

    /// The egress, for the launcher's own calls.
    pub fn egress(&self) -> &Egress {
        &self.p.egress
    }

    /// The private home, where the harness leaves its transcript.
    pub fn home(&self) -> &Path {
        &self.p.home
    }

    /// Whether mining goes through the harness: always on a subscription login, since
    /// that credential belongs to the harness and may be used by nothing else; by choice
    /// otherwise.
    pub fn mines_via_harness(&self, via: MineVia) -> bool {
        via_harness(via, self.p.server.auth)
    }

    /// Mine the newest transcript in the private home, if there is one. Through the
    /// harness on a subscription login: a prompted, confined run of the harness itself on
    /// a small model, whose printed answer is the proposals. Through the egress otherwise.
    ///
    /// # Errors
    ///
    /// When the model call, the harness run, or a write fails.
    pub async fn mine(
        &self,
        model: Option<&str>,
        via: MineVia,
    ) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        let Some(t) = mine::newest_transcript(&self.p.home) else {
            eprintln!("memory: nothing to mine; no transcript under the private home");
            return Ok(Vec::new());
        };
        let model = model.map_or_else(
            || mine::default_model(&self.p.server.provider).to_string(),
            str::to_string,
        );
        let client = work::client(self.p.memory.clone()).await?;
        let ids = if self.mines_via_harness(via) {
            self.mine_via_harness(&client, &model, &t).await?
        } else {
            mine::mine(&self.p.egress, &client, &self.p.server.provider, &model, &t).await?
        };
        eprintln!("memory: mined {} entries from {}", ids.len(), t.display());
        client.close().await.ok();
        Ok(ids)
    }

    async fn mine_via_harness(
        &self,
        client: &dm_client::MemoryClient,
        model: &str,
        transcript: &Path,
    ) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        let Some(excerpt) = mine::excerpt_of(transcript)? else {
            return Ok(Vec::new());
        };
        let mut args = self.harness.headless(&mine::prompt_for(&excerpt));
        args.extend(self.harness.model_flag(model));
        let (code, output) = self.run(&args, true).await?;
        // the mining run is not a session anyone continues: its own hook notes go
        let _ = std::fs::remove_dir_all(self.outbox());
        if code != 0 {
            return Err(format!("the mining run exited {code}").into());
        }
        let found = mine::proposals(&output.unwrap_or_default());
        mine::write_proposals(client, &found, transcript, model).await
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
        let mut full: Vec<String> = [self.program.as_slice(), args].concat();
        if let Some(m) = &self.model {
            // OpenCode names a model by its provider, which here is the destination
            let m = if self.harness == Harness::OpenCode && !m.contains('/') {
                format!("{}/{m}", self.p.provider)
            } else {
                m.clone()
            };
            full.extend(self.harness.model_flag(&m));
        }
        let args = full.as_slice();
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
        self.p.memory.project_into(&self.p.projection).await?;
        let mut plan = dm_sandbox::prepare(&dm_sandbox::Request {
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
            model: self.p.model.as_deref(),
            image: true,
            workdir_inside: Some(&r.workdir_inside),
            rootfs: None,
            projection: Some(&self.p.projection),
        })?;
        plan.env.insert(
            "MEMORY_SESSION".into(),
            self.p.memory.session_id().to_string(),
        );
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
        if !self.p.metered {
            record_reported_usage(&self.p);
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
            cmd.env("MEMORY_OUTBOX", home.join(hooks::OUTBOX));
        }
        Harness::Gemini
        | Harness::Antigravity
        | Harness::Qwen
        | Harness::Grok
        | Harness::Muse
        | Harness::Copilot
        | Harness::OpenCode
        | Harness::Command => {
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
        cmd @ (Command::Search { .. } | Command::Recall { .. }) => retrieve(m, cmd).await,
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

/// `search` and `recall`: the retrieval operations, one shape of JSON each.
async fn retrieve(m: &Memory, command: Command) -> Fallible {
    match command {
        Command::Search {
            query,
            domain,
            limit,
            depth: DepthArg::Sections,
        } => {
            let pack = m.search_sections(&query, domain.as_deref(), limit).await?;
            json(&serde_json::json!({
                "hits": pack.pieces,
                "truncated": pack.truncated,
                "tokens": pack.tokens,
            }))
        }
        Command::Search {
            query,
            domain,
            limit,
            depth: DepthArg::Summaries,
        } => json(&hits_json(
            &m.search(&query, domain.as_deref(), limit).await?,
        )),
        Command::Recall {
            question,
            domain,
            depth,
            ..
        } => {
            let r = m
                .recall(
                    &question,
                    domain.as_deref(),
                    depth.map(Into::into),
                    Budget::default(),
                )
                .await?;
            json(&recall::to_json(&r))
        }
        _ => unreachable!("only search and recall are routed here"),
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
                "imported {} entries, skipped {} already present, refused {} erased here",
                im.written.len(),
                im.skipped.len(),
                im.erased.len()
            );
            Ok(())
        }
        Command::Reindex => {
            let (n, events) = m.reindex().await?;
            println!("indexed {n} entries, replayed {events} ledger events");
            Ok(())
        }
        Command::Erase { id, reason } => json(&m.erase(&id, reason.into()).await?),
        Command::Init { .. }
        | Command::Serve { .. }
        | Command::Launch { .. }
        | Command::Work { .. }
        | Command::Inside { .. }
        | Command::Image { .. }
        | Command::Web { .. }
        | Command::Mine { .. }
        | Command::Models { .. }
        | Command::WslcApiCheck { .. }
        | Command::Savings { .. }
        | Command::Golden { .. }
        | Command::Hook { .. }
        | Command::Relay { .. }
        | Command::Usage
        | Command::Task { .. }
        | Command::Tap { .. }
        | Command::Write { .. }
        | Command::Search { .. }
        | Command::Recall { .. }
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

#[cfg(test)]
mod backend_tests {
    use super::*;

    #[test]
    fn model_ids_are_read_from_either_listing_shape() {
        let openai = serde_json::json!({ "object": "list", "data": [{ "id": "b" }, { "id": "a" }, { "id": "a" }] });
        assert_eq!(model_ids(&openai), vec!["a", "b"]);
        let google = serde_json::json!({ "models": [{ "name": "models/gemini-2.5-flash" }] });
        assert_eq!(model_ids(&google), vec!["gemini-2.5-flash"]);
        assert!(model_ids(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn run_options_cross_to_the_inside_process_as_flags() {
        assert!(run_options_args(None, SubagentsArg::Harness).is_empty());
        assert_eq!(
            run_options_args(Some("qwen3.8-27b"), SubagentsArg::Board),
            vec!["--model", "qwen3.8-27b", "--subagents", "board"]
        );
        assert_eq!(SubagentsArg::default(), SubagentsArg::Harness);
    }

    #[test]
    fn the_working_tree_and_the_root_may_not_overlap() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("memory");
        std::fs::create_dir_all(root.join("state/projection/claude")).unwrap();
        let beside = dir.path().join("proj");
        std::fs::create_dir_all(&beside).unwrap();
        assert!(resolve_workdir(&root, Some(beside)).is_ok(), "beside: fine");
        for inside in ["state", "state/projection/claude", "."] {
            let err = resolve_workdir(&root, Some(root.join(inside)))
                .unwrap_err()
                .to_string();
            assert!(err.contains("overlap"), "{inside}: {err}");
        }
        let err = resolve_workdir(&root, Some(dir.path().to_path_buf()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("overlap"), "a tree holding the root: {err}");
    }

    /// Set these for a test and put back what was there, so a developer's own keys
    /// survive the run.
    fn with_env<T>(vars: &[(&str, &str)], f: impl FnOnce() -> T) -> T {
        let kept: Vec<(&str, Option<String>)> = vars
            .iter()
            .map(|(k, _)| (*k, std::env::var(k).ok()))
            .collect();
        for (k, v) in vars {
            std::env::set_var(k, v);
        }
        let out = f();
        for (k, v) in kept {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        out
    }

    #[test]
    fn the_server_s_own_calls_follow_the_harness_unless_given_a_route() {
        // the harness on a personal login: its route is pass-through, named for the provider
        let harness = route_for(Harness::Claude, AuthMode::Subscription, None).unwrap();
        assert_eq!(harness.name, "anthropic");
        // nothing asked: the server's calls follow the harness, so a login gives no deputy
        let (server, extra) = server_route(&harness, AuthMode::Subscription, None, None).unwrap();
        assert_eq!(
            (server.auth, server.provider.as_str()),
            (AuthMode::Subscription, "anthropic")
        );
        assert!(extra.is_none());
        // a login cannot stand in for the server's calls
        let e = server_route(
            &harness,
            AuthMode::Subscription,
            Some(AuthMode::Subscription),
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("harness's alone"), "{e}");
        // the same key route as the harness's is the harness's, registered once
        with_env(&[("OPENAI_API_KEY", "k")], || {
            let keyed = route_for(Harness::Codex, AuthMode::Key, Some("openai")).unwrap();
            let (server, extra) =
                server_route(&keyed, AuthMode::Key, Some(AuthMode::Key), Some("openai")).unwrap();
            assert_eq!(
                (server.auth, server.provider.as_str()),
                (AuthMode::Key, "openai")
            );
            assert!(extra.is_none());
        });
        // mining goes through the harness only when the server's calls have no
        // credential of their own
        assert!(via_harness(MineVia::Auto, AuthMode::Subscription));
        assert!(!via_harness(MineVia::Auto, AuthMode::Gateway));
        assert!(via_harness(MineVia::Harness, AuthMode::Gateway));
        assert!(!via_harness(MineVia::Api, AuthMode::Subscription));
    }

    #[test]
    fn the_server_s_gateway_route_is_named_for_the_gateway_and_a_clash_is_refused() {
        let harness = route_for(Harness::Claude, AuthMode::Subscription, None).unwrap();
        with_env(&[("LLMAO_KEY", "pat"), ("ANTHROPIC_API_KEY", "k")], || {
            // the gateway: a route of its own, named for the gateway so it never shares
            // the harness's name, with the gateway's ceiling
            let (server, extra) = server_route(
                &harness,
                AuthMode::Subscription,
                Some(AuthMode::Gateway),
                None,
            )
            .unwrap();
            assert_eq!(
                (server.auth, server.provider.as_str()),
                (AuthMode::Gateway, "llmao")
            );
            let extra = extra.expect("a destination to register");
            assert_eq!(
                (extra.name.as_str(), extra.ceiling_of.as_str()),
                ("llmao", "llmao")
            );
            assert_eq!(extra.destination.upstream, "http://127.0.0.1:4000");
            // it takes no provider name
            let e = server_route(
                &harness,
                AuthMode::Subscription,
                Some(AuthMode::Gateway),
                Some("openai"),
            )
            .unwrap_err()
            .to_string();
            assert!(e.contains("takes no name"), "{e}");
            // a key to the provider the harness's login already names would collide
            // under one name with a different destination; refused rather than guessed
            let e = server_route(
                &harness,
                AuthMode::Subscription,
                Some(AuthMode::Key),
                Some("anthropic"),
            )
            .unwrap_err()
            .to_string();
            assert!(
                e.contains("anthropic") && e.contains("--server-provider"),
                "{e}"
            );
        });
    }

    #[test]
    fn a_grok_login_is_a_tunnel_to_grok_s_own_hosts() {
        // a login talks to the cli-chat-proxy and websocket relays on grok.com, and
        // refreshes at auth.x.ai: a tunnel in this first cut, pinned without metering
        let d = pass_through_provider("grok").unwrap();
        assert!(d.upstream.is_empty(), "a tunnel, not a forward");
        for host in [
            "cli-chat-proxy.grok.com",
            "code.grok.com",
            "grok.com",
            "auth.x.ai",
        ] {
            assert!(
                d.tunnel_hosts.iter().any(|h| h == host),
                "{host}: {:?}",
                d.tunnel_hosts
            );
        }
    }

    #[test]
    fn dashscope_is_a_keyed_forward_and_has_no_login_mode() {
        // the DashScope compatible endpoint, keyed from DASHSCOPE_API_KEY; the harness
        // appends /v1 to it, so the base ends at compatible-mode
        let err = keyed_provider("dashscope").err().map(|e| e.to_string());
        if let Some(e) = &err {
            assert!(e.contains("DASHSCOPE_API_KEY"), "{e}");
        }
        std::env::set_var("DASHSCOPE_API_KEY", "k");
        let d = keyed_provider("dashscope").unwrap();
        std::env::remove_var("DASHSCOPE_API_KEY");
        assert_eq!(
            d.upstream,
            "https://dashscope-intl.aliyuncs.com/compatible-mode"
        );
        assert!(d.tunnel_hosts.is_empty(), "a forward, metered");
        // Qwen OAuth ended in April 2026: no subscription destination, under either name
        for name in ["dashscope", "qwen"] {
            let e = pass_through_provider(name).unwrap_err().to_string();
            assert!(e.contains("DASHSCOPE_API_KEY"), "{name}: {e}");
        }
        assert_eq!(crate::model::default_model("dashscope"), "qwen-flash");
    }

    #[test]
    fn an_empty_variable_is_a_missing_one() {
        assert_eq!(present(Some("k".into())).as_deref(), Some("k"));
        assert_eq!(present(Some(String::new())), None);
        assert_eq!(present(Some("  ".into())), None);
        assert_eq!(present(None), None);
    }

    #[test]
    fn unconfined_wins_and_auto_prefers_the_image_when_unpacked() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            backend_of(BackendArg::Wslc, true, dir.path()),
            Backend::None
        );
        assert_eq!(
            backend_of(BackendArg::Wslc, false, dir.path()),
            Backend::Wslc
        );
        assert_eq!(
            backend_of(BackendArg::Image, false, dir.path()),
            Backend::Image
        );
        assert_eq!(
            backend_of(BackendArg::None, false, dir.path()),
            Backend::None
        );
        assert_eq!(
            backend_of(BackendArg::Namespaces, false, dir.path()),
            Backend::Namespaces
        );
        let auto = backend_of(BackendArg::Auto, false, dir.path());
        if cfg!(target_os = "linux") {
            assert_eq!(auto, Backend::Namespaces, "no image unpacked yet");
            let r = image::rootfs_dir(dir.path());
            std::fs::create_dir_all(r.join("usr/local/bin")).unwrap();
            std::fs::create_dir_all(r.join("opt/memory")).unwrap();
            std::fs::write(r.join("opt/memory/memory"), "").unwrap();
            assert_eq!(
                backend_of(BackendArg::Auto, false, dir.path()),
                Backend::Image
            );
            assert!(matches!(
                runner_for(Backend::Image, dir.path()).unwrap(),
                Runner::Image(_)
            ));
        }
        assert!(runner_for(Backend::Image, &dir.path().join("empty")).is_err());
        assert!(matches!(
            runner_for(Backend::None, dir.path()).unwrap(),
            Runner::Unconfined(None)
        ));
    }
}
