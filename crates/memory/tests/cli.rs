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

/// One tool call as an agent, through `amem serve` on standard input and output: the
/// text of the tool's answer, or the message of its refusal.
fn as_agent(root: &Path, tool: &str, arguments: &serde_json::Value) -> Result<String, String> {
    use std::io::{BufRead, BufReader, Write};
    let mut child = Command::new(env!("CARGO_BIN_EXE_amem"))
        .arg("--root")
        .arg(root)
        .args(["serve", "--author", AGENT])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn serve");
    let mut to = child.stdin.take().expect("stdin");
    let mut from = BufReader::new(child.stdout.take().expect("stdout"));
    let mut send = |message: serde_json::Value| writeln!(to, "{message}").expect("send");
    let mut answer = || {
        let mut line = String::new();
        from.read_line(&mut line).expect("an answer");
        json(&line)
    };
    send(serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": {"name": "cli-test", "version": "0"},
        },
    }));
    answer();
    send(serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    send(serde_json::json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": tool, "arguments": arguments},
    }));
    let reply = answer();
    drop(to);
    child.wait().expect("wait");
    if let Some(message) = reply["error"]["message"].as_str() {
        return Err(message.to_string());
    }
    let text = reply["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("no text in {reply}"))
        .to_string();
    if reply["result"]["isError"] == true {
        Err(text)
    } else {
        Ok(text)
    }
}

const AGENT: &str = "agent:x";

/// The owner is the guard of a starter root, so the owner's own `declassify` is the
/// approval: the copy lands at the lower label, and `approvals` lists that approval.
#[test]
fn declassify_lands_a_copy_at_a_lower_label_and_approvals_lists_its_approval() {
    let d = step_init();
    let source = id_of(&ok(
        &d.root,
        &[
            "write",
            "--title",
            "Office move",
            "--classification",
            "restricted",
        ],
        Some("The office moves on 3 March.\n"),
    ));
    let why = "The date was announced to all staff.";
    let copy_at = |label: &'static str| ["declassify", &source, "--to", label, "--why", why];
    let err = fails(&d.root, &copy_at("restricted"));
    assert!(err.contains("restricted is not lower"), "{err}");
    // the owner's own erasure of another entry is an approval of that entry
    let other = id_of(&ok(&d.root, &["write"], Some("A note by mistake.\n")));
    ok(&d.root, &["erase", &other, "--reason", "mistaken"], None);
    let none = json(&ok(&d.root, &["approvals", &source], None));
    assert_eq!(none, serde_json::json!([]), "a refused copy has none");
    let landed = json(&ok(&d.root, &copy_at("internal"), None));
    assert_eq!(landed["outcome"], "declassified", "{landed}");
    assert_eq!(landed["classification"], "internal");
    let copy = landed["id"].as_str().unwrap();
    assert_ne!(copy, source);
    let cites = landed["cites"].as_str().unwrap();
    let version = cites.strip_prefix(&format!("{source}@")).expect(cites);
    let text = ok(&d.root, &["get", copy], None);
    assert!(text.contains("classification: internal"), "{text}");
    assert!(text.contains(cites), "the copy cites {cites}: {text}");
    assert!(text.contains("The office moves on 3 March."), "{text}");
    let listed = json(&ok(&d.root, &["approvals", &source], None));
    let listed = listed.as_array().unwrap();
    assert_eq!(listed.len(), 1, "{listed:?}");
    let expected = serde_json::json!({
        "id": landed["approval"]["id"], "domain": "dom-me",
        "principal": "human:test:me", "action": "declassify:internal",
        "target": source, "version": version, "scope": "human:test:me",
        "expires": landed["approval"]["expires"], "at": landed["approval"]["at"],
        "in_force": true,
    });
    assert_eq!(listed[0], expected);
    // with no entry named, the approvals of the domain, oldest first
    let all = json(&ok(&d.root, &["approvals", "--domain", "dom-me"], None));
    let all: Vec<_> = all
        .as_array()
        .unwrap()
        .iter()
        .map(|a| (a["action"].as_str().unwrap(), a["target"].as_str().unwrap()))
        .collect();
    let expected = [
        ("erase", other.as_str()),
        ("declassify:internal", source.as_str()),
    ];
    assert_eq!(all, expected);
}

/// `amem approve --expires` gives the approval of a sign-off task the time that the
/// command names. A time that is not after now permits nothing, and is refused.
#[test]
fn approve_gives_a_sign_off_task_the_expiry_that_the_command_names() {
    let d = step_init();
    let id = id_of(&ok(
        &d.root,
        &["write", "--title", "Gate code"],
        Some("The gate code is 4417.\n"),
    ));
    let erase = serde_json::json!({"id": id, "reason": "mistaken"});
    let refused = as_agent(&d.root, "erase", &erase).unwrap_err();
    let named = refused
        .split_once("Task ")
        .and_then(|(_, rest)| rest.split_once(" asks human:test:me for it."));
    let Some((task, _)) = named else {
        panic!("the refusal names no task: {refused}")
    };
    let err = fails(&d.root, &["approve", task, "--expires", "tomorrow"]);
    assert!(err.contains("RFC 3339"), "{err}");
    let past = "2020-01-01T00:00:00Z";
    let err = fails(&d.root, &["approve", task, "--expires", past]);
    assert!(err.contains("permits nothing"), "{err}");
    let none = json(&ok(&d.root, &["approvals", &id], None));
    assert_eq!(none, serde_json::json!([]), "nothing was approved");
    let later = "2099-01-01T00:00:00Z";
    let approved = json(&ok(&d.root, &["approve", task, "--expires", later], None));
    assert_eq!(approved["outcome"], "approved", "{approved}");
    assert_eq!(approved["approval"]["expires"], later);
    assert_eq!(approved["approval"]["scope"], AGENT);
    let listed = json(&ok(&d.root, &["approvals", &id], None));
    assert_eq!(listed[0]["expires"], later);
    assert_eq!(listed[0]["in_force"], true);
    // the requester runs the action again, and it runs under that approval
    let receipt = json(&as_agent(&d.root, "erase", &erase).unwrap());
    assert_eq!(receipt["id"], id.as_str());
    assert_eq!(receipt["erased_by"], AGENT);
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
