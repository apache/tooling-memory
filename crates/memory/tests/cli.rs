//! The seven M0 steps from PLAN.md, run against the built binary. This is the demo in
//! test form: if it passes, someone can clone, build, and do the same by hand.

// An integration test is test code, but clippy's allow-unwrap-in-tests only sees
// #[cfg(test)] modules; here a panic is the failure being reported.
#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn memory(root: &Path, args: &[&str], stdin: Option<&str>) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_amem"));
    cmd.arg("--root").arg(root).args(args);
    cmd.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn memory");
    if let Some(text) = stdin {
        use std::io::Write;
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(text.as_bytes())
            .expect("write stdin");
    }
    child.wait_with_output().expect("wait")
}

fn ok(root: &Path, args: &[&str], stdin: Option<&str>) -> String {
    let out = memory(root, args, stdin);
    assert!(
        out.status.success(),
        "memory {args:?} failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf-8 stdout")
}

fn fails(root: &Path, args: &[&str]) -> String {
    let out = memory(root, args, None);
    assert!(!out.status.success(), "memory {args:?} should have failed");
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn json(text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("not json ({e}): {text}"))
}

fn id_of(text: &str) -> String {
    json(text)["id"].as_str().expect("id").to_string()
}

/// What the steps hand to one another.
struct Demo {
    dir: tempfile::TempDir,
    root: PathBuf,
    decision: String,
    handoff: String,
}

/// 1. init creates a domain with a grants file.
fn step_init() -> Demo {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("memory");
    let out = ok(
        &root,
        &["init", "--owner", "human:test:me", "--domain", "dom-me"],
        None,
    );
    assert!(out.contains("dom-me"), "{out}");
    assert!(root.join("grants.toml").is_file());
    assert!(fails(&root, &["init"]).contains("already exists"));
    Demo {
        dir,
        root,
        decision: String::new(),
        handoff: String::new(),
    }
}

/// 2. a session writes a decision and a handoff.
fn step_write(d: &mut Demo) {
    d.decision = id_of(&ok(
        &d.root,
        &[
            "write",
            "--kind",
            "decision",
            "--title",
            "Use SQLite for the pilot",
        ],
        Some("Nothing to operate, and FTS5 is built in.\n"),
    ));
    d.handoff = id_of(&ok(
        &d.root,
        &[
            "write",
            "--kind",
            "handoff",
            "--title",
            "Stopped at step 3 of the import",
        ],
        Some("Steps 1 and 2 done; the manifest parses. Next: digests.\n"),
    ));
}

/// 3. a fresh session loads T0 and finds the handoff without any transcript.
fn step_t0(d: &Demo) {
    let t0 = ok(&d.root, &["t0"], None);
    assert!(t0.contains(&d.handoff), "{t0}");
    assert!(t0.contains("Next: digests."), "{t0}");
    assert!(t0.contains(&d.decision), "{t0}");
    assert!(t0.contains("write a `handoff` entry"), "{t0}");
}

/// 4. search ranks and returns summaries; get returns one body.
fn step_search_and_get(d: &Demo) {
    let hits = json(&ok(&d.root, &["search", "digests"], None));
    assert_eq!(hits[0]["id"], d.handoff);
    assert_eq!(hits[0]["type"], "handoff");
    assert!(hits[0]["summary"]
        .as_str()
        .unwrap()
        .contains("manifest parses"));
    let body = ok(&d.root, &["get", &d.handoff], None);
    assert!(body.starts_with("---\nid: "), "{body}");
    assert!(body.contains("Next: digests."));
    assert!(fails(&d.root, &["get", "mem-me-nope"]).contains("not found"));
}

/// 4b. recall with no model behind it, and a golden suite scored against the record.
fn step_recall_and_golden(d: &Demo) {
    // recall with no model behind it: the pack comes back with synthesis owed
    let r = json(&ok(
        &d.root,
        &["recall", "what is next after the manifest"],
        None,
    ));
    assert_eq!(r["synthesis"]["by"], "owed", "{r}");
    assert_eq!(r["pack"]["depth"], "sections");
    let pieces = r["pack"]["pieces"].as_array().unwrap();
    assert!(pieces.iter().any(|p| p["id"] == d.handoff), "{r}");
    assert!(pieces
        .iter()
        .any(|p| p["body"].as_str().is_some_and(|b| b.contains("digests"))));
    assert!(r["citations"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c == &d.handoff));
    assert!(fails(&d.root, &["recall", "q", "--model", "m"]).contains("provider"));
    assert!(fails(
        &d.root,
        &[
            "recall",
            "q",
            "--provider",
            "anthropic",
            "--auth",
            "subscription"
        ]
    )
    .contains("subscription"));
    // taps: a name is checked before it becomes a path, and a run needs a grant
    assert!(fails(&d.root, &["tap", "install", "Bad Name", "x.wasm"]).contains("name"));
    assert!(fails(&d.root, &["tap", "run", "github-issues"]).contains("grants.toml has no"));
    // the savings report runs from the CLI and counts that recall
    let savings = ok(&d.root, &["savings", "--query", "digests"], None);
    assert!(savings.contains("owed packs             1"), "{savings}");
    // a golden suite scores the question against the record with no model behind it
    let suite = d.dir.path().join("golden.toml");
    std::fs::write(
        &suite,
        "[[question]]\nask = \"what comes after the manifest\"\nanswers = [\"step 3 of the import\"]\nkeywords = \"digests\"\n",
    )
    .unwrap();
    let g = json(&ok(
        &d.root,
        &["golden", suite.to_str().unwrap(), "--json"],
        None,
    ));
    assert_eq!(g["questions"], 1, "{g}");
    let kw = g["scores"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["strategy"] == "keywords")
        .unwrap();
    assert_eq!(kw["hits"], 1, "{g}");
}

/// 5. a task is created, claimed with a lease, heartbeated, completed; a second claim
///    while the lease holds finds nothing.
fn step_tasks(d: &Demo) {
    let task = json(&ok(
        &d.root,
        &["task", "create", "Finish the import", "--priority", "5"],
        None,
    ));
    let id = task["id"].as_str().unwrap().to_string();
    assert_eq!(task["status"], "proposed");
    assert_eq!(ok(&d.root, &["task", "claim-next"], None).trim(), "null");
    ok(&d.root, &["task", "admit", &id], None);
    let claimed = json(&ok(&d.root, &["task", "claim-next"], None));
    assert_eq!(claimed["id"], id);
    assert_eq!(claimed["status"], "claimed");
    assert_eq!(
        ok(&d.root, &["task", "claim-next"], None).trim(),
        "null",
        "one holder"
    );
    assert_eq!(claimed["generation"], 1, "the first claim");
    // a command that names no generation, or an earlier one, holds nothing
    fails(&d.root, &["task", "heartbeat", &id]);
    let why = fails(&d.root, &["task", "heartbeat", &id, "--generation", "0"]);
    assert!(why.contains("generation"), "{why}");
    ok(
        &d.root,
        &["task", "heartbeat", &id, "--generation", "1"],
        None,
    );
    let done = json(&ok(
        &d.root,
        &["task", "complete", &id, &d.handoff, "--generation", "1"],
        None,
    ));
    assert_eq!(done["status"], "done");
    assert_eq!(done["handoff"], d.handoff);
    let completed = json(&ok(&d.root, &["task", "list", "--status", "done"], None));
    assert_eq!(completed.as_array().unwrap().len(), 1);
}

/// 6. export writes a bundle; import lands it raw elsewhere and refuses to import twice.
fn step_export_import(d: &Demo) {
    let bundle = d.dir.path().join("bundle");
    let bundle_arg = bundle.to_str().unwrap();
    let out = ok(
        &d.root,
        &["export", bundle_arg, "--max-label", "internal"],
        None,
    );
    assert!(out.contains("exported 2 entries"), "{out}");
    assert!(bundle.join("manifest.yaml").is_file());
    let other = d.dir.path().join("other");
    ok(
        &other,
        &[
            "init",
            "--owner",
            "human:test:other",
            "--domain",
            "dom-other",
        ],
        None,
    );
    let out = ok(&other, &["import", bundle_arg], None);
    assert!(
        out.contains("imported 2 entries with 0 transcript excerpts, skipped 0"),
        "{out}"
    );
    let out = ok(&other, &["import", bundle_arg], None);
    assert!(
        out.contains("imported 0 entries with 0 transcript excerpts, skipped 2"),
        "{out}"
    );
    let theirs = ok(&other, &["get", &d.handoff], None);
    assert!(theirs.contains("trust_tier: raw"), "{theirs}");
    assert!(theirs.contains("domain: dom-other"), "{theirs}");
    assert!(
        theirs.contains("author: human:test:me"),
        "provenance kept: {theirs}"
    );
}

/// 7. delete the index and rebuild it from the record alone.
fn step_reindex(d: &Demo) {
    std::fs::remove_file(d.root.join("state/index.db")).unwrap();
    let gone = json(&ok(&d.root, &["search", "digests"], None));
    assert_eq!(gone.as_array().unwrap().len(), 0);
    let out = ok(&d.root, &["reindex"], None);
    assert!(out.contains("indexed 2 entries"), "{out}");
    let hits = json(&ok(&d.root, &["search", "digests"], None));
    assert_eq!(hits[0]["id"], d.handoff);
    let list = json(&ok(&d.root, &["list", "--kind", "handoff"], None));
    assert_eq!(list.as_array().unwrap().len(), 1);
}

#[test]
fn the_seven_m0_steps() {
    let mut d = step_init();
    step_write(&mut d);
    step_t0(&d);
    step_search_and_get(&d);
    step_recall_and_golden(&d);
    step_tasks(&d);
    step_export_import(&d);
    step_reindex(&d);
}

#[test]
fn erasure_leaves_a_stub_a_receipt_and_a_flag_on_the_citing_entry() {
    let mut d = step_init();
    step_write(&mut d);
    let marker = "passphrase-Qz7xx";
    let secret = id_of(&ok(
        &d.root,
        &["write", "--title", "Vault"],
        Some(&format!("The vault opens with {marker}.\n")),
    ));
    let citing = id_of(&ok(
        &d.root,
        &["write", "--title", "Where the vault is explained"],
        Some(&format!("See {secret}.\n")),
    ));
    // the query is a word, not the secret: the read log keeps queries
    let ids = |text: &str| -> Vec<String> {
        json(text)
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["id"].as_str().unwrap().to_string())
            .collect()
    };
    assert!(ids(&ok(&d.root, &["search", "vault opens"], None)).contains(&secret));
    // a why that quotes the secret is the entry's too, and leaves with it
    let why = format!("tried {marker} on the vault");
    ok(
        &d.root,
        &["promote", &secret, "--to", "curated", "--why", &why],
        None,
    );
    let err = fails(&d.root, &["erase", &secret, "--reason", "because"]);
    assert!(err.contains("invalid value"), "{err}");
    let receipt = json(&ok(
        &d.root,
        &["erase", &secret, "--reason", "sensitive"],
        None,
    ));
    assert_eq!(receipt["id"], secret);
    assert_eq!(receipt["reason"], "sensitive");
    assert_eq!(receipt["removed"][0]["version"], "current");
    assert_eq!(receipt["flagged"], serde_json::json!([citing]));
    assert_eq!(receipt["promotion_whys_removed"], 1);
    assert!(!ids(&ok(&d.root, &["search", "vault opens"], None)).contains(&secret));
    let err = fails(&d.root, &["get", &secret]);
    assert!(err.contains("gone") && err.contains("erased"), "{err}");
    let err = fails(&d.root, &["erase", &secret, "--reason", "legal"]);
    assert!(err.contains("already erased"), "{err}");
    let stub = std::fs::read_to_string(d.root.join(format!("record/dom-me/{secret}.md"))).unwrap();
    assert!(
        stub.contains("status: erased") && !stub.contains(marker),
        "{stub}"
    );
    let receipt_file = d
        .root
        .join(format!("record/dom-me/.erasures/{secret}.yaml"));
    assert!(receipt_file.is_file());
    if let Some(file) = holds(&d.root, marker.as_bytes()) {
        panic!("{} still holds the erased body", file.display());
    }
}

#[test]
fn promotions_says_why_an_entry_was_promoted() {
    let mut d = step_init();
    step_write(&mut d);
    let why = "read it against the pilot's notes";
    let promoted = json(&ok(
        &d.root,
        &["promote", &d.decision, "--to", "curated", "--why", why],
        None,
    ));
    assert_eq!(promoted["tier"], "curated");
    let records = json(&ok(&d.root, &["promotions", &d.decision], None));
    let records = records.as_array().unwrap();
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["why"], why);
    assert_eq!(records[0]["tier"], "curated");
    assert_eq!(records[0]["by"], "human:test:me");
    assert_eq!(records[0]["landed"], true);
    let none = json(&ok(&d.root, &["promotions", &d.handoff], None));
    assert_eq!(none, serde_json::json!([]));
    assert!(fails(&d.root, &["promotions", "mem-none"]).contains("not found"));
}

#[test]
fn an_erasure_takes_the_proposal_to_verify_the_entry_off_the_board() {
    let d = step_init();
    let id = id_of(&ok(
        &d.root,
        &["write", "--title", "A fact"],
        Some("A fact worth verifying, then erased.\n"),
    ));
    let proposal = json(&ok(
        &d.root,
        &["promote", &id, "--to", "verified", "--why", "checked"],
        None,
    ));
    assert_eq!(proposal["outcome"], "proposed", "{proposal}");
    let task = proposal["task"]["id"].as_str().unwrap().to_string();
    ok(&d.root, &["erase", &id, "--reason", "mistaken"], None);
    let listed = |status: &str| -> Vec<String> {
        json(&ok(&d.root, &["task", "list", "--status", status], None))
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["id"].as_str().unwrap().to_string())
            .collect()
    };
    for open in ["proposed", "open", "claimed"] {
        assert!(
            !listed(open).contains(&task),
            "{task} is still {open} after the erasure"
        );
    }
    assert!(listed("invalidated").contains(&task));
    let err = fails(&d.root, &["approve", &task]);
    assert!(err.contains("erased"), "{err}");
    let err = fails(
        &d.root,
        &["promote", &id, "--to", "verified", "--why", "again"],
    );
    assert!(err.contains("erased"), "{err}");
}

#[test]
fn a_declined_proposal_is_not_printed_as_proposed_again() {
    let d = step_init();
    let id = id_of(&ok(
        &d.root,
        &["write", "--title", "A fact"],
        Some("A fact a person declines.\n"),
    ));
    let proposal = json(&ok(
        &d.root,
        &["promote", &id, "--to", "verified", "--why", "checked"],
        None,
    ));
    let task = proposal["task"]["id"].as_str().unwrap().to_string();
    ok(
        &d.root,
        &["task", "invalidate", &task, "--reason", "does not hold"],
        None,
    );
    let out = memory(
        &d.root,
        &["promote", &id, "--to", "verified", "--why", "again"],
        None,
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("proposed"),
        "nothing was filed, and the command says: {stdout}"
    );
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains(&task), "{stderr}");
}

/// The first file under `root` whose bytes contain `needle`.
fn holds(root: &Path, needle: &[u8]) -> Option<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for f in std::fs::read_dir(&dir).unwrap().flatten() {
            if f.path().is_dir() {
                stack.push(f.path());
            } else if std::fs::read(f.path())
                .unwrap()
                .windows(needle.len())
                .any(|w| w == needle)
            {
                return Some(f.path());
            }
        }
    }
    None
}

#[test]
fn no_root_is_a_clear_error() {
    // with no --root and no MEMORY_ROOT the root is ~/memory, and a missing one says so
    let home = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_amem"))
        .env_remove("MEMORY_ROOT")
        .env("HOME", home.path())
        .arg("t0")
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains(&home.path().join("memory").display().to_string()),
        "{err}"
    );
    assert!(err.contains("amem init") && err.contains("--root"), "{err}");
    // and with no home either, there is nowhere to default to
    let out = Command::new(env!("CARGO_BIN_EXE_amem"))
        .env_remove("MEMORY_ROOT")
        .env_remove("HOME")
        .env_remove("USERPROFILE")
        .arg("t0")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("give --root"));
}

/// The body of an entry that defines the position `pos-release`, which `codex` fills.
const RELEASE: &str = "# The release coordinator\n\n```position pos-release\n[charter]\norders = \"Each release is cut from main, after the mirrors are checked.\"\ncovers = []\nagent = \"codex\"\n\n[envelope]\ntools = []\nhandling = \"restricted\"\n```\n";

/// What one session of `amem serve` did.
struct Served {
    /// For each call, whether it was allowed, and the result or the refusal as text.
    calls: Vec<(bool, String)>,
    /// What the process wrote on standard error.
    stderr: String,
}

impl Served {
    /// The result of call `n`, which was allowed, as JSON.
    fn result(&self, n: usize) -> serde_json::Value {
        let (allowed, text) = &self.calls[n];
        assert!(allowed, "call {n} was refused: {text}\n{}", self.stderr);
        json(text)
    }
}

/// Tool calls in one session of `amem serve` as `author`, as a harness makes them over
/// stdio. `flags` follow `serve`, and `env` goes into the environment of the process.
fn serve(
    root: &Path,
    author: &str,
    flags: &[&str],
    env: &[(&str, &str)],
    calls: &[(&str, serde_json::Value)],
) -> Served {
    use std::io::{BufRead, BufReader, Read, Write};
    let mut child = Command::new(env!("CARGO_BIN_EXE_amem"))
        .arg("--root")
        .arg(root)
        .args(["serve", "--author", author])
        .args(flags)
        .envs(env.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn amem serve");
    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));
    let mut send = |m: serde_json::Value| writeln!(stdin, "{m}").expect("write to amem serve");
    let mut line = String::new();
    send(
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                   "clientInfo": {"name": "test", "version": "0"}}}),
    );
    stdout.read_line(&mut line).expect("the handshake");
    send(serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    let mut done = Vec::new();
    for (n, (tool, arguments)) in calls.iter().enumerate() {
        send(
            serde_json::json!({"jsonrpc": "2.0", "id": n + 2, "method": "tools/call",
            "params": {"name": tool, "arguments": arguments}}),
        );
        line.clear();
        stdout.read_line(&mut line).expect("an answer");
        let r = json(&line);
        if let Some(e) = r.get("error") {
            done.push((false, e["message"].as_str().unwrap_or_default().to_string()));
            continue;
        }
        let text: Vec<&str> = r["result"]["content"]
            .as_array()
            .map(|c| c.iter().filter_map(|c| c["text"].as_str()).collect())
            .unwrap_or_default();
        done.push((r["result"]["isError"] != true, text.join(" ")));
    }
    drop(send);
    drop(stdin);
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("stderr")
        .read_to_string(&mut stderr)
        .expect("read stderr");
    child.wait().expect("wait for amem serve");
    Served {
        calls: done,
        stderr,
    }
}

/// Write the position `pos-release` as the owner, through `amem write`. Returns its entry.
fn write_position(root: &Path) -> String {
    id_of(&ok(
        root,
        &[
            "write",
            "--kind",
            "position",
            "--title",
            "The release coordinator",
        ],
        Some(RELEASE),
    ))
}

/// Verify an entry through the commands: an agent proposes it over `amem serve`, and
/// the owner approves the proposal.
fn verify(root: &Path, entry: &str) {
    let proposed = serve(
        root,
        "agent:curator",
        &[],
        &[],
        &[(
            "promote",
            serde_json::json!({"id": entry, "to": "verified", "why": "read against the charter"}),
        )],
    );
    let task = proposed.result(0)["task"]["id"]
        .as_str()
        .expect("a proposal task")
        .to_string();
    ok(root, &["approve", &task], None);
}

/// File a task pinned to `pos-release` for continuity, and admit it.
fn pinned_task(root: &Path, title: &str) -> String {
    let filed = json(&ok(
        root,
        &[
            "task",
            "create",
            title,
            "--body",
            "Tell the list.",
            "--position",
            "pos-release",
            "--pin-reason",
            "continuity",
        ],
        None,
    ));
    assert_eq!(filed["position"], "pos-release", "{filed}");
    assert_eq!(filed["pin_reason"], "continuity", "{filed}");
    let id = filed["id"].as_str().expect("id").to_string();
    ok(root, &["task", "admit", &id], None);
    id
}

fn positions(root: &Path) -> serde_json::Value {
    json(&ok(root, &["position", "list"], None))
}

/// A position's entry is listed by its kind, and no position is in force before a
/// person verifies the entry. Returns the entry in force after that.
fn step_position_review_gate(root: &Path) -> String {
    let entry = write_position(root);
    let kinds = json(&ok(root, &["list", "--kind", "position"], None));
    assert_eq!(kinds[0]["id"], entry, "{kinds}");
    assert_eq!(kinds[0]["type"], "position", "{kinds}");
    let before = positions(root);
    assert_eq!(before[0]["entry"], entry, "{before}");
    assert_eq!(before[0]["state"], "none", "{before}");
    let why = before[0]["why_not"].as_str().unwrap_or_default();
    assert!(why.contains("once a person verifies it"), "{before}");
    verify(root, &entry);
    let after = positions(root);
    assert_eq!(after.as_array().map(Vec::len), Some(1), "{after}");
    assert_eq!(after[0]["name"], "pos-release", "{after}");
    assert_eq!(after[0]["state"], "active", "{after}");
    after[0]["entry"].as_str().unwrap().to_string()
}

/// Half a pin, and a reason that is not one of the three words, file nothing.
fn step_position_bad_pins(root: &Path) {
    let create = ["task", "create", "Announce release 1.4"];
    let half = fails(
        root,
        &[&create[..], &["--position", "pos-release"][..]].concat(),
    );
    assert!(
        half.contains("a pin names a position and gives a reason"),
        "{half}"
    );
    let pin = ["--position", "pos-release", "--pin-reason", "habit"];
    let word = fails(root, &[&create[..], &pin[..]].concat());
    assert!(
        word.contains("continuity, relationship, separation"),
        "{word}"
    );
    let none = json(&ok(root, &["task", "list", "--status", "proposed"], None));
    assert_eq!(none, serde_json::json!([]), "a refused task is not filed");
}

/// A pinned task waits for a session in the position, and the listing and the view say
/// so. Returns the task.
fn step_position_pinned_task(root: &Path, entry: &str) -> String {
    let id = pinned_task(root, "Announce release 1.4");
    // the owner fills no position, and the ledger gives it no pinned task
    let next = ok(root, &["task", "claim-next"], None);
    assert_eq!(next.trim(), "null");
    let waits = positions(root);
    assert_eq!(waits[0]["pinned"], 1, "{waits}");
    assert_eq!(waits[0]["waiting"], serde_json::json!([id]), "{waits}");
    assert_eq!(waits[0]["sessions"], serde_json::json!([]), "{waits}");
    assert_eq!(waits[0]["wake"], true, "{waits}");
    assert_eq!(waits[0]["agent"], "codex", "{waits}");
    let shown = json(&ok(root, &["position", "show", "pos-release"], None));
    assert_eq!(shown["entry"], entry, "{shown}");
    assert_eq!(shown["charter"]["agent"], "codex", "{shown}");
    assert_eq!(shown["commitments"][0]["id"], id, "{shown}");
    assert_eq!(shown["commitments"][0]["pin_reason"], "continuity");
    id
}

/// The owner retires the position: the task loses its pin and stays open.
fn step_position_retire(root: &Path, entry: &str, id: &str) {
    let retire = ["position", "retire", "pos-release", "--why"];
    let retired = json(&ok(
        root,
        &[&retire[..], &["the release train ended"][..]].concat(),
        None,
    ));
    assert_eq!(retired["position"], "pos-release", "{retired}");
    assert_eq!(retired["entry"], entry, "{retired}");
    assert_eq!(retired["unpinned"], serde_json::json!([id]), "{retired}");
    let task = json(&ok(root, &["task", "get", id], None));
    assert_eq!(task["status"], "open", "{task}");
    assert!(task.get("position").is_none(), "{task}");
    let gone = positions(root);
    assert_eq!(gone[0]["state"], "retired", "{gone}");
    assert_eq!(gone[0]["entry"], retired["retired_by"], "{gone}");
    assert_eq!(gone[0]["pinned"], 0, "{gone}");
    assert!(fails(root, &["position", "show", "pos-none"]).contains("pos-none"));
}

#[test]
fn a_position_is_written_verified_pinned_to_and_retired_through_the_commands() {
    let d = step_init();
    let entry = step_position_review_gate(&d.root);
    step_position_bad_pins(&d.root);
    let id = step_position_pinned_task(&d.root, &entry);
    step_position_retire(&d.root, &entry, &id);
}

fn search_call() -> (&'static str, serde_json::Value) {
    ("search", serde_json::json!({"query": "digests"}))
}

fn settings_call() -> (&'static str, serde_json::Value) {
    ("settings", serde_json::json!({}))
}

/// A session of `amem serve` under the arm a launcher's variable names: what it is
/// refused, what it still does, and what it reads.
#[test]
fn serve_gives_the_session_the_arm_a_launcher_names() {
    let mut d = step_init();
    step_write(&mut d);
    let body = "# Stopped\n\nThe digests are next.\n";
    let write = serde_json::json!({"body": body, "entry_type": "handoff"});

    // no launcher named an arm: production's settings, and the search finds the handoff
    let calls = [search_call(), settings_call()];
    let open = serve(&d.root, "agent:codex", &[], &[], &calls);
    let found = &open.result(0)["hits"][0]["id"];
    assert_eq!(*found, d.handoff, "{:?}", open.calls);
    assert_eq!(open.result(1)["arm"], "full", "{:?}", open.calls);
    assert_eq!(open.result(1)["settings"]["memory"], true);

    // the variable a launcher sets
    let env = [("MEMORY_ARM", "no-memory")];
    let t0 = ("t0", serde_json::json!({}));
    let calls = [search_call(), settings_call(), ("write", write), t0];
    let off = serve(&d.root, "agent:codex", &[], &env, &calls);
    let (allowed, why) = &off.calls[0];
    assert!(!allowed, "the search is refused: {why}");
    assert!(why.contains("memory access is off"), "{why}");
    assert_eq!(off.result(1)["arm"], "no-memory", "{:?}", off.calls);
    assert_eq!(off.result(1)["settings"]["memory"], false);
    assert!(off.result(2)["id"].is_string(), "the write is not governed");
    let (_, t0) = &off.calls[3];
    assert!(t0.contains("T0 is off"), "{t0}");
    assert!(!t0.contains(&d.handoff), "{t0}");
    let said = "amem: arm no-memory (the floor)";
    assert!(off.stderr.contains(said), "{}", off.stderr);
}

/// The flags of `amem serve`, and what ends the command before it serves.
#[test]
fn serve_takes_an_arm_and_a_snapshot_as_flags_and_refuses_what_is_neither() {
    let mut d = step_init();
    step_write(&mut d);
    // the raw-only arm at a snapshot before the record was written
    let flags = ["--arm", "raw-only", "--as-of", "2026-01-01T00:00:00Z"];
    let calls = [search_call(), settings_call()];
    let early = serve(&d.root, "agent:codex", &flags, &[], &calls);
    let none = serde_json::json!([]);
    assert_eq!(early.result(0)["hits"], none, "{:?}", early.calls);
    let told = early.result(1);
    assert_eq!(told["arm"], "raw-only", "{told}");
    assert_eq!(told["settings"]["not_after"], "2026-01-01T00:00:00Z");

    // an empty variable is unset, and a name that is no arm ends the command
    let unset = [("MEMORY_ARM", ""), ("MEMORY_AS_OF", "")];
    let same = serve(&d.root, "agent:codex", &[], &unset, &[settings_call()]);
    assert_eq!(same.result(0)["arm"], "full", "{:?}", same.calls);
    for (flags, why) in [
        (["--arm", "everything"], "is not an arm"),
        (["--as-of", "yesterday"], "RFC 3339"),
        (["--arm", "transcript-replay"], "in front of a prompt"),
    ] {
        let refused = fails(&d.root, &[&["serve"][..], &flags[..]].concat());
        assert!(refused.contains(why), "{refused}");
    }
}

#[test]
fn eval_arms_lists_the_five_arms_in_the_paper_s_order() {
    let d = step_init();
    let arms = json(&ok(&d.root, &["eval", "arms"], None));
    let names: Vec<&str> = arms
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["arm"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "no-memory",
            "transcript-replay",
            "t0-only",
            "raw-only",
            "full"
        ]
    );
    assert_eq!(arms[0]["stands_for"], "the floor");
    assert_eq!(arms[0]["settings"]["memory"], false);
    assert_eq!(arms[1]["settings"]["transcripts"], true);
    assert_eq!(arms[2]["settings"]["depth"], "t0");
    assert_eq!(arms[3]["settings"]["max_tier"], "raw");
    assert_eq!(arms[3]["settings"]["t0"], "digest");
    assert_eq!(arms[4]["stands_for"], "the system as built");
    assert_eq!(arms[4]["settings"]["t0"], "full");
}

#[test]
fn launch_refuses_an_arm_over_an_empty_record_and_a_name_that_is_no_arm() {
    let d = step_init();
    let launch = ["launch", "--harness", "claude", "--unconfined"];
    let both = [&launch[..], &["--arm", "full", "--empty-memory"][..]].concat();
    let refused = fails(&d.root, &both);
    assert!(refused.contains("--empty-memory"), "{refused}");
    assert!(refused.contains("cannot be used with"), "{refused}");
    let unknown = [&launch[..], &["--arm", "everything", "--prompt", "hi"][..]].concat();
    let refused = fails(&d.root, &unknown);
    assert!(refused.contains("is not an arm"), "{refused}");
    let replay = [&launch[..], &["--arm", "transcript-replay"][..]].concat();
    let refused = fails(&d.root, &replay);
    assert!(refused.contains("in front of a prompt"), "{refused}");
}

#[test]
fn serve_fills_the_position_a_launcher_names_before_it_serves() {
    let d = step_init();
    let entry = write_position(&d.root);
    verify(&d.root, &entry);
    let id = pinned_task(&d.root, "Announce release 1.4");
    let show = || ("position", serde_json::json!({"name": "pos-release"}));
    let claim = || ("claim_next", serde_json::json!({"lease_seconds": 600}));

    // a session that no launcher put in the position reads it, and gets no pinned task
    let outside = serve(&d.root, "agent:codex", &[], &[], &[show(), claim()]);
    assert!(
        outside.result(0).get("seat").is_none(),
        "{:?}",
        outside.calls
    );
    assert!(outside.result(1)["task"].is_null(), "{:?}", outside.calls);

    // the flag: the session fills the position under the charter's agent definition
    let author = "agent:codex (model: test-1)";
    let flag = ["--position", "pos-release"];
    let filled = serve(&d.root, author, &flag, &[], &[show(), claim()]);
    assert_eq!(
        filled.result(0)["seat"]["seconded"],
        false,
        "{:?}",
        filled.calls
    );
    assert_eq!(filled.result(1)["task"]["id"], id, "{:?}", filled.calls);
    assert!(
        filled.stderr.contains("amem: fills pos-release (mem-"),
        "{}",
        filled.stderr
    );

    // the variable a launcher sets: a session under another agent definition is seconded
    let env = [("MEMORY_POSITION", "pos-release")];
    let second = serve(&d.root, "agent:substitute", &[], &env, &[show()]);
    let view = second.result(0);
    assert_eq!(view["seat"]["seconded"], true, "{view}");
    assert_eq!(view["sessions"], serde_json::json!([author]), "{view}");
    assert!(
        second
            .stderr
            .contains("amem: seconded to pos-release: its charter names codex"),
        "{}",
        second.stderr
    );

    // a position that is not in force ends the command before it serves
    let refused = fails(&d.root, &["serve", "--position", "pos-none"]);
    assert!(
        refused.contains("no position pos-none is in force"),
        "{refused}"
    );
}
