//! `amem reenact`: a session run again on its original prompt, at its original commit,
//! against its tape (the paper's live re-execution), to compare harness versions or
//! models on the same task.
//!
//! The memory server answers the re-run's tool calls from the tape while they match, by
//! tool, arguments and occurrence, and writes nothing; the first call that does not
//! match is the first divergence. After it a read is served as the record is now, a
//! write is captured for the report, and a call outside is refused. The working tree is
//! a detached checkout of the original commit beside the repository, removed after the
//! run, and the harness runs confined in it as a launch does, with a private home of
//! its own. Nothing is substituted into the harness's model calls.
//!
//! The terms the owner checked on 11 October 2026 set two rules here. One run on the
//! owner's own login, started by the owner, is ordinary use of the harness; repeated
//! runs belong on an API key or the gateway, so `--runs` above one refuses a
//! subscription. And a recorded session is never used to train a model: the runs are
//! reports, kept under `state/reenact/`, and nothing here exports a tape.

use crate::{
    backend_of, runner_for, AuthArg, AuthMode, BackendArg, Credentials, Fallible, Harness,
    HarnessArg, RunOptions, Session, SubagentsArg,
};
use clap::{Subcommand, ValueEnum};
use dm_server::tape::{Playback, Played, Tape};
use dm_server::Memory;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

type Fallibly<T> = Result<T, Box<dyn std::error::Error>>;

/// Where the runs of a session are a line each, under the root.
const RUNS: &str = "state/reenact";

/// What `amem reenact` does.
#[derive(Subcommand)]
pub enum ReenactOp {
    /// The tapes the record keeps: each session's prompt, harness, commit and calls.
    Tapes,
    /// One session's tape in full.
    Show {
        /// The session.
        session: String,
    },
    /// Keep a tape past the transcripts' clock, for a benchmark.
    Pin {
        /// The session.
        session: String,
    },
    /// Let a pinned tape go on the clock again.
    Unpin {
        /// The session.
        session: String,
    },
    /// Run a session again against its tape, and report how far it kept to it.
    Run(RunArgs),
    /// The runs of a session, side by side: each harness and model, against the
    /// original.
    Report {
        /// The session.
        session: String,
    },
}

/// `amem reenact run`.
#[derive(clap::Args)]
pub struct RunArgs {
    /// The session to re-enact.
    session: String,
    /// The harness; the original's by default.
    #[arg(long, value_enum)]
    harness: Option<HarnessArg>,
    /// The model; the original's by default.
    #[arg(long)]
    model: Option<String>,
    /// Whose credential the harness's calls go out on, said every time: a subscription
    /// login allows one run, started by you; repeated runs go on a key or the gateway.
    #[arg(long, value_enum)]
    auth: AuthArg,
    /// Where model calls go under --auth key, as for a launch.
    #[arg(long)]
    provider: Option<String>,
    /// How many runs.
    #[arg(long, default_value_t = 1)]
    runs: usize,
    /// The repository the session worked in, which holds its commit; the current
    /// directory by default.
    #[arg(long)]
    repo: Option<PathBuf>,
    /// A name for this side in the report, as `claude-2.1.296`.
    #[arg(long)]
    label: Option<String>,
    /// Where the harness runs, as for a launch.
    #[arg(long, value_enum, default_value = "auto")]
    backend: BackendArg,
    /// Run without confinement, as for a launch.
    #[arg(long)]
    unconfined: bool,
    /// Keep the checkout after the run, to look at what the re-run changed.
    #[arg(long)]
    keep_tree: bool,
}

/// How a run's checkout was made.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Tree {
    /// The commit, when the session recorded one.
    pub commit: Option<String>,
    /// Whether the original had changes the commit does not hold.
    pub original_dirty: bool,
    /// Where it was, when kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kept_at: Option<String>,
}

/// Tokens a run spent, as the egress metered them.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Tokens {
    /// Model calls.
    pub calls: u64,
    /// Input, cache reads excluded.
    pub input: u64,
    /// Output.
    pub output: u64,
    /// Cache reads.
    pub cache_read: u64,
}

/// One re-run, as the report keeps it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RunReport {
    /// This run.
    pub run: String,
    /// The session re-enacted.
    pub session: String,
    /// When it ran.
    pub at: String,
    /// The harness, by binary name.
    pub harness: String,
    /// The model it was given, if any.
    pub model: Option<String>,
    /// The side's name, when given.
    pub label: Option<String>,
    /// The re-run's own memory session.
    pub rerun_session: String,
    /// The harness's exit code.
    pub exit: i32,
    /// Calls on the tape.
    pub tape_calls: usize,
    /// Calls the re-run made.
    pub calls: usize,
    /// Calls before the first divergence.
    pub steps_to_divergence: usize,
    /// Whether it diverged.
    pub diverged: bool,
    /// The longest run of tools the two sequences share in order, by tool name.
    pub aligned: usize,
    /// `2 * aligned / (tape_calls + calls)`: one for the same sequence of tools.
    pub alignment: f64,
    /// The writes captured after the divergence, which were not applied.
    pub captured: Vec<serde_json::Value>,
    /// Whether the re-run left a handoff, from the tape or captured.
    pub handoff: bool,
    /// What the harness answered, its start.
    pub answer: String,
    /// Tokens.
    pub tokens: Tokens,
    /// Wall time.
    pub seconds: f64,
    /// The checkout.
    pub tree: Tree,
}

/// Run `amem reenact`.
///
/// # Errors
///
/// Anyone but the owner, a session with no tape, or a run that cannot start.
pub async fn cmd(root: &Path, op: ReenactOp) -> Fallible {
    match op {
        ReenactOp::Tapes => crate::json(&Memory::open(root)?.tapes().await?),
        ReenactOp::Show { session } => crate::json(&Memory::open(root)?.tape(&session).await?),
        ReenactOp::Pin { session } => {
            crate::json(&Memory::open(root)?.pin_tape(&session, true).await?)
        }
        ReenactOp::Unpin { session } => {
            crate::json(&Memory::open(root)?.pin_tape(&session, false).await?)
        }
        ReenactOp::Run(args) => run(root, args).await,
        ReenactOp::Report { session } => {
            print!("{}", report(root, &session)?);
            Ok(())
        }
    }
}

/// The harness a tape names, by its binary name.
fn harness_named(name: &str) -> Option<HarnessArg> {
    HarnessArg::value_variants()
        .iter()
        .copied()
        .find(|h| Harness::from(*h).binary() == name)
}

/// The longest common subsequence of two tool sequences, by length.
#[must_use]
pub fn aligned(a: &[String], b: &[String]) -> usize {
    let mut row = vec![0usize; b.len() + 1];
    for x in a {
        let mut diag = 0;
        for (j, y) in b.iter().enumerate() {
            let up = row[j + 1];
            row[j + 1] = if x == y {
                diag + 1
            } else {
                row[j + 1].max(row[j])
            };
            diag = up;
        }
    }
    row[b.len()]
}

/// What a run did against its tape, as numbers.
fn scored(played: &Played) -> (usize, f64, Vec<serde_json::Value>, bool) {
    let tools: Vec<String> = played.steps.iter().map(|s| s.tool.clone()).collect();
    let n = aligned(&played.tape_tools, &tools);
    let total = played.tape_tools.len() + tools.len();
    let alignment = if total == 0 {
        1.0
    } else {
        2.0 * n as f64 / total as f64
    };
    let captured = played
        .steps
        .iter()
        .filter(|s| s.served == "captured")
        .map(|s| serde_json::json!({ "tool": s.tool, "arguments": s.arguments }))
        .collect();
    let handoff = played.steps.iter().any(|s| {
        s.tool == "write"
            && (s.served == "tape" || s.served == "captured")
            && s.arguments
                .get("entry_type")
                .and_then(serde_json::Value::as_str)
                == Some("handoff")
    }) || played
        .steps
        .iter()
        .any(|s| s.tool == "complete" && (s.served == "tape" || s.served == "captured"));
    (n, alignment, captured, handoff)
}

/// Git, run on the host against a repository a confined harness could write: with an
/// environment of nothing but where git is and whose it is, so neither the launcher's
/// credentials nor a hook's own `GIT_*` reach it, and with the repository's hooks and
/// fsmonitor off, so nothing the harness planted in `.git` runs here.
fn git(repo: &Path, args: &[&str]) -> Fallibly<String> {
    let mut cmd = std::process::Command::new("git");
    cmd.env_clear();
    for keep in ["PATH", "HOME", "SYSTEMROOT", "USERPROFILE"] {
        if let Some(v) = std::env::var_os(keep) {
            cmd.env(keep, v);
        }
    }
    cmd.args([
        "-c",
        "core.fsmonitor=false",
        "-c",
        "core.hooksPath=/dev/null",
        "--no-optional-locks",
    ]);
    let out = cmd.arg("-C").arg(repo).args(args).output()?;
    if !out.status.success() {
        return Err(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )
        .into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The commit a working tree is at, and whether it has changes the commit lacks; none
/// for a directory that is not a git checkout.
#[must_use]
pub fn commit_of(workdir: &Path) -> Option<(String, bool)> {
    let commit = git(workdir, &["rev-parse", "HEAD"]).ok()?;
    let dirty = git(workdir, &["status", "--porcelain"]).is_ok_and(|s| !s.is_empty());
    Some((commit, dirty))
}

/// A clone of `repo` at `commit`, detached, beside the repository and named for the run:
/// a repository of its own, objects hardlinked rather than borrowed, so git works inside
/// the sandbox, which mounts the working tree and nothing of the repository it came from.
fn checkout(repo: &Path, commit: &str, run: &str) -> Fallibly<PathBuf> {
    let repo = std::fs::canonicalize(repo)?;
    let name = repo
        .file_name()
        .map_or_else(|| "repo".into(), |n| n.to_string_lossy().to_string());
    let parent = repo
        .parent()
        .ok_or("the repository has no parent directory")?;
    let into = parent.join(format!("{name}-reenact-{run}"));
    git(&repo, &["cat-file", "-e", &format!("{commit}^{{commit}}")]).map_err(|_| {
        format!(
            "{commit} is not in {}; pass --repo with the repository the session worked in",
            repo.display()
        )
    })?;
    if into.exists() {
        return Err(format!("{} is in the way", into.display()).into());
    }
    let to = into.display().to_string();
    git(&repo, &["clone", "--quiet", "--no-checkout", ".", &to])?;
    if let Err(e) = git(&into, &["checkout", "--quiet", "--detach", commit]) {
        remove_checkout(&into);
        return Err(e);
    }
    Ok(into)
}

fn remove_checkout(into: &Path) {
    if std::fs::remove_dir_all(into).is_err() {
        eprintln!("memory: the checkout at {} was left", into.display());
    }
}

fn now() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// The runs, refused where the terms say so, and the side they make.
fn check_runs(args: &RunArgs) -> Fallibly<()> {
    if args.runs == 0 {
        return Err("--runs is at least one".into());
    }
    if args.runs > 1 && AuthMode::from(args.auth) == AuthMode::Subscription {
        return Err(
            "repeated runs belong on an API key or the gateway, not a subscription login: \
             pass --auth key or --auth gateway, or run once"
                .into(),
        );
    }
    Ok(())
}

async fn run(root: &Path, args: RunArgs) -> Fallible {
    check_runs(&args)?;
    let tape = Memory::open(root)?.tape(&args.session).await?;
    let prompt = tape.header.start.prompt.clone().ok_or_else(|| {
        format!(
            "{} was interactive: its tape has no prompt to run again",
            args.session
        )
    })?;
    let harness = match args.harness {
        Some(h) => h,
        None => harness_named(&tape.header.start.harness).ok_or_else(|| {
            format!(
                "the tape names harness {:?}; pass --harness",
                tape.header.start.harness
            )
        })?,
    };
    let model = args
        .model
        .clone()
        .or_else(|| tape.header.start.model.clone());
    let repo = match &args.repo {
        Some(r) => r.clone(),
        None => std::env::current_dir()?,
    };
    for i in 1..=args.runs {
        let r = one(
            root,
            &args,
            &tape,
            &prompt,
            harness,
            model.clone(),
            &repo,
            i,
        )
        .await?;
        let dir = root.join(RUNS);
        std::fs::create_dir_all(&dir)?;
        dm_server::reads::append_line(
            &dir.join(format!("{}.jsonl", args.session)),
            &serde_json::to_string(&r)?,
        );
        println!("{}", serde_json::to_string_pretty(&r)?);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)] // one per thing a run is given
async fn one(
    root: &Path,
    args: &RunArgs,
    tape: &Tape,
    prompt: &str,
    harness: HarnessArg,
    model: Option<String>,
    repo: &Path,
    i: usize,
) -> Fallibly<RunReport> {
    let stamp = time::OffsetDateTime::now_utc().unix_timestamp();
    let run = format!("rx-{stamp}-{}-{i}", std::process::id());
    let start = &tape.header.start;
    let (workdir, scratch) = tree_for(repo, start.commit.as_deref(), &args.session, &run)?;
    let home = root.join(RUNS).join(&args.session).join(&run).join("home");
    let play = Arc::new(Playback::new(tape.clone()));
    let h: Harness = harness.into();
    // the tree is put away however the run ends
    let ran = run_once(
        root,
        args,
        h,
        model.clone(),
        &workdir,
        home.clone(),
        prompt,
        &play,
    )
    .await;
    let kept_at = put_away(&workdir, scratch, args.keep_tree);
    // the run's home held its login's copy and a transcript of what it was served
    if let Some(run_dir) = home.parent() {
        std::fs::remove_dir_all(run_dir).ok();
        // and the session's directory, once no other run is in it
        if let Some(session_dir) = run_dir.parent() {
            std::fs::remove_dir(session_dir).ok();
        }
    }
    let (exit, output, rerun_session, tokens, seconds) = ran?;
    let played = play.played();
    let (aligned, alignment, captured, handoff) = scored(&played);
    Ok(RunReport {
        run,
        session: args.session.clone(),
        at: now(),
        harness: h.binary().to_string(),
        model,
        label: args.label.clone(),
        rerun_session,
        exit,
        tape_calls: played.tape_calls,
        calls: played.steps.len(),
        steps_to_divergence: played.steps_to_divergence,
        diverged: played.diverged,
        aligned,
        alignment,
        captured,
        handoff,
        answer: output.unwrap_or_default().chars().take(2_000).collect(),
        tokens,
        seconds,
        tree: Tree {
            commit: start.commit.clone(),
            original_dirty: start.dirty,
            kept_at,
        },
    })
}

/// One run of the harness against `play`, in `workdir`: its exit code, its answer, its
/// memory session, what it spent, and how long it took.
#[allow(clippy::too_many_arguments)] // one per thing a run is given
async fn run_once(
    root: &Path,
    args: &RunArgs,
    h: Harness,
    model: Option<String>,
    workdir: &Path,
    home: PathBuf,
    prompt: &str,
    play: &Arc<Playback>,
) -> Fallibly<(i32, Option<String>, String, Tokens, f64)> {
    std::fs::create_dir_all(&home)?;
    let session = Session::start(
        root,
        h,
        Credentials {
            auth: args.auth.into(),
            provider: args.provider.as_deref(),
            server_auth: None,
            server_provider: None,
        },
        workdir.to_path_buf(),
        runner_for(backend(args, root, h)?, root)?,
        RunOptions::new(model, SubagentsArg::Harness, Vec::new(), None).home(home),
    )
    .await?;
    session.memory().set_playback(play.clone());
    session.memory().disable_read_log();
    eprintln!(
        "memory: re-enacting {} ({} calls on its tape) as {}",
        args.session,
        play.played().tape_calls,
        session.memory().session_id()
    );
    let began = std::time::Instant::now();
    let ran = session.run(&h.headless(prompt), true).await;
    let seconds = began.elapsed().as_secs_f64();
    let id = session.memory().session_id().to_string();
    let tokens = tokens_of(&session, &id);
    let code = ran.as_ref().map_or(1, |(c, _)| *c);
    session.finish(code).await;
    let (exit, output) = ran?;
    Ok((exit, output, id, tokens, seconds))
}

/// Where a re-run's harness runs: as a launch's does, except that the `wslc` backend,
/// which a launch hands to its own path, is not one a re-run takes yet; refused rather
/// than run unconfined.
fn backend(args: &RunArgs, root: &Path, h: Harness) -> Fallibly<crate::Backend> {
    let b = backend_of(args.backend, args.unconfined, root, h);
    if b == crate::Backend::Wslc {
        return Err(
            "a re-run does not take the wslc backend yet; run it under WSL with \
                    --backend namespaces or image, or pass --unconfined"
                .into(),
        );
    }
    Ok(b)
}

/// The working tree for a run: a checkout of the commit, or an empty directory made for
/// the run when the session recorded none, which is the second of the pair.
fn tree_for(
    repo: &Path,
    commit: Option<&str>,
    session: &str,
    run: &str,
) -> Fallibly<(PathBuf, Option<PathBuf>)> {
    if let Some(c) = commit {
        return Ok((checkout(repo, c, &format!("{session}-{run}"))?, None));
    }
    eprintln!("memory: {session} recorded no commit; the re-run works in an empty directory");
    // as an empty record is made for an evaluation's baseline
    let d = std::env::temp_dir().join(format!("amem-reenact-{}-{run}", std::process::id()));
    std::fs::create_dir_all(&d)?;
    Ok((d.clone(), Some(d)))
}

/// Put a run's tree away: an empty directory goes, a checkout is removed unless asked to
/// stay. Where it stayed, if it did.
fn put_away(workdir: &Path, scratch: Option<PathBuf>, keep: bool) -> Option<String> {
    if let Some(d) = scratch {
        std::fs::remove_dir_all(d).ok();
        return None;
    }
    if keep {
        return Some(workdir.display().to_string());
    }
    remove_checkout(workdir);
    None
}

/// What the egress metered for the re-run's session.
fn tokens_of(session: &Session, id: &str) -> Tokens {
    session
        .egress()
        .telemetry()
        .session(id)
        .ok()
        .flatten()
        .map(|t| Tokens {
            calls: t.calls,
            input: t.input_tokens,
            output: t.output_tokens,
            cache_read: t.cache_read_tokens,
        })
        .unwrap_or_default()
}

/// Drop what the re-runs of `session` left under the root: their reports and any run
/// directory an interrupted run did not put away. With the tape, on its clock.
pub fn forget_runs(root: &Path, session: &str) {
    if !session
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return;
    }
    let dir = root.join(RUNS);
    std::fs::remove_file(dir.join(format!("{session}.jsonl"))).ok();
    std::fs::remove_dir_all(dir.join(session)).ok();
}

/// The runs of a session, read back.
fn runs_of(root: &Path, session: &str) -> Fallibly<Vec<RunReport>> {
    if !session
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!("{session:?} is not a session id").into());
    }
    let path = root.join(RUNS).join(format!("{session}.jsonl"));
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    Ok(text
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect())
}

fn mean(xs: impl Iterator<Item = f64>) -> f64 {
    let (n, sum) = xs.fold((0usize, 0.0), |(n, s), x| (n + 1, s + x));
    if n == 0 {
        0.0
    } else {
        sum / n as f64
    }
}

/// The report: one line per side, a side being the harness, model and label.
///
/// # Errors
///
/// A run log that cannot be read, or a session id that is not one.
pub fn report(root: &Path, session: &str) -> Fallibly<String> {
    let runs = runs_of(root, session)?;
    let mut sides: Vec<(String, Vec<&RunReport>)> = Vec::new();
    for r in &runs {
        let side = format!(
            "{}{}{}",
            r.harness,
            r.model
                .as_deref()
                .map(|m| format!(" {m}"))
                .unwrap_or_default(),
            r.label
                .as_deref()
                .map(|l| format!(" ({l})"))
                .unwrap_or_default()
        );
        match sides.iter_mut().find(|(s, _)| *s == side) {
            Some((_, v)) => v.push(r),
            None => sides.push((side, vec![r])),
        }
    }
    let mut s = format!("Re-enactments of {session}: {} runs\n", runs.len());
    if let Some(first) = runs.first() {
        s.push_str(&format!("  the tape: {} calls\n", first.tape_calls));
    }
    s.push_str(
        "  side                                  runs  to divergence  alignment  exit 0  handoff   tokens in/out      seconds\n",
    );
    for (side, rs) in &sides {
        let n = rs.len() as f64;
        let ok = rs.iter().filter(|r| r.exit == 0).count();
        let handed = rs.iter().filter(|r| r.handoff).count();
        s.push_str(&format!(
            "  {:<37} {:>4}  {:>13.1}  {:>9.2}  {:>3}/{:<2}  {:>3}/{:<2}  {:>8.0}/{:<8.0}  {:>7.1}\n",
            side,
            rs.len(),
            mean(rs.iter().map(|r| r.steps_to_divergence as f64)),
            mean(rs.iter().map(|r| r.alignment)),
            ok,
            n as usize,
            handed,
            n as usize,
            mean(rs.iter().map(|r| r.tokens.input as f64)),
            mean(rs.iter().map(|r| r.tokens.output as f64)),
            mean(rs.iter().map(|r| r.seconds)),
        ));
    }
    if runs.is_empty() {
        s.push_str("  no runs yet; amem reenact run <session>\n");
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dm_server::tape::{Kind, TapeCall, TapeHeader, TapeStart};
    use serde_json::json;

    #[test]
    fn alignment_is_the_longest_shared_order_of_tools() {
        let v = |xs: &[&str]| xs.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        assert_eq!(
            aligned(
                &v(&["t0", "search", "get", "write"]),
                &v(&["t0", "get", "write"])
            ),
            3
        );
        assert_eq!(aligned(&v(&["a", "b"]), &v(&["b", "a"])), 1);
        assert_eq!(aligned(&v(&[]), &v(&["a"])), 0);
    }

    fn tape() -> Tape {
        let call = |n: usize, tool: &str, arguments: serde_json::Value| TapeCall {
            n,
            at: now(),
            tool: tool.into(),
            arguments,
            occurrence: 1,
            label: dm_core::Label::Internal,
            result: json!({ "content": [] }),
            is_error: false,
        };
        Tape {
            header: TapeHeader {
                session: "sess-a".into(),
                domain: "dom-me".into(),
                started_at: now(),
                start: TapeStart {
                    harness: "agy".into(),
                    prompt: Some("Finish the import.".into()),
                    ..TapeStart::default()
                },
                pinned: false,
            },
            calls: vec![
                call(1, "t0", json!({})),
                call(2, "search", json!({"query": "import"})),
                call(
                    3,
                    "write",
                    json!({"body": "Done.", "entry_type": "handoff"}),
                ),
            ],
        }
    }

    #[test]
    fn a_run_is_scored_against_its_tape() {
        let p = Playback::new(tape());
        p.answer("t0", &json!({}), Kind::Read);
        p.answer("search", &json!({"query": "other"}), Kind::Read);
        p.answer(
            "write",
            &json!({"body": "Stopped at four.", "entry_type": "handoff"}),
            Kind::Mutation,
        );
        let played = p.played();
        let (n, alignment, captured, handoff) = scored(&played);
        assert_eq!(played.steps_to_divergence, 1);
        assert_eq!(n, 3, "the same tools in the same order");
        assert!((alignment - 1.0).abs() < f64::EPSILON);
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0]["arguments"]["body"], "Stopped at four.");
        assert!(handoff, "captured, but left");
    }

    #[test]
    fn a_tape_names_its_harness_by_binary_and_repeated_runs_need_a_key() {
        assert!(matches!(
            harness_named("agy"),
            Some(HarnessArg::Antigravity)
        ));
        assert!(harness_named("nope").is_none());
        let args = |runs: usize, auth: AuthArg| RunArgs {
            session: "sess-a".into(),
            harness: None,
            model: None,
            auth,
            provider: None,
            runs,
            repo: None,
            label: None,
            backend: BackendArg::Auto,
            unconfined: false,
            keep_tree: false,
        };
        assert!(check_runs(&args(1, AuthArg::Subscription)).is_ok());
        let e = check_runs(&args(3, AuthArg::Subscription)).unwrap_err();
        assert!(e.to_string().contains("API key or the gateway"), "{e}");
        assert!(check_runs(&args(3, AuthArg::Key)).is_ok());
        assert!(check_runs(&args(0, AuthArg::Key)).is_err());
    }

    #[test]
    fn the_report_puts_each_side_on_a_line() {
        let dir = tempfile::tempdir().unwrap();
        let r = |harness: &str, model: &str, steps: usize, exit: i32| RunReport {
            run: "rx-1".into(),
            session: "sess-a".into(),
            at: now(),
            harness: harness.into(),
            model: Some(model.into()),
            label: None,
            rerun_session: "sess-b".into(),
            exit,
            tape_calls: 4,
            calls: 4,
            steps_to_divergence: steps,
            diverged: steps < 4,
            aligned: 4,
            alignment: 1.0,
            captured: Vec::new(),
            handoff: true,
            answer: String::new(),
            tokens: Tokens::default(),
            seconds: 2.0,
            tree: Tree {
                commit: None,
                original_dirty: false,
                kept_at: None,
            },
        };
        let path = dir.path().join(RUNS).join("sess-a.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let lines: Vec<String> = [
            r("claude", "opus", 4, 0),
            r("claude", "opus", 2, 0),
            r("codex", "gpt", 1, 1),
        ]
        .iter()
        .map(|x| serde_json::to_string(x).unwrap())
        .collect();
        std::fs::write(&path, lines.join("\n")).unwrap();
        let text = report(dir.path(), "sess-a").unwrap();
        assert!(
            text.contains("3 runs") && text.contains("the tape: 4 calls"),
            "{text}"
        );
        let claude = text.lines().find(|l| l.contains("claude opus")).unwrap();
        assert!(
            claude.contains("   2 ") && claude.contains("3.0"),
            "{claude}"
        );
        assert!(
            text.lines()
                .any(|l| l.contains("codex gpt") && l.contains("0/1")),
            "{text}"
        );
        assert!(report(dir.path(), "../x").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn git_on_the_host_runs_nothing_the_harness_planted_and_carries_no_credential() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("r");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"]).unwrap();
        git(
            &repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.org",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "c",
            ],
        )
        .unwrap();
        // what a confined harness could leave in the working tree's .git
        let marker = dir.path().join("ran");
        let script = dir.path().join("planted.sh");
        std::fs::write(&script, format!("#!/bin/sh\nenv > {}\n", marker.display())).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let config = repo.join(".git/config");
        let mut text = std::fs::read_to_string(&config).unwrap();
        text.push_str(&format!("[core]\n\tfsmonitor = {}\n", script.display()));
        std::fs::write(&config, text).unwrap();
        std::fs::copy(&script, repo.join(".git/hooks/post-checkout")).unwrap();
        std::fs::write(repo.join("new.txt"), "x").unwrap();
        let (commit, dirty) = commit_of(&repo).unwrap();
        assert_eq!(commit.len(), 40);
        assert!(dirty);
        let into = checkout(&repo, &commit, "t").unwrap();
        assert!(!marker.exists(), "a planted fsmonitor or hook ran");
        remove_checkout(&into);
    }
}
