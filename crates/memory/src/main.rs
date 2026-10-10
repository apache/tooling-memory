//! `amem`: the per-session memory server, the egress, the launcher, and the command
//! line. With no subcommand it opens the browser on the web launcher, starting it first
//! when it is not already running.
//!
//! Every subcommand is a thin call into the library crates; the MCP server and the command
//! line see the same operations. The binary resolves its mounts at startup (`--root`,
//! `MEMORY_ROOT`, or `~/memory`) and, for `launch`, reads the one or two credentials it
//! needs from its own environment, filled from the root's `credentials.env`, and never
//! passes them inside.

mod board;
mod canary;
mod checker;
mod credentials;
mod curation;
mod deputy;
mod doctor;
mod drift;
mod golden;
mod harness_usage;
mod hooks;
mod image;
mod librarian;
mod mcp_http;
mod mine;
mod model;
mod open;
mod profiles;
mod replay;
mod requests;
mod review;
mod savings;
mod status;
mod taps;
mod transcripts;
mod work;
mod wslc;
mod wslc_api;

use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
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
    name = "amem",
    version,
    about = "Durable memory for disposable agents: one root, served over MCP, with a confined launcher for the harness.",
    after_help = "With no command, amem opens the web launcher in the browser, starting it when it is not running. `amem doctor` says what this host can run."
)]
struct Cli {
    /// The memory root: grants.toml, credentials.env, record/, state/. The only path
    /// this process touches. Defaults to ~/memory.
    #[arg(long, env = "MEMORY_ROOT", global = true)]
    root: Option<PathBuf>,
    #[command(flatten)]
    open: OpenArgs,
    #[command(subcommand)]
    command: Option<Command>,
}

/// What `amem` with no command takes: `amem web`'s two everyday flags.
#[derive(clap::Args)]
struct OpenArgs {
    /// Print the pairing link instead of opening a browser on it.
    #[arg(long)]
    no_open: bool,
    /// The working tree sessions start in. Defaults to the current directory, which may
    /// not hold the root.
    #[arg(long)]
    workdir: Option<PathBuf>,
    /// A hosted copy of the pages to open instead of the server's own, its origin let
    /// through: `https://<host>/<path>/`.
    #[arg(long, env = "AMEM_HOSTED")]
    hosted: Option<String>,
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
    Policy,
}

impl From<Kind> for EntryType {
    fn from(k: Kind) -> EntryType {
        match k {
            Kind::Decision => EntryType::Decision,
            Kind::Fact => EntryType::Fact,
            Kind::Convention => EntryType::Convention,
            Kind::Method => EntryType::Method,
            Kind::Handoff => EntryType::Handoff,
            Kind::Policy => EntryType::Policy,
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
    /// The pi coding agent, `pi`.
    Pi,
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
            HarnessArg::Pi => Harness::Pi,
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
        /// Run over an empty record of the same owner and domain, made for this run under
        /// the system's temporary directory: the same sandbox, egress and tools, and
        /// nothing in memory. For an evaluation's baseline. Its calls are metered in that
        /// record's telemetry, not this one's.
        #[arg(long)]
        empty_memory: bool,
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
        /// Leave the prefetch out of the prompt: no summaries of the entries the task
        /// likely needs, for comparisons against a run without memory's push.
        #[arg(long)]
        no_prefetch: bool,
        /// Work this task rather than the next one, and stop after it.
        #[arg(long)]
        task: Option<String>,
    },
    /// The librarian's survey: what in a domain needs curation, from the record and
    /// the read log, as proposals; with --file, each becomes a task for a run to do with
    /// the memory server's own tools, born proposed unless the domain admits its own.
    Librarian {
        #[arg(long)]
        domain: Option<String>,
        /// File each proposal as a task on the ledger.
        #[arg(long)]
        file: bool,
        /// Days after which a raw entry with no review date is due for a look.
        #[arg(long, default_value_t = 30)]
        stale_days: i64,
        /// Characters of body above which an entry is proposed for a split.
        #[arg(long, default_value_t = 12_000)]
        split_chars: usize,
        /// Where an entry's `file:` sources are resolved; a source changed after the
        /// entry was written calls for a verify. Defaults to the current directory.
        #[arg(long)]
        workdir: Option<PathBuf>,
        /// The librarian's own telemetry instead of a survey: curation per author from
        /// the record and the ledger, and the alarms it raises; --file files them.
        #[arg(long)]
        telemetry: bool,
    },
    /// One curation cycle under a budget: the survey filed, the checker on a sample,
    /// contradictions hunted, the canaries checked, the telemetry's alarms filed, and a
    /// line in state/curation.jsonl; with --every, again at that interval. The tasks it
    /// files are worked by `amem work` on a cheap model.
    Curate {
        #[arg(long)]
        domain: Option<String>,
        #[arg(long, value_enum, default_value = "gateway")]
        auth: AuthArg,
        #[arg(long)]
        provider: Option<String>,
        /// The checker's model, a different one than the librarian runs on.
        #[arg(long)]
        model: Option<String>,
        /// Entries the checker samples each cycle.
        #[arg(long, default_value_t = 8)]
        sample: usize,
        /// Hunt contradictions too, on the same sample.
        #[arg(long)]
        contradictions: bool,
        /// Curation tasks that may be open at once; the survey files up to the gap.
        #[arg(long, default_value_t = 20)]
        budget_tasks: usize,
        /// Tokens a cycle's model calls may take in all; the checker stops at it.
        #[arg(long, default_value_t = 200_000)]
        budget_tokens: u64,
        /// Run the vendor-drift probes on the checker's model each cycle, against its
        /// baseline, filing confirmed drift for the owner. Uses probes/drift.toml in the
        /// working directory.
        #[arg(long)]
        drift: bool,
        /// Where `file:` sources are resolved.
        #[arg(long)]
        workdir: Option<PathBuf>,
        /// Run a cycle every so many seconds, until stopped.
        #[arg(long)]
        every: Option<u64>,
    },
    /// The checker: a model of its own, not the librarian's, judges whether each
    /// summary the index serves entails its body, on a sample of a domain; it writes
    /// nothing, and with --file a rejected summary becomes a task for a librarian run.
    Check {
        #[arg(long, value_enum, default_value = "key")]
        auth: AuthArg,
        /// The destination; openai by default, llmao with --auth gateway.
        #[arg(long)]
        provider: Option<String>,
        /// The checker's model. Make it a different one than wrote the summaries.
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        domain: Option<String>,
        /// How many entries to sample, newest first.
        #[arg(long, default_value_t = 20)]
        sample: usize,
        #[arg(long)]
        file: bool,
        /// Hunt contradictions too: pairs a search puts side by side, judged by the model.
        #[arg(long)]
        contradictions: bool,
        /// Read the quarantine too: each held write not yet reviewed gets the model's
        /// verdict, malicious, report or benign, kept beside it for the owner.
        #[arg(long)]
        quarantine: bool,
    },
    /// Frozen-context replay: a kept transcript's decision points presented again to a
    /// model, its next action scored against the one recorded. The owner's; the run is
    /// logged under state/replay.
    Replay {
        /// The session whose transcript holds the decision points.
        session: String,
        /// Only this turn; every assistant turn otherwise, up to --limit.
        #[arg(long)]
        turn: Option<usize>,
        #[arg(long, default_value_t = 6)]
        limit: usize,
        #[arg(long, value_enum, default_value = "gateway")]
        auth: AuthArg,
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        model: Option<String>,
    },
    /// Canary entries: planted with known content, checked for alteration.
    Canary {
        #[command(subcommand)]
        op: CanaryOp,
    },
    /// The policy in force: the checkable perimeter as Rego, built in or the root's own
    /// under policy/*.rego. `show` prints it with its version; `eval` decides one
    /// request read as JSON from a file or standard input.
    Policy {
        #[command(subcommand)]
        op: PolicyOp,
    },
    /// Writes the gates refused, held under state/quarantine and never served: list
    /// them, show one, release one into the record as it was, or drop it.
    Quarantine {
        #[command(subcommand)]
        op: QuarantineOp,
    },
    /// What an entry may have reached: the sessions that read it and the entries they
    /// wrote after; where to look when an entry turns out to be poison.
    Taint { id: String },
    /// Promote an entry's trust tier. To curated at once, by anyone but its author; to
    /// verified by proposal only, a task a person who did not propose it approves.
    Promote {
        id: String,
        /// curated or verified.
        #[arg(long)]
        to: String,
        /// What was checked, and against what.
        #[arg(long, default_value = "")]
        why: String,
    },
    /// Approve a proposal to promote an entry to verified. The approver did not
    /// propose it, and is a person.
    Approve { task: String },
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
        /// The projection's directory name, which the launcher outside chose and mounts.
        #[arg(long)]
        projection: Option<String>,
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
        /// Task mode without the prefetch.
        #[arg(long)]
        no_prefetch: bool,
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
    /// Vendor drift: run a pinned probe set against one destination and model, and
    /// compare the latest run with a baseline, detector by detector, once nothing on
    /// our side is found to have changed.
    Drift {
        #[command(subcommand)]
        op: drift::DriftOp,
    },
    /// Inside the sandbox, run by the harness's hooks: read the event from standard
    /// input and leave a note in the outbox for the launcher to file as a handoff.
    #[command(hide = true)]
    Hook {
        /// Which harness's hook format: claude, codex, gemini, antigravity, qwen, grok, copilot or opencode.
        harness: String,
    },
    /// A web terminal on the launcher: sessions in pseudo-terminals streamed to the
    /// browser, with a reading pane over the record. Loopback only, behind a token kept
    /// in state/web.json. When one is already running over this root, opens that instead.
    Web {
        /// Where to listen; a loopback address. Defaults to 127.0.0.1:4200, or the next
        /// free port above it.
        #[arg(long)]
        listen: Option<String>,
        /// Print the link instead of opening a browser on it.
        #[arg(long)]
        no_open: bool,
        /// For the browser tests: every session runs this program, split on spaces, in
        /// place of the launcher, so the pages are driven without a harness or a sandbox.
        #[arg(long, hide = true)]
        session_command: Option<String>,
        /// A hosted copy of the pages to open instead of the server's own, its origin let
        /// through: `https://<host>/<path>/`.
        #[arg(long, env = "AMEM_HOSTED")]
        hosted: Option<String>,
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
    /// The profiles: `profiles.toml` under the root, and every harness by its own name.
    /// `amem <profile>` launches one; `launch` and `work` take `--profile <name>`.
    Profiles {
        /// As JSON, for the web launcher.
        #[arg(long)]
        json: bool,
    },
    /// What this host can run: the root and its grants, the credentials and logins
    /// found, the harnesses on the PATH and in the image, the backend, and whether the
    /// gateway answers. Names credentials, never their values.
    Doctor {
        /// The report as JSON, for the web launcher.
        #[arg(long)]
        json: bool,
        /// Skip the network: do not ask the gateway whether it answers.
        #[arg(long)]
        offline: bool,
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
    /// Session transcripts, kept in the record as sources and reached by citation:
    /// list them, show one turn, sweep the expired, erase one early.
    Transcript {
        #[command(subcommand)]
        op: transcripts::TranscriptOp,
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
enum PolicyOp {
    /// The policy text, its version and where it came from.
    Show,
    /// Decide one request: a JSON input, from a file or `-` for standard input.
    Eval {
        #[arg(default_value = "-")]
        input: String,
    },
    /// The record's policies: each by layer and tier, whether its rule is enforced, and
    /// the exceptions in force.
    List,
    /// The paper's conflict rules over the record's policies, and from the decision log
    /// each policy's citations and overrides; --file files a bug on the lower layer of
    /// each conflict, for the owner.
    Lint {
        #[arg(long)]
        file: bool,
    },
}

#[derive(Subcommand)]
enum QuarantineOp {
    List,
    Show {
        id: String,
    },
    /// Write it into the record as it was written, author and all. The owner's act.
    Release {
        id: String,
    },
    /// Discard it.
    Drop {
        id: String,
    },
}

#[derive(Subcommand)]
enum CanaryOp {
    /// Plant canaries in a domain and record them under the record's state.
    Plant {
        #[arg(long)]
        domain: Option<String>,
        #[arg(long, default_value_t = 3)]
        count: usize,
    },
    /// Read every planted canary back; exit 3 when one has been altered, and with
    /// --file, a task for the owner per tripped canary.
    Check {
        #[arg(long)]
        file: bool,
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
    /// Close a task without doing it, and everything still open that was filed from it.
    Invalidate {
        id: String,
        #[arg(long, default_value = "obsolete")]
        reason: String,
    },
    /// One task.
    Get { id: String },
    /// A task with everything filed from it: subagent tasks, continuations, retries.
    Tree { id: String },
    /// File a task again from one that dead-ended, with its chain's handoffs as lessons.
    Retry { id: String },
    /// Rank a domain's open tasks by the rubric in force, once: the paper's cycle.
    /// Blockers rise above what they block, children to their parents, and curation
    /// keeps its floor; each move is a `rescored` event with its reason.
    Rescore {
        #[arg(long)]
        domain: Option<String>,
    },
    /// How tasks flow: by family and status, why they were invalidated, and the
    /// requirements waiting that no completed task ever carried.
    Stats {
        #[arg(long)]
        domain: Option<String>,
    },
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

/// The command line, with any profile it names spelled out as flags, parsed. The root is
/// found as `Cli` finds it: `--root`, `MEMORY_ROOT`, then `~/memory`.
fn parse_with_profiles(args: Vec<OsString>) -> Result<Cli, String> {
    let commands = command_words();
    let root = profiles_root(&args);
    let cli = Cli::command();
    // which of a command's flags take a value, as its definition says
    let takes_value = |command: &str, flag: &str| {
        let long = flag.trim_start_matches('-');
        cli.find_subcommand(command)
            .and_then(|c| c.get_arguments().find(|a| a.get_long() == Some(long)))
            .is_some_and(|a| a.get_action().takes_values())
    };
    let argv = profiles::expand(args, &commands, &takes_value, || match &root {
        Some(r) => profiles::load(r, &commands),
        None => Ok(profiles::builtin()),
    })?;
    Ok(Cli::parse_from(argv))
}

/// The words that are commands, which no profile may take: clap's own `help` too, though
/// it is not listed until the parser is built.
fn command_words() -> Vec<String> {
    Cli::command()
        .get_subcommands()
        .map(|c| c.get_name().to_string())
        .chain(std::iter::once("help".to_string()))
        .collect()
}

/// The root a command line names, before it is parsed.
fn profiles_root(args: &[OsString]) -> Option<PathBuf> {
    let mut it = args.iter().skip(1);
    while let Some(a) = it.next() {
        if a == "--root" {
            return it.next().map(PathBuf::from);
        }
        if let Some(r) = a.to_str().and_then(|s| s.strip_prefix("--root=")) {
            return Some(PathBuf::from(r));
        }
    }
    std::env::var_os("MEMORY_ROOT")
        .map(PathBuf::from)
        .or_else(credentials::default_root)
}

fn main() {
    // The relay unshares a pid namespace, which only a single-threaded process may do,
    // so it runs before any runtime starts.
    let args: Vec<OsString> = std::env::args_os().collect();
    if args.get(1).is_some_and(|a| a == "relay") || args.get(3).is_some_and(|a| a == "relay") {
        let cli = Cli::parse();
        if let Some(Command::Relay {
            socket,
            listen,
            argv,
        }) = cli.command
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
    // Traces, when a collector is named; the guard flushes at exit. After the relay
    // and the hook: neither runs a runtime, and the relay may not have a second thread.
    let _otel = dm_otel::init("amem");
    let cli = match parse_with_profiles(args) {
        Ok(cli) => cli,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(2);
        }
    };
    let Some(root) = cli.root.clone().or_else(credentials::default_root) else {
        eprintln!("error: give --root, or set MEMORY_ROOT; there is no home for ~/memory");
        std::process::exit(1);
    };
    // one spelling of the root: the launcher's proof, its state and its sessions name it
    let root = std::fs::canonicalize(&root).unwrap_or(root);
    // the environment is written here, while this is the only thread
    let at_start = credentials::KNOWN
        .iter()
        .filter(|(k, _)| std::env::var_os(k).is_some_and(|v| !v.is_empty()))
        .map(|(k, _)| (*k).to_string())
        .collect::<Vec<_>>();
    credentials::load(&root);
    // The command futures are large and Windows gives the main thread one megabyte, so
    // the CLI runs on a thread with room; the runtime starts there.
    let outcome = std::thread::Builder::new()
        .name("amem".into())
        .stack_size(64 << 20)
        .spawn(move || run_cli(cli, root, at_start).map_err(|e| e.to_string()))
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

/// What is wrong with an invocation before anything runs: amem's own flags given to a
/// command, which would ignore them, or a root that is not there for a command that
/// needs one.
fn refuse_invocation(cli: &Cli, root: &Path) -> Fallible {
    let Some(command) = &cli.command else {
        return Ok(());
    };
    if cli.open.no_open || cli.open.workdir.is_some() {
        return Err(
            "--workdir and --no-open before a command are amem's own; give them after \
                    the command that takes them, as `amem web --workdir <dir>`"
                .into(),
        );
    }
    if !root.exists() && !matches!(command, Command::Init { .. } | Command::Doctor { .. }) {
        return Err(format!(
            "no record at {}: run `amem init` to make one there, or give --root (or MEMORY_ROOT)",
            root.display()
        )
        .into());
    }
    Ok(())
}

/// `amem serve`: the root over MCP on standard input and output, as an author, holding
/// the task `MEMORY_TASK` names if a launcher set it.
async fn serve_stdio_cmd(root: &Path, author: Option<String>, manifest: Vec<String>) -> Fallible {
    let mut m = Memory::open(root)?.with_manifest(manifest);
    if let Some(a) = author {
        m = m.with_author(a);
    }
    // a harness working a task has it in MEMORY_TASK, as the launcher sets it;
    // what this session files or writes is judged against that task
    if let Some(t) = std::env::var("MEMORY_TASK").ok().filter(|t| !t.is_empty()) {
        m.set_held_task(&t);
    }
    mcp::serve_stdio(Arc::new(m)).await?;
    Ok(())
}

#[tokio::main]
async fn run_cli(cli: Cli, root: PathBuf, at_start: Vec<String>) -> Fallible {
    refuse_invocation(&cli, &root)?;
    let Some(command) = cli.command else {
        return web_cmd(
            &root,
            Command::Web {
                listen: None,
                no_open: cli.open.no_open,
                session_command: None,
                hosted: cli.open.hosted,
                harness: None,
                auth: AuthArg::Subscription,
                backend: BackendArg::Auto,
                workdir: cli.open.workdir,
            },
        )
        .await;
    };
    match command {
        Command::Init { owner, domain } => {
            let m = Memory::init(&root, &owner.unwrap_or_else(default_owner), &domain)?;
            println!(
                "initialised {} for {} with domain {domain}",
                m.root().display(),
                m.author()
            );
            Ok(())
        }
        Command::Serve { author, manifest } => serve_stdio_cmd(&root, author, manifest).await,
        cmd @ (Command::Launch { .. } | Command::Work { .. } | Command::Inside { .. }) => {
            launch_cmd(&root, cmd).await
        }
        Command::Hook { .. } => unreachable!("handled in main before the root is needed"),
        cmd @ Command::Web { .. } => web_cmd(&root, cmd).await,
        Command::Doctor { json, offline } => doctor::run(&root, &at_start, json, offline).await,
        Command::Profiles { json: as_json } => {
            profiles::show(&profiles::load(&root, &command_words())?, as_json)
        }
        cmd @ (Command::Mine { .. }
        | Command::Check { .. }
        | Command::Curate { .. }
        | Command::Replay { .. }
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
        Command::Transcript { op } => transcripts::cmd(&Memory::open(&root)?, op).await,
        Command::Tap { op } => tap_cmd(&root, op).await,
        Command::Drift { op } => drift::run(&root, op).await,
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
                // Antigravity's eligibility check, before anything else, on a login:
                // the account's profile, then its picture
                "www.googleapis.com",
                ".googleusercontent.com",
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
                let token = crate::credentials::gh_token().ok_or(
                    "copilot needs GH_TOKEN in the launcher's environment or credentials.env, \
                     or a `gh auth login` on this host",
                )?;
                harness_env.push(("GH_TOKEN".to_string(), token));
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
            // so a cap that leaves a 131k-context model room for the prompt; and its
            // Gemini adapter drops a tool result Antigravity sends under the model's role
            (
                Destination::forward(url, bearer(key))
                    .with_folded_system_messages()
                    .with_function_responses_in_user_turns()
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
    /// Removes the render with the session, on the error paths `finish` never reaches.
    _render: RemoveOnDrop,
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

/// A directory removed when this is dropped. A session's render is its own and goes with
/// it, whichever way the session ends short of the process being killed.
struct RemoveOnDrop(Option<PathBuf>);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        if let Some(dir) = &self.0 {
            std::fs::remove_dir_all(dir).ok();
        }
    }
}

/// Whether a launch renders the projection, and under what name. A tool run on the host,
/// `recall` or `golden` say, mounts nothing and renders nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Projected<'a> {
    No,
    /// Under this session's id.
    AsSession,
    /// Under a name the launcher outside chose, for a backend that mounts the directory
    /// before the session inside it exists.
    Named(&'a str),
}

/// A projection's directory name as the command line gives it: one path component, so it
/// cannot name a directory outside the harness's own.
fn projection_name(name: &str) -> Result<&str, Box<dyn std::error::Error>> {
    if !name.is_empty()
        && name.len() <= 64
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        Ok(name)
    } else {
        Err(format!("--projection {name:?} is not a name: letters, digits and '-'").into())
    }
}

#[allow(clippy::too_many_arguments)] // one argument per thing a launch is given
async fn prepare_launch(
    root: &Path,
    harness: Harness,
    creds: Credentials<'_>,
    socket: Option<PathBuf>,
    recall_model: Option<&str>,
    projected: Projected<'_>,
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
    // one directory per session: two sessions of a harness at different ceilings would
    // otherwise each read what the other rendered
    let projection_dir = match projected {
        Projected::Named(n) => projection_name(n)?.to_string(),
        Projected::No | Projected::AsSession => memory.session_id().to_string(),
    };
    let projection = root
        .join(dm_sandbox::PROJECTION)
        .join(harness.binary())
        .join(projection_dir);
    // a name from outside is a directory made empty for this session; one with a render
    // in it is another session's, whose recall pages and handling this one would take over
    if matches!(projected, Projected::Named(_))
        && std::fs::read_dir(&projection).is_ok_and(|mut d| d.next().is_some())
    {
        return Err(format!(
            "the projection {} already holds a render; a session's is its own",
            projection.display()
        )
        .into());
    }
    let memory = memory.with_handling_cap(ceiling);
    let memory = Arc::new(if projected != Projected::No {
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
        _render: RemoveOnDrop((projected != Projected::No).then(|| projection.clone())),
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

#[allow(clippy::too_many_arguments)] // one argument per thing a run is given
async fn run_confined(
    p: &Prepared,
    harness: Harness,
    auth: AuthMode,
    workdir: &Path,
    args: &[String],
    env: &[(String, String)],
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
        harness_env: env,
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
        no_open,
        session_command,
        hosted,
        harness,
        auth,
        backend,
        workdir,
    } = command
    else {
        unreachable!("routed by run_cli")
    };
    if Memory::open(root).is_err() {
        return Err(format!(
            "no record at {}: run `amem init` to make one, or give --root; `amem doctor` says more",
            root.display()
        )
        .into());
    }
    let wanted = resolve_workdir(root, workdir);
    let opening = open::Opening {
        browser: !no_open,
        hosted: hosted.as_deref().map(open::Hosted::parse).transpose()?,
    };
    let mut state = open::state(root).await?;
    if open_running(
        root,
        &state,
        listen.as_deref(),
        wanted.as_ref().ok(),
        &opening,
    )
    .await?
    {
        return Ok(());
    }
    let workdir = wanted?;
    let listener = open::bind(listen.as_deref()).await?;
    let url = format!("http://{}", listener.local_addr()?);
    state.url = Some(url.clone());
    state.workdir = Some(workdir.display().to_string());
    dm_web::state::save(root, &state)?;
    let origins = opening
        .hosted
        .iter()
        .map(|h| h.origin().to_string())
        .collect();
    let access = dm_web::Access::new(root, &state, origins);
    let code = access.pairing_code()?;
    let config = web_config(
        root,
        workdir,
        (harness, auth, backend),
        access,
        session_command,
    )?;
    eprintln!(
        "amem: web launcher over {} · sessions start in {} · ctrl-c stops it",
        root.display(),
        config.workdir.display()
    );
    open::show(&url, &code, &opening);
    tokio::select! {
        r = dm_web::serve(config, listener) => r?,
        _ = tokio::signal::ctrl_c() => {}
    }
    // the address goes with the process, and only this process's address: the token
    // stays for the next server, and no browser key survives it
    let mut after = dm_web::state::load(root)?.state;
    if after.token == state.token && after.url.as_deref() == Some(url.as_str()) {
        after.url = None;
        after.workdir = None;
        dm_web::state::save(root, &after)?;
    }
    Ok(())
}

/// One server per root: when it is already up and proves it, the browser goes to it, and
/// a second one is not started beside it, whatever address was asked for. Whether it was.
async fn open_running(
    root: &Path,
    state: &dm_web::state::WebState,
    listen: Option<&str>,
    wanted: Option<&PathBuf>,
    opening: &open::Opening,
) -> Result<bool, Box<dyn std::error::Error>> {
    let Some(url) = &state.url else {
        return Ok(false);
    };
    let Some(running) = open::running(url, &state.token, root).await else {
        return Ok(false);
    };
    if listen.is_some_and(|l| format!("http://{l}") != running.url) {
        return Err(format!(
            "a launcher over this root is already running at {}; one per root",
            running.url
        )
        .into());
    }
    let pairing = open::pair(&running, &state.token)
        .await
        .ok_or("the running launcher gave no pairing code")?;
    if let Some(h) = &opening.hosted {
        if !pairing.origins.iter().any(|o| o == h.origin()) {
            return Err(format!(
                "the launcher running at {} was not started with --hosted {}, so that page \
                 cannot reach it: stop it (ctrl-c where it runs) and run amem again",
                running.url,
                h.origin()
            )
            .into());
        }
    }
    if let Some(w) = wanted {
        if pairing.workdir != w.display().to_string() {
            eprintln!(
                "amem: already running over this root, with sessions starting in {}, not {}; \
                 stop it (ctrl-c where it runs) to start them here",
                pairing.workdir,
                w.display()
            );
        }
    }
    open::show(&running.url, &pairing.code, opening);
    Ok(true)
}

/// The server's configuration. `session_command`, for the browser tests, runs in each
/// session's terminal in place of the launcher.
fn web_config(
    root: &Path,
    workdir: PathBuf,
    (harness, auth, backend): (Option<HarnessArg>, AuthArg, BackendArg),
    access: dm_web::Access,
    session_command: Option<String>,
) -> Result<dm_web::Config, Box<dyn std::error::Error>> {
    let backend_name = match backend {
        BackendArg::Auto => "auto",
        BackendArg::Namespaces => "namespaces",
        BackendArg::Image => "image",
        BackendArg::Wslc => "wslc",
        BackendArg::None => "none",
    };
    Ok(dm_web::Config {
        root: root.to_path_buf(),
        memory_binary: std::env::current_exe()?,
        workdir,
        harness: harness.map(|h| Harness::from(h).binary().to_string()),
        auth: match AuthMode::from(auth) {
            AuthMode::Subscription => "subscription",
            AuthMode::Key => "key",
            AuthMode::Gateway => "gateway",
        }
        .to_string(),
        backend: backend_name.to_string(),
        command_override: session_command
            .map(|c| c.split_whitespace().map(str::to_string).collect::<Vec<_>>())
            .filter(|c| !c.is_empty()),
        access,
    })
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
                Projected::No,
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
        Projected::No,
    )
    .await?;
    let client = work::client(p.memory.clone()).await?;
    let model = model::resolve(&p.egress, &p.provider, model.as_deref()).await;
    let kept = transcripts::keep_file(&p.memory, "mined", None, None, transcript).await?;
    let ids = mine::mine(
        &p.egress,
        &client,
        &p.provider,
        &model,
        transcript,
        Some(&kept.session),
    )
    .await?;
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
    let (destination, models) = list_models(root, auth, &provider).await?;
    println!(
        "{}",
        serde_json::json!({ "destination": destination, "models": models })
    );
    Ok(())
}

/// The models a destination lists, through the egress as a session would reach it.
async fn list_models(
    root: &Path,
    auth: AuthMode,
    provider: &str,
) -> Result<(String, Vec<String>), Box<dyn std::error::Error>> {
    let p = prepare_launch(
        root,
        Harness::Command,
        Credentials::keyed(auth, provider),
        None,
        None,
        Projected::No,
    )
    .await?;
    // Google lists under v1beta; everyone else speaks OpenAI's list, Anthropic included
    let path = if matches!(p.provider.as_str(), "gemini" | "google") {
        "v1beta/models"
    } else {
        "v1/models"
    };
    let v = p.egress.get(&p.provider, path).await?;
    Ok((p.provider.clone(), model::model_ids(&v)))
}

async fn tools_cmd(root: &Path, command: Command) -> Fallible {
    match command {
        Command::Models { auth, provider } => models_cmd(root, auth, provider).await,
        cmd @ (Command::Check { .. } | Command::Curate { .. } | Command::Replay { .. }) => {
            curation_cmd(root, cmd).await
        }
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
                Projected::No,
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

/// `amem replay`.
#[allow(clippy::too_many_arguments)] // one per flag of the command
async fn replay_cmd(
    root: &Path,
    session: &str,
    turn: Option<usize>,
    limit: usize,
    auth: AuthArg,
    provider: Option<String>,
    model: Option<String>,
) -> Fallible {
    let auth = AuthMode::from(auth);
    let provider = checker_destination(auth, provider);
    let p = prepare_launch(
        root,
        Harness::Command,
        Credentials::keyed(auth, &provider),
        None,
        None,
        Projected::No,
    )
    .await?;
    p.memory.disable_read_log();
    let model = model::resolve(&p.egress, &p.provider, model.as_deref()).await;
    // the decision contexts are read as the owner; the calls go through the egress
    let owner = Memory::open(root)?.without_read_log();
    let run = replay::replay(&p.egress, &owner, session, &p.provider, &model, turn, limit).await?;
    json(&run)
}

/// The curation commands: the checker, and the cycle.
async fn curation_cmd(root: &Path, command: Command) -> Fallible {
    match command {
        Command::Check {
            auth,
            provider,
            model,
            domain,
            sample,
            file,
            contradictions,
            quarantine,
        } => {
            let what = CheckWhat {
                file,
                contradictions,
                quarantine,
            };
            check_cmd(root, auth, provider, model, domain, sample, what).await
        }
        Command::Curate {
            domain,
            auth,
            provider,
            model,
            sample,
            contradictions,
            budget_tasks,
            budget_tokens,
            drift,
            workdir,
            every,
        } => {
            let plan = CuratePlan {
                domain,
                auth,
                provider,
                model,
                sample,
                contradictions,
                budget_tasks,
                budget_tokens,
                drift,
                workdir: workdir.or_else(|| std::env::current_dir().ok()),
            };
            curate_loop(root, &plan, every).await
        }
        Command::Replay {
            session,
            turn,
            limit,
            auth,
            provider,
            model,
        } => replay_cmd(root, &session, turn, limit, auth, provider, model).await,
        _ => unreachable!("routed by tools_cmd"),
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
    if backend_of(*backend, *unconfined, root, (*harness).into()) != Backend::Wslc {
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
        projection: wslc::projection_name(),
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
        no_prefetch,
        ..
    } = command
    else {
        return Ok(None);
    };
    if backend_of(*backend, *unconfined, root, (*harness).into()) != Backend::Wslc {
        return Ok(None);
    }
    let mut inside_args = work_inside_args(
        run_options_args(model.as_deref(), *subagents),
        domain.as_deref(),
        *lease_seconds,
        *keep_going,
        *idle_seconds,
    );
    if *no_prefetch {
        inside_args.push("--no-prefetch".into());
    }
    Ok(Some(wslc::Launch {
        projection: wslc::projection_name(),
        root: root.to_path_buf(),
        harness: (*harness).into(),
        auth: (*auth).into(),
        provider: provider.clone(),
        server_auth: server_auth.map(AuthMode::from),
        server_provider: server_provider.clone(),
        workdir: resolve_workdir(root, workdir.clone())?,
        image: image.clone(),
        inside_args,
        interactive: false,
        via: *wslc_via,
        storage: wslc_storage.clone(),
    }))
}

/// A session the web launcher started runs confined unless the operator allowed
/// otherwise: a browser, or a profile it picked, asking for no confinement is refused.
/// `auto` on a platform with no backend is what the command line gets too, and stands.
fn refuse_browser_unconfined(unconfined: bool, backend: BackendArg) -> Fallible {
    let asked = unconfined || matches!(backend, BackendArg::None);
    let allowed = std::env::var("AMEM_WEB_ALLOW_UNCONFINED").is_ok_and(|v| v == "1");
    if asked && status::web_session().is_some() && !allowed {
        return Err(
            "a session started from the browser runs confined; the operator allows \
                    otherwise with `amem web --backend none`"
                .into(),
        );
    }
    Ok(())
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
        empty_memory,
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
    refuse_browser_unconfined(unconfined, backend)?;
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
        runner_for(backend_of(backend, unconfined, root, harness.into()), root)?,
        RunOptions::new(model, subagents, program, None).empty_memory(empty_memory),
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
        no_prefetch,
        task,
        ..
    } = command
    else {
        unreachable!("routed by launch_cmd")
    };
    let workdir = resolve_workdir(root, workdir)?;
    refuse_browser_unconfined(unconfined, backend)?;
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
        runner_for(backend_of(backend, unconfined, root, harness.into()), root)?,
        RunOptions::new(model, subagents, Vec::new(), None),
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
        prefetch: !no_prefetch,
        task,
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
        projection,
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
        no_prefetch,
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
        RunOptions::new(model, subagents, program, projection),
    )
    .await?;
    let code = if work {
        let options = work::Options {
            domain,
            lease_seconds,
            keep_going,
            idle: std::time::Duration::from_secs(idle_seconds),
            mine: None,
            prefetch: !no_prefetch,
            task: None,
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
    /// The projection's directory name, when the launcher outside chose it.
    projection: Option<String>,
    /// Run over an empty record made for this run, for an evaluation's baseline.
    empty_memory: bool,
}

impl RunOptions {
    fn new(
        model: Option<String>,
        subagents: SubagentsArg,
        program: Vec<String>,
        projection: Option<String>,
    ) -> RunOptions {
        RunOptions {
            model,
            subagents,
            program,
            projection,
            empty_memory: false,
        }
    }

    fn empty_memory(mut self, yes: bool) -> RunOptions {
        self.empty_memory = yes;
        self
    }
}

/// An empty record of the same owner and domain as `root`, under the system's temporary
/// directory, for a run that should have everything but memory.
fn empty_record(root: &Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let grants = Memory::open(root)?.grants().clone();
    let domain = grants
        .domains
        .keys()
        .next()
        .cloned()
        .ok_or("the record grants no domain to copy")?;
    let dir = std::env::temp_dir().join(format!(
        "amem-empty-{}-{}",
        std::process::id(),
        time::OffsetDateTime::now_utc().unix_timestamp_nanos()
    ));
    std::fs::create_dir_all(&dir)?;
    Memory::init(&dir, &grants.owner, &domain)?;
    eprintln!("memory: empty memory at {}", dir.display());
    Ok(dir)
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
            "the working tree {} and the record root {} overlap: sessions would reach the \
             record by the side door. Run amem from a project directory, or pass --workdir",
            w.display(),
            root.display()
        )
        .into());
    }
    Ok(w)
}

/// The model as the harness names it on this route. OpenCode and pi name a model by its
/// provider, which here is the destination. Antigravity CLI takes a model outside
/// Google's catalog only as `gemini-api://<model>`, sent to its base URL as
/// `v1beta/models/<model>`, and a gateway's models are never in that catalog.
fn model_for(harness: Harness, auth: AuthMode, provider: &str, model: &str) -> String {
    match harness {
        Harness::OpenCode | Harness::Pi if !model.contains('/') => format!("{provider}/{model}"),
        Harness::Antigravity if auth == AuthMode::Gateway && !model.contains("://") => {
            format!("gemini-api://{model}")
        }
        _ => model.to_string(),
    }
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
/// is one that carries the harness, so homogeneity is the default once you have it, and
/// the host's binaries otherwise: Antigravity CLI, say, which the image does not carry.
fn backend_of(arg: BackendArg, unconfined: bool, root: &Path, harness: Harness) -> Backend {
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
                if image::present(root).is_some_and(|r| image::carries(&r, harness)) {
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

/// The line a session starts with: where the harness runs, where its calls go, and the
/// memory session it is.
fn announce(runner: &Runner, p: &Prepared, harness: Harness, workdir: &Path) {
    let how = match runner {
        Runner::Namespaces => "confined to",
        Runner::Image(_) => "confined, in the harness image, to",
        Runner::Unconfined(_) => "unconfined in",
        Runner::Requests(_) => "in the harness image, working tree at",
    };
    let shown = match runner {
        Runner::Requests(r) => r.workdir_inside.clone(),
        _ => workdir.to_path_buf(),
    };
    eprintln!(
        "memory: {} {how} {} · model calls via {} at ceiling {} · session {}",
        harness.binary(),
        shown.display(),
        p.ceiling_of,
        p.ceiling,
        p.memory.session_id()
    );
}

/// The status a session started from the web launcher writes for it.
fn start_status(
    root: &Path,
    p: &Prepared,
    harness: Harness,
    auth: AuthMode,
    model: Option<String>,
) -> Option<status::Writer> {
    status::Writer::start(
        root,
        status::Fixed {
            memory_session: p.memory.session_id().to_string(),
            harness: harness.binary().to_string(),
            auth: wslc::auth_name(auth).to_string(),
            destination: p.provider.clone(),
            ceiling_of: p.ceiling_of.clone(),
            ceiling: p.ceiling,
            metered: p.metered,
            model,
        },
        p.memory.clone(),
    )
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
    /// What the session says about itself to the web launcher that started it.
    #[allow(dead_code)] // kept for its drop, which writes the last word
    status: Option<status::Writer>,
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
        let projected = options
            .projection
            .as_deref()
            .map_or(Projected::AsSession, Projected::Named);
        let empty;
        let root = if options.empty_memory {
            empty = empty_record(root)?;
            empty.as_path()
        } else {
            root
        };
        let mut p = prepare_launch(root, harness, creds, socket, None, projected).await?;
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
        announce(&runner, &p, harness, &workdir);
        let status = start_status(root, &p, harness, auth, options.model.clone());
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
            status,
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
        let model = model::resolve(&self.p.egress, &self.p.server.provider, model).await;
        let client = work::client(self.p.memory.clone()).await?;
        let home = Some(self.p.home.as_path());
        let kept = transcripts::keep_file(&self.p.memory, self.harness.binary(), None, home, &t);
        let kept = kept.await.ok();
        let kept = kept.as_ref().map(|k| k.session.as_str());
        let ids = if self.mines_via_harness(via) {
            self.mine_via_harness(&client, &model, &t, kept).await?
        } else {
            mine::mine(
                &self.p.egress,
                &client,
                &self.p.server.provider,
                &model,
                &t,
                kept,
            )
            .await?
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
        kept: Option<&str>,
    ) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        let Some((excerpt, turns)) = mine::excerpt_of(transcript)? else {
            return Ok(Vec::new());
        };
        let mut args = self.harness.headless(&mine::prompt_for(&excerpt));
        // the harness's name for the model on this route; a mined entry names it plainly
        let named = model_for(self.harness, self.auth, &self.p.provider, model);
        args.extend(self.harness.model_flag(&named));
        let (code, output) = self.run(&args, true).await?;
        // the mining run is not a session anyone continues: its own hook notes go
        let _ = std::fs::remove_dir_all(self.outbox());
        if code != 0 {
            return Err(format!("the mining run exited {code}").into());
        }
        let found = mine::proposals(&output.unwrap_or_default());
        let cite = mine::cite_for(kept, transcript, turns);
        mine::write_proposals(client, &found, &cite, model).await
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
        self.run_task(args, capture, None).await
    }

    /// Run the harness for a task it holds: the task's id goes into the environment as
    /// `MEMORY_TASK`, so the hooks know which task a subagent is filed from.
    ///
    /// # Errors
    ///
    /// When the sandbox cannot be built or the harness cannot be started.
    pub async fn run_task(
        &self,
        args: &[String],
        capture: bool,
        task: Option<&str>,
    ) -> Result<(i32, Option<String>), Box<dyn std::error::Error>> {
        let mut env = self.p.harness_env.clone();
        if let Some(t) = task {
            env.push(("MEMORY_TASK".to_string(), t.to_string()));
            self.p.memory.set_held_task(t);
        }
        let mut full: Vec<String> = [self.program.as_slice(), args].concat();
        if let Some(m) = &self.model {
            let m = model_for(self.harness, self.auth, &self.p.provider, m);
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
                    &env,
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
                    &env,
                    capture,
                    Some(rootfs),
                )
                .await
            }
            Runner::Requests(r) => self.run_requested(r, args, &env, capture).await,
        }
    }

    /// Write a run request for the host and wait for the harness container's exit code.
    async fn run_requested(
        &self,
        r: &RequestsRunner,
        args: &[String],
        env: &[(String, String)],
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
            harness_env: env,
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
        // the render was this session's alone, and goes with it; here as well as in the
        // guard, since a launch ends in process::exit, which runs no destructors
        std::fs::remove_dir_all(&self.p.projection).ok();
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
    // the egress holds the credentials; the harness gets what is set for it below
    for (k, _) in credentials::KNOWN {
        cmd.env_remove(k);
    }
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
        | Harness::Pi
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

/// `amem canary`: plant, or check and alarm.
async fn canary_cmd(m: &Memory, op: CanaryOp) -> Fallible {
    match op {
        CanaryOp::Plant { domain, count } => {
            let new = canary::plant(m, m.root(), domain.as_deref(), count).await?;
            json(&new)
        }
        CanaryOp::Check { file } => {
            let report = canary::check(m, m.root()).await?;
            let filed = if file && !report.tripped.is_empty() {
                let domain = canary::planted(m.root())?
                    .first()
                    .map(|c| c.domain.clone())
                    .unwrap_or_default();
                canary::file(m, &report, &domain).await?
            } else {
                Vec::new()
            };
            json(&serde_json::json!({
                "planted": report.planted,
                "intact": report.intact,
                "tripped": report.tripped,
                "filed": filed,
            }))?;
            if !report.tripped.is_empty() {
                std::process::exit(3);
            }
            Ok(())
        }
    }
}

/// `amem check`: the checker over a sample of a domain, through a destination of its own.
/// A curation cycle's settings.
struct CuratePlan {
    domain: Option<String>,
    auth: AuthArg,
    provider: Option<String>,
    model: Option<String>,
    sample: usize,
    contradictions: bool,
    budget_tasks: usize,
    budget_tokens: u64,
    drift: bool,
    workdir: Option<PathBuf>,
}

/// The destination a checker or a cycle speaks to, from the auth mode and provider given.
fn checker_destination(auth: AuthMode, provider: Option<String>) -> String {
    provider.unwrap_or_else(|| {
        if auth == AuthMode::Gateway {
            "llmao".to_string()
        } else {
            "openai".to_string()
        }
    })
}

/// How many curation tasks are open: proposed or admitted, filed by the survey, the
/// checker or the telemetry.
async fn open_curation_tasks(
    m: &Memory,
    domain: &str,
) -> Result<usize, Box<dyn std::error::Error>> {
    let mut n = 0;
    for status in [dm_ledger::Status::Proposed, dm_ledger::Status::Admitted] {
        n += m
            .tasks(Some(domain), status)
            .await?
            .iter()
            .filter(|t| {
                t.spec.origin.as_deref().is_some_and(|o| {
                    o.starts_with("librarian:")
                        || o.starts_with("checker:")
                        || o.starts_with("telemetry:")
                })
            })
            .count();
    }
    Ok(n)
}

/// The summaries of a domain's newest entries, for the checker's tasks.
async fn summaries_of(
    m: &Memory,
    domain: &str,
    n: usize,
) -> Result<Vec<(String, String)>, Box<dyn std::error::Error>> {
    Ok(checker::sample_of(m, domain, n)
        .await?
        .iter()
        .map(|h| (h.id.clone(), h.summary.clone()))
        .collect())
}

/// `amem librarian --telemetry`: the librarian's own telemetry, its alarms filed with --file.
async fn telemetry_cmd(m: &Memory, domain: Option<&str>, file: bool) -> Fallible {
    let t = curation::telemetry(m, domain).await?;
    let filed = if file {
        curation::file_alarms(m, &t).await?
    } else {
        Vec::new()
    };
    json(&serde_json::json!({
        "domain": t.domain,
        "by_author": t.by_author,
        "alarms": t.alarms,
        "filed": filed,
    }))
}

/// `amem curate`: a cycle, and with `every`, a cycle at that interval until stopped.
async fn curate_loop(root: &Path, plan: &CuratePlan, every: Option<u64>) -> Fallible {
    loop {
        curate_cmd(root, plan).await?;
        let Some(secs) = every else {
            return Ok(());
        };
        eprintln!("amem: next curation cycle in {secs}s");
        tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
    }
}

/// The checker's part of a cycle: the sample judged, contradictions hunted, both filed.
async fn checker_pass(
    p: &Prepared,
    plan: &CuratePlan,
    domain: &str,
    model: &str,
    cycle: &mut curation::Cycle,
) -> Result<(), Box<dyn std::error::Error>> {
    let m = &p.memory;
    let spend = checker::Spend {
        egress: &p.egress,
        session: m.session_id(),
        limit: plan.budget_tokens,
    };
    let report = checker::check(
        &p.egress,
        m,
        &p.provider,
        model,
        Some(domain),
        plan.sample,
        Some(&spend),
    )
    .await?;
    cycle.checked = report.checked.len();
    cycle.rejected = report.checked.iter().filter(|v| !v.entails).count();
    let summaries = summaries_of(m, domain, plan.sample).await?;
    checker::file(m, &report, &summaries).await?;
    if plan.contradictions {
        let found = checker::hunt(
            &p.egress,
            m,
            &p.provider,
            model,
            Some(domain),
            plan.sample,
            Some(&spend),
        )
        .await?;
        cycle.contradictions = found.len();
        checker::file_contradictions(m, domain, model, &found).await?;
    }
    if !spend.exhausted() {
        let reviewed = review::review(&p.egress, m, &p.provider, model, plan.sample).await?;
        cycle.reviewed = reviewed.len();
        cycle.malicious = reviewed
            .iter()
            .filter(|r| r.verdict == review::Kind::Malicious)
            .count();
    }
    if plan.drift && !spend.exhausted() {
        let set = plan
            .workdir
            .clone()
            .unwrap_or_default()
            .join("probes/drift.toml");
        if set.is_file() {
            cycle.drift = drift::scheduled(&p.egress, m, &set, &p.provider, model, Some(domain))
                .await
                .unwrap_or_else(|e| Some(format!("probe failed: {e}")));
        }
    }
    if let Some(t) = p.egress.telemetry().session(m.session_id())? {
        cycle.calls = t.calls;
        cycle.tokens_in = t.input_tokens;
        cycle.tokens_out = t.output_tokens;
    }
    Ok(())
}

/// The owner's part of a cycle: the canaries checked and the telemetry's alarms, filed.
async fn owner_pass(
    m: &Memory,
    root: &Path,
    domain: &str,
    cycle: &mut curation::Cycle,
) -> Result<(), Box<dyn std::error::Error>> {
    let canaries = canary::check(m, root).await?;
    cycle.canaries_tripped = canaries.tripped.len();
    if !canaries.tripped.is_empty() {
        canary::file(m, &canaries, domain).await?;
    }
    cycle.rescored = m.rescore(Some(domain)).await?.changed.len();
    let t = curation::telemetry(m, Some(domain)).await?;
    cycle.alarms = t.alarms.len();
    curation::file_alarms(m, &t).await?;
    transcripts::sweep(m).await?;
    Ok(())
}

/// `amem curate`: one cycle, logged.
async fn curate_cmd(root: &Path, plan: &CuratePlan) -> Fallible {
    let auth = AuthMode::from(plan.auth);
    let provider = checker_destination(auth, plan.provider.clone());
    let p = prepare_launch(
        root,
        Harness::Command,
        Credentials::keyed(auth, &provider),
        None,
        None,
        Projected::No,
    )
    .await?;
    let m = &p.memory;
    m.disable_read_log();
    let domain = m.resolve_domain(plan.domain.as_deref())?;
    let model = model::resolve(&p.egress, &p.provider, plan.model.as_deref()).await;
    let mut cycle = curation::Cycle {
        at: time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default(),
        domain: domain.clone(),
        budget_tasks: plan.budget_tasks,
        budget_tokens: plan.budget_tokens,
        checker_model: model.clone(),
        ..curation::Cycle::default()
    };
    // the survey, filed up to the budget's gap
    cycle.open_before = open_curation_tasks(m, &domain).await?;
    let options = librarian::Options {
        source_root: plan.workdir.clone(),
        ..librarian::Options::default()
    };
    let mut survey = librarian::survey(m, root, Some(&domain), &options).await?;
    cycle.proposals = survey.proposals.len();
    let room = plan.budget_tasks.saturating_sub(cycle.open_before);
    survey.proposals.truncate(room);
    let (filed, already) = librarian::file(m, &survey).await?;
    cycle.filed = filed.len();
    cycle.already_filed = already;
    checker_pass(&p, plan, &domain, &model, &mut cycle).await?;
    owner_pass(m, root, &domain, &mut cycle).await?;
    curation::log_cycle(root, &cycle)?;
    json(&cycle)
}

/// What a checker run does besides judging summaries.
struct CheckWhat {
    /// File what it finds as tasks.
    file: bool,
    /// Hunt contradictions.
    contradictions: bool,
    /// Read the quarantine.
    quarantine: bool,
}

async fn check_cmd(
    root: &Path,
    auth: AuthArg,
    provider: Option<String>,
    model: Option<String>,
    domain: Option<String>,
    sample: usize,
    what: CheckWhat,
) -> Fallible {
    let auth = AuthMode::from(auth);
    let provider = checker_destination(auth, provider);
    let p = prepare_launch(
        root,
        Harness::Command,
        Credentials::keyed(auth, &provider),
        None,
        None,
        Projected::No,
    )
    .await?;
    p.memory.disable_read_log();
    let model = model::resolve(&p.egress, &p.provider, model.as_deref()).await;
    let report = checker::check(
        &p.egress,
        &p.memory,
        &p.provider,
        &model,
        domain.as_deref(),
        sample,
        None,
    )
    .await?;
    let found = if what.contradictions {
        checker::hunt(
            &p.egress,
            &p.memory,
            &p.provider,
            &model,
            domain.as_deref(),
            sample,
            None,
        )
        .await?
    } else {
        Vec::new()
    };
    let reviewed = if what.quarantine {
        review::review(&p.egress, &p.memory, &p.provider, &model, sample).await?
    } else {
        Vec::new()
    };
    let mut filed = Vec::new();
    if what.file {
        let summaries = summaries_of(&p.memory, &report.domain, sample).await?;
        filed = checker::file(&p.memory, &report, &summaries).await?;
        filed
            .extend(checker::file_contradictions(&p.memory, &report.domain, &model, &found).await?);
    }
    json(&serde_json::json!({
        "domain": report.domain,
        "model": report.model,
        "checked": report.checked,
        "rejected": report.checked.iter().filter(|v| !v.entails).count(),
        "contradictions": found,
        "quarantine": reviewed,
        "filed": filed,
    }))
}

/// `amem policy`, `amem quarantine`, `amem taint`, `amem canary`, `amem erase` and
/// `amem reindex`: the perimeter's commands and the record's repair.
async fn perimeter_cmd(m: &Memory, command: Command) -> Fallible {
    match command {
        Command::Policy { op: PolicyOp::Show } => {
            println!(
                "# policy {} from {}\n{}",
                m.policy().version(),
                m.policy().source(),
                m.policy().text()
            );
            Ok(())
        }
        Command::Policy {
            op: PolicyOp::Eval { input },
        } => {
            let text = if input == "-" {
                let mut s = String::new();
                std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)?;
                s
            } else {
                std::fs::read_to_string(&input)?
            };
            let input: dm_policy::decide::Input = serde_json::from_str(&text)?;
            json(&m.policy().decide(&input)?)
        }
        Command::Policy { op: PolicyOp::List } => json(&policy_list(m)),
        Command::Policy {
            op: PolicyOp::Lint { file },
        } => policy_lint(m, file).await,
        Command::Quarantine {
            op: QuarantineOp::List,
        } => json(
            &m.quarantined()?
                .iter()
                .map(|e| {
                    let review = review::read(m.root(), &e.front.id);
                    serde_json::json!({
                        "review": review.map(|r| serde_json::json!({"verdict": r.verdict, "why": r.why, "model": r.model})),
                        "id": e.front.id, "domain": e.front.domain, "author": e.front.author,
                        "created": e.front.created.format(&time::format_description::well_known::Rfc3339).unwrap_or_default(),
                        "title": e.front.title,
                    })
                })
                .collect::<Vec<_>>(),
        ),
        Command::Quarantine {
            op: QuarantineOp::Show { id },
        } => {
            let e = m
                .quarantined()?
                .into_iter()
                .find(|e| e.front.id == id)
                .ok_or_else(|| format!("{id} is not in quarantine"))?;
            print!("{}", e.render());
            Ok(())
        }
        Command::Quarantine {
            op: QuarantineOp::Release { id },
        } => json(&m.release_quarantined(&id, true).await?),
        Command::Quarantine {
            op: QuarantineOp::Drop { id },
        } => json(&m.release_quarantined(&id, false).await?),
        Command::Taint { id } => json(&m.taint(&id)),
        Command::Promote { id, to, why } => json(&m.promote(&id, to.parse()?, &why).await?),
        Command::Approve { task } => json(&m.approve(&task).await?),
        Command::Canary { op } => canary_cmd(m, op).await,
        Command::Erase { id, reason } => json(&m.erase(&id, reason.into()).await?),
        Command::Reindex => {
            let (n, events) = m.reindex().await?;
            println!("indexed {n} entries, replayed {events} ledger events");
            Ok(())
        }
        _ => unreachable!("routed by maintain"),
    }
}

/// The record's policies, as `amem policy list` shows them.
fn policy_list(m: &Memory) -> serde_json::Value {
    let p = m.policy();
    let now = time::OffsetDateTime::now_utc();
    let rows: Vec<_> = p
        .entries()
        .iter()
        .map(|e| {
            serde_json::json!({
                "id": e.meta.id, "entry": e.entry_id, "layer": e.meta.layer, "tier": e.tier,
                "rule": e.meta.rule, "exception": e.meta.is_exception(),
                "enforced": e.enforced(), "in_force": e.in_force(now),
                "breaches": e.meta.breaches, "scope_task": e.meta.scope_task,
            })
        })
        .collect();
    serde_json::json!({
        "version": p.version(), "policies": rows,
        "refused": p.refused().iter().map(|(id, why)| serde_json::json!({"id": id, "why": why})).collect::<Vec<_>>(),
    })
}

/// Citations and overrides per policy, from the decision log: how often each rule
/// refused something, and how often an exception let something through.
fn policy_rates(root: &Path) -> serde_json::Value {
    let text = std::fs::read_to_string(root.join("state/decisions.jsonl")).unwrap_or_default();
    let mut cited: std::collections::BTreeMap<String, (u64, u64)> =
        std::collections::BTreeMap::new();
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let allow = v["allow"].as_bool().unwrap_or(false);
        for id in v["cites"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| c.as_str())
        {
            let row = cited.entry(id.to_string()).or_default();
            if allow {
                row.1 += 1;
            } else {
                row.0 += 1;
            }
        }
    }
    serde_json::json!(cited
        .into_iter()
        .map(|(id, (refused, overridden))| serde_json::json!({"id": id, "refused": refused, "let_through": overridden}))
        .collect::<Vec<_>>())
}

/// `amem policy lint`.
async fn policy_lint(m: &Memory, file: bool) -> Fallible {
    let p = m.policy();
    let mut findings = dm_policy::record::lint(p.entries(), time::OffsetDateTime::now_utc());
    findings.extend(
        p.refused()
            .iter()
            .map(|(id, why)| dm_policy::record::Finding {
                policy: id.clone(),
                kind: "module-refused".into(),
                why: format!("{id}'s module would not load: {why}"),
            }),
    );
    let mut filed = Vec::new();
    if file {
        let domain = m.resolve_domain(None)?;
        for f in findings.iter().filter(|f| f.kind != "inert") {
            let created = m
                .file_own(dm_ledger::TaskSpec {
                    domain: domain.clone(),
                    title: format!("Policy bug, {}: {}", f.kind, f.policy),
                    body: format!(
                        "{}\n\nAcross layers the higher wins and the lower is fixed: amend or supersede {} so it agrees, or file an exception with a scope, an expiry and an approver. Within a layer, resolve from the stated intents and record the trade-off.",
                        f.why, f.policy
                    ),
                    requirements: vec!["human".into()],
                    depends_on: Vec::new(),
                    origin: Some(format!("policy:{}", f.kind)),
                    upstream_event: Some(f.policy.clone()),
                    created_by: String::new(),
                    classification: dm_core::Label::Internal,
                    priority: 4,
                })
                .await?;
            if !created.deduplicated {
                filed.push(created.task.id);
            }
        }
    }
    json(&serde_json::json!({
        "findings": findings, "rates": policy_rates(m.root()), "filed": filed,
    }))
}

/// `amem librarian`: the survey, or the survey filed as tasks.
async fn librarian_cmd(
    m: &Memory,
    domain: Option<&str>,
    file: bool,
    stale_days: i64,
    split_chars: usize,
    workdir: Option<PathBuf>,
) -> Fallible {
    let options = librarian::Options {
        stale_days,
        split_chars,
        source_root: workdir,
        ..librarian::Options::default()
    };
    let survey = librarian::survey(m, m.root(), domain, &options).await?;
    if file {
        let (ids, already) = librarian::file(m, &survey).await?;
        json(&serde_json::json!({
            "domain": survey.domain,
            "entries": survey.entries,
            "proposals": survey.proposals.len(),
            "filed": ids,
            "already_filed": already,
        }))
    } else {
        json(&survey)
    }
}

/// `amem export` and `amem import`: a domain as a bundle, out and in.
async fn bundle_cmd(m: &Memory, command: Command) -> Fallible {
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
        _ => unreachable!("routed by maintain"),
    }
}

async fn maintain(m: &Memory, command: Command) -> Fallible {
    match command {
        cmd @ (Command::Export { .. } | Command::Import { .. }) => bundle_cmd(m, cmd).await,
        Command::Librarian {
            domain,
            file,
            stale_days,
            split_chars,
            workdir,
            telemetry,
        } => {
            // the survey's reads are the librarian's, not a session's
            m.disable_read_log();
            if telemetry {
                return telemetry_cmd(m, domain.as_deref(), file).await;
            }
            let workdir = workdir.or_else(|| std::env::current_dir().ok());
            librarian_cmd(m, domain.as_deref(), file, stale_days, split_chars, workdir).await
        }
        cmd @ (Command::Canary { .. }
        | Command::Erase { .. }
        | Command::Promote { .. }
        | Command::Approve { .. }
        | Command::Reindex
        | Command::Policy { .. }
        | Command::Quarantine { .. }
        | Command::Taint { .. }) => perimeter_cmd(m, cmd).await,
        Command::Init { .. }
        | Command::Serve { .. }
        | Command::Launch { .. }
        | Command::Work { .. }
        | Command::Inside { .. }
        | Command::Image { .. }
        | Command::Web { .. }
        | Command::Mine { .. }
        | Command::Check { .. }
        | Command::Curate { .. }
        | Command::Replay { .. }
        | Command::Models { .. }
        | Command::WslcApiCheck { .. }
        | Command::Savings { .. }
        | Command::Golden { .. }
        | Command::Drift { .. }
        | Command::Hook { .. }
        | Command::Doctor { .. }
        | Command::Profiles { .. }
        | Command::Relay { .. }
        | Command::Usage
        | Command::Task { .. }
        | Command::Transcript { .. }
        | Command::Tap { .. }
        | Command::Write { .. }
        | Command::Search { .. }
        | Command::Recall { .. }
        | Command::Get { .. }
        | Command::List { .. }
        | Command::T0 { .. } => unreachable!("dispatched before reaching maintain"),
    }
}

/// `amem task rescore` and `amem task stats`: the fabric's ranking and its telemetry.
async fn fabric_cmd(m: &Memory, op: TaskOp) -> Fallible {
    match op {
        TaskOp::Rescore { domain } => json(&m.rescore(domain.as_deref()).await?),
        TaskOp::Stats { domain } => json(&m.task_stats(domain.as_deref()).await?),
        _ => unreachable!("routed by task"),
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
        TaskOp::Tree { id } => json(&m.task_tree(&id).await?),
        TaskOp::Retry { id } => json(&m.retry_task(&id).await?),
        op @ (TaskOp::Rescore { .. } | TaskOp::Stats { .. }) => fabric_cmd(m, op).await,
        TaskOp::Get { id } => json(&m.get_task(&id).await?),
    }
}

#[cfg(test)]
mod backend_tests {
    use super::*;

    #[tokio::test]
    async fn each_session_renders_under_a_name_of_its_own() {
        assert_eq!(dm_sandbox::PROJECTION, dm_server::projection::PROJECTIONS);
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("m");
        dm_server::Memory::init(&root, "human:test:me", "dom-me").unwrap();
        let creds = || Credentials {
            auth: AuthMode::Subscription,
            provider: None,
            server_auth: None,
            server_provider: None,
        };
        let at = |projected| prepare_launch(&root, Harness::Claude, creds(), None, None, projected);
        let base = root.join(dm_sandbox::PROJECTION).join("claude");
        let a = at(Projected::AsSession).await.unwrap();
        let b = at(Projected::AsSession).await.unwrap();
        assert_eq!(a.projection, base.join(a.memory.session_id()));
        assert_ne!(a.projection, b.projection, "two sessions, two renders");
        // the name the launcher outside mounts is the one the inside renders under
        let named = at(Projected::Named("run-7-1a2b")).await.unwrap();
        assert_eq!(named.projection, base.join("run-7-1a2b"));
        assert!(at(Projected::Named("../codex")).await.is_err());
        // a name whose directory another session already rendered into is not taken over
        let busy = base.join("run-8-busy");
        std::fs::create_dir_all(busy.join("dom-me")).unwrap();
        assert!(at(Projected::Named("run-8-busy")).await.is_err());
        assert!(busy.join("dom-me").exists(), "and is left as it was");
        // the empty one the launcher outside made for it is the session's to render into
        std::fs::create_dir_all(base.join("run-9-empty")).unwrap();
        assert!(at(Projected::Named("run-9-empty")).await.is_ok());
    }

    #[test]
    fn a_projection_name_from_the_command_line_is_one_component() {
        assert!(projection_name("run-4242").is_ok());
        assert!(projection_name("sess-ab12").is_ok());
        for bad in [
            "",
            "..",
            "../claude",
            "a/b",
            "/abs",
            "a\\b",
            "run 1",
            &"x".repeat(65),
        ] {
            assert!(projection_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn model_ids_are_read_from_either_listing_shape() {
        let openai = serde_json::json!({ "object": "list", "data": [{ "id": "b" }, { "id": "a" }, { "id": "a" }] });
        assert_eq!(model::model_ids(&openai), vec!["a", "b"]);
        let google = serde_json::json!({ "models": [{ "name": "models/gemini-2.5-flash" }] });
        assert_eq!(model::model_ids(&google), vec!["gemini-2.5-flash"]);
        assert!(model::model_ids(&serde_json::json!({})).is_empty());
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
    fn a_gateway_route_reshapes_what_its_translation_drops() {
        let gateway = with_env(&[("LLMAO_KEY", "pat")], || {
            route_for(Harness::Antigravity, AuthMode::Gateway, None)
        })
        .unwrap();
        assert!(gateway.destination.fold_function_responses);
        assert!(gateway.destination.fold_system_messages);
        let google = with_env(&[("GEMINI_API_KEY", "k")], || {
            route_for(Harness::Antigravity, AuthMode::Key, None)
        })
        .unwrap();
        assert!(
            !google.destination.fold_function_responses,
            "Google's own API takes a tool result as agy sends it"
        );
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
    fn a_model_is_named_as_the_harness_takes_it_on_the_route() {
        let at = |h, a| model_for(h, a, "llmao", "gemma4-26b");
        assert_eq!(
            at(Harness::Antigravity, AuthMode::Gateway),
            "gemini-api://gemma4-26b",
            "a gateway's model is outside Antigravity's catalog"
        );
        assert_eq!(at(Harness::Antigravity, AuthMode::Key), "gemma4-26b");
        assert_eq!(
            model_for(
                Harness::Antigravity,
                AuthMode::Gateway,
                "llmao",
                "gemini-api://x"
            ),
            "gemini-api://x",
            "named in full already"
        );
        assert_eq!(at(Harness::OpenCode, AuthMode::Gateway), "llmao/gemma4-26b");
        assert_eq!(at(Harness::Pi, AuthMode::Gateway), "llmao/gemma4-26b");
        assert_eq!(at(Harness::Gemini, AuthMode::Gateway), "gemma4-26b");
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn auto_takes_the_image_only_for_a_harness_it_carries() {
        let dir = tempfile::tempdir().unwrap();
        let r = image::rootfs_dir(dir.path());
        std::fs::create_dir_all(r.join("usr/local/bin")).unwrap();
        std::fs::create_dir_all(r.join("opt/memory")).unwrap();
        std::fs::write(r.join("opt/memory/memory"), "").unwrap();
        // npm's entries are links to its own tree, which the host need not have
        std::os::unix::fs::symlink(
            "/usr/local/lib/node_modules/claude/cli.js",
            r.join("usr/local/bin/claude"),
        )
        .unwrap();
        assert_eq!(
            backend_of(BackendArg::Auto, false, dir.path(), Harness::Claude),
            Backend::Image
        );
        // a harness the image does not carry runs from this host's install
        assert_eq!(
            backend_of(BackendArg::Auto, false, dir.path(), Harness::Antigravity),
            Backend::Namespaces
        );
        // a program is whatever the command names, wherever it lives
        assert_eq!(
            backend_of(BackendArg::Auto, false, dir.path(), Harness::Command),
            Backend::Image
        );
        // asked for by name, the image is what it gets
        assert_eq!(
            backend_of(BackendArg::Image, false, dir.path(), Harness::Antigravity),
            Backend::Image
        );
    }

    #[test]
    fn unconfined_wins_and_auto_prefers_the_image_when_unpacked() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            backend_of(BackendArg::Wslc, true, dir.path(), Harness::Claude),
            Backend::None
        );
        assert_eq!(
            backend_of(BackendArg::Wslc, false, dir.path(), Harness::Claude),
            Backend::Wslc
        );
        assert_eq!(
            backend_of(BackendArg::Image, false, dir.path(), Harness::Claude),
            Backend::Image
        );
        assert_eq!(
            backend_of(BackendArg::None, false, dir.path(), Harness::Claude),
            Backend::None
        );
        assert_eq!(
            backend_of(BackendArg::Namespaces, false, dir.path(), Harness::Claude),
            Backend::Namespaces
        );
        let auto = backend_of(BackendArg::Auto, false, dir.path(), Harness::Claude);
        if cfg!(target_os = "linux") {
            assert_eq!(auto, Backend::Namespaces, "no image unpacked yet");
            let r = image::rootfs_dir(dir.path());
            std::fs::create_dir_all(r.join("usr/local/bin")).unwrap();
            std::fs::create_dir_all(r.join("opt/memory")).unwrap();
            std::fs::write(r.join("opt/memory/memory"), "").unwrap();
            std::fs::write(r.join("usr/local/bin/claude"), "").unwrap();
            assert_eq!(
                backend_of(BackendArg::Auto, false, dir.path(), Harness::Claude),
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
