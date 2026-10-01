//! The `memory` binary: the per-session memory server and its command line.
//!
//! Every subcommand is a thin call into `dm_server::Memory`; the MCP server and the
//! command line see the same operations. The one thing the binary does that the library
//! does not is resolve its mounts at startup: `--root`, or `MEMORY_ROOT`.

use clap::{Parser, Subcommand, ValueEnum};
use dm_core::{EntryType, Label};
use dm_ledger::{Status, TaskSpec};
use dm_server::{mcp, Memory, WriteRequest};
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use time::Duration;

#[derive(Parser)]
#[command(
    name = "memory",
    version,
    about = "Durable memory for disposable agents: one root, served over MCP or driven from the command line."
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
        Command::Task { op } => task(&Memory::open(&root)?, op).await,
        other => entries(&Memory::open(&root)?, other).await,
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
