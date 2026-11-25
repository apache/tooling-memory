//! The seven M0 steps from PLAN.md, run against the built binary. This is the demo in
//! test form: if it passes, someone can clone, build, and do the same by hand.

// An integration test is test code, but clippy's allow-unwrap-in-tests only sees
// #[cfg(test)] modules; here a panic is the failure being reported.
#![allow(clippy::unwrap_used)]

use serde_json::Value;
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

/// `id` is withheld: a get says why, and its body, with `text` in it, is still in the
/// record.
fn is_withheld(root: &Path, id: &str, text: &str) {
    let err = fails(root, &["get", id]);
    assert!(err.contains("gone") && err.contains("withheld"), "{err}");
    let kept = std::fs::read_to_string(root.join(format!("record/dom-me/{id}.md"))).unwrap();
    assert!(
        kept.contains("status: withheld") && kept.contains(text),
        "{kept}"
    );
}

#[test]
fn erasure_leaves_a_stub_a_receipt_and_the_citing_entry_withheld() {
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
    assert_eq!(receipt["withheld"], serde_json::json!([citing]));
    assert_eq!(receipt["promotion_whys_removed"], 1);
    assert!(!ids(&ok(&d.root, &["search", "vault opens"], None)).contains(&secret));
    is_withheld(&d.root, &citing, &format!("See {secret}."));
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
    as_named(root, AGENT, tool, arguments)
}

/// [`as_agent`], for the agent `author`.
fn as_named(
    root: &Path,
    author: &str,
    tool: &str,
    arguments: &serde_json::Value,
) -> Result<String, String> {
    as_served(root, &["--author", author], tool, arguments)
}

/// One tool call through `amem serve` with `flags`, on standard input and output: the
/// text of the tool's answer, or the message of its refusal.
fn as_served(
    root: &Path,
    flags: &[&str],
    tool: &str,
    arguments: &serde_json::Value,
) -> Result<String, String> {
    use std::io::{BufRead, BufReader, Write};
    let mut child = Command::new(env!("CARGO_BIN_EXE_amem"))
        .arg("--root")
        .arg(root)
        .arg("serve")
        .args(flags)
        .env_remove("MEMORY_POSITION")
        .env_remove("MEMORY_ARM")
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
/// Tool calls as `author` in one session, through `amem serve` over stdio, as a harness
/// makes them: the JSON result of each.
fn session(root: &Path, author: &str, calls: &[(&str, serde_json::Value)]) -> Vec<Value> {
    use std::io::{BufRead, BufReader, Write};
    let mut child = Command::new(env!("CARGO_BIN_EXE_amem"))
        .arg("--root")
        .arg(root)
        .args(["serve", "--author", author])
        .env_remove("MEMORY_ROOT")
        .env_remove("MEMORY_TASK")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    let hello = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                   "clientInfo": {"name": "test", "version": "0"}}});
    writeln!(stdin, "{hello}").unwrap();
    stdout.read_line(&mut line).unwrap();
    let ready = serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
    writeln!(stdin, "{ready}").unwrap();
    let mut results = Vec::new();
    for (n, (tool, arguments)) in calls.iter().enumerate() {
        let call = serde_json::json!({"jsonrpc": "2.0", "id": n + 2, "method": "tools/call",
            "params": {"name": tool, "arguments": arguments}});
        writeln!(stdin, "{call}").unwrap();
        line.clear();
        stdout.read_line(&mut line).unwrap();
        let answer = json(&line);
        let text = answer["result"]["content"][0]["text"].as_str();
        let text = text.unwrap_or_else(|| panic!("{tool} as {author}: {answer}"));
        assert_ne!(answer["result"]["isError"], true, "{tool}: {text}");
        results.push(json(text));
    }
    drop(stdin);
    child.wait().unwrap();
    results
}

/// A session of `author` that reads `id` and then writes a note: the id of the note.
fn reads_then_writes(root: &Path, author: &str, id: &str, body: &str) -> String {
    let calls = [
        ("get", serde_json::json!({ "id": id })),
        ("write", serde_json::json!({ "body": body })),
    ];
    let results = session(root, author, &calls);
    results[1]["id"].as_str().unwrap().to_string()
}

/// The entries of a lineage answer, each as its id, its hops, and whether a read
/// informed it.
fn edges(lineage: &Value) -> Vec<(String, u64, bool)> {
    let entries = lineage["entries"].as_array().unwrap();
    let edge = |e: &Value| {
        let id = e["id"].as_str().unwrap().to_string();
        (id, e["hops"].as_u64().unwrap(), e["informed"] == true)
    };
    entries.iter().map(edge).collect()
}

#[test]
fn lineage_gives_forward_and_with_back_backward_edges_of_a_session_that_read_then_wrote() {
    let d = step_init();
    let x = id_of(&ok(
        &d.root,
        &["write", "--title", "Port"],
        Some("The service listens on 8080.\n"),
    ));
    let y = reads_then_writes(&d.root, "agent:reader", &x, "The proxy forwards to it.\n");
    let forward = json(&ok(&d.root, &["lineage", &x], None));
    assert_eq!(forward["direction"], "forward");
    assert_eq!(edges(&forward), [(y.clone(), 1, true)]);
    let back = json(&ok(&d.root, &["lineage", &y, "--back"], None));
    assert_eq!(back["direction"], "back");
    assert_eq!(edges(&back), [(x.clone(), 1, true)]);
    // one session read the entry and wrote the note
    assert_eq!(forward["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(forward["sessions"], back["sessions"]);
    assert_eq!(back["complete"], true);

    // a step further on, and a depth that leaves it out
    let z = reads_then_writes(&d.root, "agent:other", &y, "Restart the proxy first.\n");
    let all = json(&ok(&d.root, &["lineage", &x], None));
    assert_eq!(edges(&all), [(y.clone(), 1, true), (z.clone(), 2, true)]);
    let near = json(&ok(&d.root, &["lineage", &x, "--depth", "1"], None));
    assert_eq!(edges(&near), [(y.clone(), 1, true)]);
    let far = json(&ok(&d.root, &["lineage", &z, "--back"], None));
    assert_eq!(edges(&far), [(y, 1, true), (x.clone(), 2, true)]);

    // an export is a send: forward names where it went, and back from the send event
    // gives what the export read
    let bundle = d.dir.path().join("bundle");
    let bundle = bundle.to_str().unwrap();
    ok(
        &d.root,
        &["export", bundle, "--max-label", "internal"],
        None,
    );
    let sent = json(&ok(&d.root, &["lineage", &x], None));
    let sends = sent["sends"].as_array().unwrap();
    assert_eq!(sends.len(), 1, "{sent}");
    assert!(sends[0]["to"].as_str().unwrap().starts_with("export:"));
    let of = format!("dom-me@{}", sends[0]["first"]);
    let from = json(&ok(
        &d.root,
        &["lineage", &of, "--back", "--depth", "1"],
        None,
    ));
    let read: Vec<String> = edges(&from).into_iter().map(|e| e.0).collect();
    assert!(read.contains(&x) && read.contains(&z), "{from}");
    let why = fails(&d.root, &["lineage", &of]);
    assert!(why.contains("no lineage forward"), "{why}");
}

#[test]
fn withheld_list_names_an_entry_that_an_erasure_withheld_and_release_serves_it_again() {
    let d = step_init();
    let secret = id_of(&ok(
        &d.root,
        &["write", "--title", "Vault"],
        Some("The vault opens with a passphrase.\n"),
    ));
    let citing = id_of(&ok(
        &d.root,
        &["write", "--title", "Where the vault is explained"],
        Some(&format!("See {secret}.\n")),
    ));
    let none = json(&ok(&d.root, &["withheld", "list"], None));
    assert_eq!(none, serde_json::json!([]));
    let receipt = json(&ok(
        &d.root,
        &["erase", &secret, "--reason", "mistaken"],
        None,
    ));
    assert_eq!(receipt["withheld"], serde_json::json!([citing]));
    let listed = json(&ok(
        &d.root,
        &["withheld", "list", "--domain", "dom-me"],
        None,
    ));
    assert_eq!(
        listed,
        serde_json::json!([{ "id": citing, "because": secret }])
    );
    let why = fails(&d.root, &["withheld", "list", "--domain", "dom-other"]);
    assert!(why.contains("not granted"), "{why}");
    is_withheld(&d.root, &citing, &format!("See {secret}."));
    let released = json(&ok(&d.root, &["withheld", "release", &citing], None));
    assert_eq!(released, serde_json::json!({ "released": citing }));
    let served = ok(&d.root, &["get", &citing], None);
    assert!(
        served.contains(&format!("See {secret}.")) && !served.contains("status: withheld"),
        "{served}"
    );
    let none = json(&ok(&d.root, &["withheld", "list"], None));
    assert_eq!(none, serde_json::json!([]));
    let why = fails(&d.root, &["withheld", "release", &citing]);
    assert!(why.contains("is not withheld"), "{why}");
}

/// `amem log freshness`: whether it succeeded is asserted; its answer, and what it
/// said on standard error.
fn freshness(root: &Path) -> (Value, String) {
    let out = memory(root, &["log", "freshness"], None);
    assert!(out.status.success());
    let said = String::from_utf8_lossy(&out.stderr).into_owned();
    (json(&String::from_utf8_lossy(&out.stdout)), said)
}

#[test]
fn log_freshness_reports_a_domain_within_the_bound_after_a_write() {
    let d = step_init();
    ok(
        &d.root,
        &["write", "--title", "Port"],
        Some("It is 8080.\n"),
    );
    let (fresh, said) = freshness(&d.root);
    assert_eq!(fresh["domain"], "dom-me");
    assert_eq!(fresh["within_bound"], true);
    assert_eq!(fresh["unfed"], 0);
    assert_eq!(fresh["bound_seconds"], 60);
    assert_eq!(fresh["fed_seq"], fresh["log_seq"]);
    assert!(fresh["log_seq"].as_u64().unwrap() >= 2, "{fresh}");
    assert_eq!(said, "", "nothing waits, so nothing is said");
}

#[test]
fn log_show_serves_the_ledger_s_events_up_to_now_and_from_a_sequence_number() {
    let d = step_init();
    ok(
        &d.root,
        &["write", "--title", "Port"],
        Some("It is 8080.\n"),
    );
    // filed after the last write: nothing copied the ledger into the log since
    ok(&d.root, &["task", "create", "Move the port"], None);
    let events = json(&ok(&d.root, &["log", "show"], None));
    let events = events.as_array().unwrap();
    assert_eq!(events[0]["kind"], "write");
    let last = &events[events.len() - 1];
    assert_eq!(last["kind"], "ledger", "{last}");
    let from = last["seq"].to_string();
    let tail = json(&ok(&d.root, &["log", "show", "--from", &from], None));
    assert_eq!(tail, serde_json::json!([last]));
}

/// The log events of the root with one kind.
fn events_of(root: &Path, kind: &str) -> Vec<Value> {
    let events = json(&ok(root, &["log", "show"], None));
    let of = events.as_array().unwrap().iter();
    of.filter(|e| e["kind"] == kind).cloned().collect()
}

#[test]
fn demote_leaves_a_tombstone_and_a_log_event_and_restore_serves_the_entry_again() {
    let d = step_init();
    let note = ok(
        &d.root,
        &["write", "--title", "Port"],
        Some("It is 8080.\n"),
    );
    let id = id_of(&note);
    let none = json(&ok(&d.root, &["dead"], None));
    assert_eq!(
        none,
        serde_json::json!({ "dead": [] }),
        "the window is 90 days"
    );
    let dead = json(&ok(&d.root, &["dead", "--days", "0"], None));
    assert_eq!(dead["dead"][0]["id"], id, "{dead}");
    let bad = fails(&d.root, &["demote", &id, "--reason", "stale"]);
    assert!(bad.contains("dead or wrong"), "{bad}");

    let done = json(&ok(&d.root, &["demote", &id, "--reason", "dead"], None));
    assert_eq!(done["reason"], "dead", "{done}");
    assert_eq!(done["by"], "human:test:me", "{done}");
    let gone = fails(&d.root, &["get", &id]);
    assert!(gone.contains("demoted"), "{gone}");
    let listed = ok(&d.root, &["list"], None);
    assert!(!listed.contains(&id), "{listed}");
    let demoted = events_of(&d.root, "demoted");
    assert_eq!(demoted.len(), 1, "{demoted:?}");
    assert_eq!(demoted[0]["id"], id);
    assert_eq!(demoted[0]["by"], "human:test:me");
    assert_eq!(demoted[0]["reason"], "dead");

    let back = json(&ok(&d.root, &["restore", &id], None));
    assert_eq!(back, serde_json::json!({ "restored": id }));
    assert!(ok(&d.root, &["get", &id], None).contains("It is 8080."));
    assert_eq!(events_of(&d.root, "restored")[0]["id"], id);
}

#[test]
fn conventions_names_an_entry_with_no_owner_and_files_it_once() {
    let d = step_init();
    let convention = ["write", "--kind", "convention", "--title"];
    let owned = [&convention[..], &["Titles", "--owner", "human:test:lead"]].concat();
    let owned = id_of(&ok(&d.root, &owned, Some("A title says a claim.\n")));
    assert!(ok(&d.root, &["get", &owned], None).contains("owner: human:test:lead"));
    let agent = [&convention[..], &["Titles", "--owner", "agent:worker"]].concat();
    fails(&d.root, &agent);
    let clean = json(&ok(&d.root, &["conventions", "--file"], None));
    let none = serde_json::json!({ "failures": [], "filed": [], "already_filed": 0 });
    assert_eq!(clean, none);

    let bare = [&convention[..], &["Bodies"]].concat();
    let bare = id_of(&ok(&d.root, &bare, Some("A body gives the reason.\n")));
    let found = json(&ok(&d.root, &["conventions"], None));
    assert_eq!(found["failures"][0]["check"], "ownership", "{found}");
    assert_eq!(found["failures"][0]["entry"], bare, "{found}");
    assert_eq!(found["failures"][0]["owner"], "human:test:me", "{found}");
    assert_eq!(found["filed"], serde_json::json!([]), "{found}");
    let filed = json(&ok(&d.root, &["conventions", "--file"], None));
    let task = filed["filed"][0].as_str().expect("a task");
    let task = json(&ok(&d.root, &["task", "get", task], None));
    assert_eq!(task["origin"], "convention:ownership", "{task}");
    let again = json(&ok(&d.root, &["conventions", "--file"], None));
    assert_eq!(again["filed"], serde_json::json!([]), "{again}");
    assert_eq!(again["already_filed"], 1, "{again}");

    // the window of the freshness check is the one that the command names
    let due = time::OffsetDateTime::now_utc().date() - time::Duration::days(10);
    let late = serde_json::json!({
        "body": "The port is 8080.\n", "entry_type": "decision", "review_by": due.to_string(),
    });
    session(&d.root, "human:test:me", &[("write", late)]);
    let fresh = |days: &str| {
        let found = json(&ok(&d.root, &["conventions", "--overdue-days", days], None));
        let all = found["failures"].as_array().unwrap().clone();
        let fresh = all.into_iter().filter(|f| f["check"] == "freshness");
        fresh.count()
    };
    assert_eq!((fresh("30"), fresh("5")), (0, 1));
}

/// `amem task outcome` has two forms, and each form makes its own record. A word alone
/// is a work outcome, on the definition that the task names. `--kind` is an outcome
/// event, on the task. One call takes one form.
#[test]
fn task_outcome_takes_one_form_and_each_form_makes_its_own_record() {
    let d = step_init();
    let task = json(&ok(&d.root, &["task", "create", "Find the port"], None));
    let id = task["id"].as_str().unwrap().to_string();
    let neither = fails(&d.root, &["task", "outcome", &id]);
    assert!(neither.contains("required"), "{neither}");
    let both = ["task", "outcome", &id, "rejected", "--kind", "rejected"];
    let both = fails(&d.root, &both);
    assert!(both.contains("cannot be used with"), "{both}");
    let noted = ["task", "outcome", &id, "rejected", "--note", "No."];
    let noted = fails(&d.root, &noted);
    assert!(noted.contains("cannot be used with"), "{noted}");
    // the task names no definition: the work outcome is refused, and leaves no event
    let bare = fails(&d.root, &["task", "outcome", &id, "rejected"]);
    assert!(bare.contains("names no task definition"), "{bare}");
    let none = json(&ok(&d.root, &["task", "outcomes", &id], None));
    assert_eq!(none, serde_json::json!({ "outcomes": [] }));
    // the outcome event needs no definition
    let event = ["task", "outcome", &id, "--kind", "rejected"];
    let said = json(&ok(&d.root, &event, None));
    assert_eq!(said["outcomes"][0]["outcome"], "rejected", "{said}");
    assert_eq!(said["outcomes"].as_array().unwrap().len(), 1, "{said}");
}

#[test]
fn an_outcome_makes_a_correction_and_corrections_files_it_for_a_curator() {
    let d = step_init();
    let task = json(&ok(&d.root, &["task", "create", "Find the port"], None));
    let id = task["id"].as_str().unwrap().to_string();
    let bad = fails(&d.root, &["task", "outcome", &id, "--kind", "wrong"]);
    assert!(
        bad.contains("accepted, corrected, rejected or reverted"),
        "{bad}"
    );
    let none = json(&ok(&d.root, &["task", "outcomes", &id], None));
    assert_eq!(none, serde_json::json!({ "outcomes": [] }));
    let outcome = ["--kind", "corrected", "--note", "The port is 8443."];
    let said = [&["task", "outcome", &id], &outcome[..]].concat();
    let said = json(&ok(&d.root, &said, None));
    assert_eq!(said["outcomes"][0]["outcome"], "corrected", "{said}");
    assert_eq!(said["outcomes"][0]["note"], "The port is 8443.", "{said}");
    assert_eq!(said["outcomes"][0]["by"], "human:test:me", "{said}");
    assert_eq!(json(&ok(&d.root, &["task", "outcomes", &id], None)), said);

    let found = json(&ok(&d.root, &["corrections"], None));
    assert_eq!(found["corrections"][0]["task"], id, "{found}");
    assert_eq!(found["corrections"][0]["disputed"], false, "{found}");
    assert_eq!(found["filed"], serde_json::json!([]), "{found}");
    let filed = json(&ok(&d.root, &["corrections", "--file"], None));
    let curator = filed["filed"][0].as_str().expect("a task");
    let curator = json(&ok(&d.root, &["task", "get", curator], None));
    assert_eq!(curator["origin"], "curator:correction", "{curator}");
    let question = "Could the agent have reached the right answer";
    assert!(curator["body"].as_str().unwrap().contains(question));
    let again = json(&ok(&d.root, &["corrections", "--file"], None));
    assert_eq!(again["already_filed"], 1, "{again}");

    let policy = json(&ok(&d.root, &["policy", "curate", "--file"], None));
    let empty = serde_json::json!({
        "overlaps": [], "repeated": [], "filed": [], "already_filed": 0,
    });
    assert_eq!(policy, empty);
}

#[test]
fn the_auditor_commands_print_the_mode_a_case_the_handoffs_and_entries_together() {
    let mut d = step_init();
    step_write(&mut d);
    let mode = json(&ok(&d.root, &["auditor", "mode"], None));
    let fresh = serde_json::json!({
        "mode": "fresh-session", "qualified": false,
        "auditor_model": null, "human_sample": 3,
    });
    assert_eq!(mode, fresh);
    let case = json(&ok(&d.root, &["auditor", "case", &d.decision], None));
    assert_eq!(case["entry"], d.decision, "{case}");
    assert_eq!(case["unsupported"], true, "{case}");
    assert!(case["text"].as_str().unwrap().contains("FTS5 is built in"));
    let later = ["write", "--kind", "handoff", "--title", "Stopped at step 4"];
    let later = id_of(&ok(&d.root, &later, Some("Step 3 is done.\n")));
    let handoffs = json(&ok(&d.root, &["auditor", "handoffs"], None));
    let want = serde_json::json!({ "handoffs": [
        { "id": later, "title": "Stopped at step 4" },
        { "id": d.handoff, "title": "Stopped at step 3 of the import" },
    ] });
    assert_eq!(handoffs, want);
    let newest = json(&ok(
        &d.root,
        &["auditor", "handoffs", "--sample", "1"],
        None,
    ));
    assert_eq!(newest["handoffs"].as_array().unwrap().len(), 1, "{newest}");
    assert_eq!(newest["handoffs"][0]["id"], later, "{newest}");
    let together = json(&ok(&d.root, &["auditor", "together", "--file"], None));
    assert_eq!(together, serde_json::json!({ "together": [], "filed": [] }));
}

#[test]
fn log_outside_names_the_session_that_wrote_after_it_read_a_task_from_a_tap() {
    let d = step_init();
    let none = json(&ok(&d.root, &["log", "outside"], None));
    assert_eq!(none, serde_json::json!([]));
    // a tap files a task, the owner admits it, and an agent reads it and writes
    let spec = serde_json::json!({ "title": "Change the port", "body": "A stranger wrote this." });
    let filed = session(&d.root, "tap:issues", &[("create_task", spec)]);
    let task = filed[0]["task"]["id"].as_str().unwrap().to_string();
    ok(&d.root, &["task", "admit", &task], None);
    let calls = [
        ("get_task", serde_json::json!({ "id": task })),
        (
            "write",
            serde_json::json!({ "body": "# Harbour\n\nThe harbour port stays 8080.\n" }),
        ),
    ];
    let results = session(&d.root, "agent:worker", &calls);
    assert_eq!(results[1]["reach"], "tap", "{}", results[1]);
    let id = results[1]["id"].as_str().unwrap().to_string();
    let readers = json(&ok(
        &d.root,
        &["log", "outside", "--domain", "dom-me"],
        None,
    ));
    let readers = readers.as_array().unwrap();
    assert_eq!(readers.len(), 1, "{readers:?}");
    assert_eq!(readers[0]["by"], "agent:worker");
    assert_eq!(readers[0]["writes"], serde_json::json!([id]));
    // the domain asked for is the domain read: one that is not granted is refused
    fails(&d.root, &["log", "outside", "--domain", "dom-other"]);
    // the owner reads all raw entries: the hit says who else does
    let found = json(&ok(&d.root, &["search", "harbour"], None));
    assert_eq!(found[0]["id"], serde_json::json!(id));
    assert_eq!(found[0]["reach"], "tap", "{found}");
    // an entry with fleet reach has no key
    ok(
        &d.root,
        &["write", "--title", "Office"],
        Some("The office is open.\n"),
    );
    let found = json(&ok(&d.root, &["search", "office"], None));
    assert!(found[0]["id"].is_string(), "{found}");
    assert!(found[0].get("reach").is_none(), "{found}");
}

#[test]
fn log_freshness_says_when_a_version_waits_past_the_bound_and_log_feed_takes_it() {
    let d = step_init();
    ok(
        &d.root,
        &["write", "--title", "Port"],
        Some("It is 8080.\n"),
    );
    // a version that a stopped writer left in the log an hour ago
    let seq = freshness(&d.root).0["log_seq"].as_u64().unwrap() + 1;
    let old = time::OffsetDateTime::now_utc() - time::Duration::hours(1);
    let old = old
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    let line = serde_json::json!({"seq": seq, "at": old, "by": "human:test:me",
        "kind": "put", "id": "mem-me-left", "digest": "d", "active": true});
    let log = d.root.join("state/log/dom-me.jsonl");
    let mut text = std::fs::read_to_string(&log).unwrap();
    text.push_str(&format!("{line}\n"));
    std::fs::write(&log, text).unwrap();
    // the answer says so, and so does a line on standard error
    let (stale, said) = freshness(&d.root);
    assert_eq!(stale["within_bound"], false);
    assert_eq!(stale["unfed"], 1);
    let waited = stale["oldest_unfed_seconds"].as_i64().unwrap();
    assert!(waited >= 3600, "{stale}");
    assert!(
        said.contains("behind its log") && said.contains("dom-me"),
        "{said}"
    );
    // the feed takes it, once, and the domain is within the bound again
    let fed = json(&ok(&d.root, &["log", "feed"], None));
    assert_eq!(fed["to"], seq);
    assert_eq!(fed["applied"], 1);
    let again = json(&ok(&d.root, &["log", "feed", "--domain", "dom-me"], None));
    assert_eq!(again["applied"], 0);
    let (fresh, said) = freshness(&d.root);
    assert_eq!(fresh["within_bound"], true);
    assert_eq!(fresh["fed_seq"], seq);
    assert_eq!(said, "");
}

const EVALUATOR: &str = "agent:eval";

/// Give a starter root a platform domain, a held-out domain and an evaluator.
fn with_platform(root: &Path) {
    let path = root.join("grants.toml");
    let grants = std::fs::read_to_string(&path).unwrap();
    let (top, tables) = grants.split_once("\n[").expect("a table in grants.toml");
    let keys =
        format!("platform = \"dom-plat\"\nheld_out = \"dom-held\"\nevaluator = \"{EVALUATOR}\"\n");
    let domain = "default_classification = \"internal\"\nadmit_own = true\n";
    let grants = format!(
        "{top}\n{keys}\n[{tables}\n[domains.dom-plat]\n{domain}\n[domains.dom-held]\n{domain}"
    );
    std::fs::write(&path, grants).unwrap();
}

/// An entry that `author` writes over the memory server and proposes for verified, and
/// that the owner approves at the command line: its id.
fn verified_by_owner(root: &Path, author: &str, domain: &str, body: &str) -> String {
    let write = serde_json::json!({"domain": domain, "entry_type": "convention", "body": body});
    let id = id_of(&as_named(root, author, "write", &write).unwrap());
    let promote = serde_json::json!({"id": id, "to": "verified", "why": "checked"});
    let proposal = json(&as_named(root, author, "promote", &promote).unwrap());
    let task = proposal["task"]["id"].as_str().expect("a task").to_string();
    ok(root, &["approve", &task], None);
    id
}

/// A task under `tidy` that the owner files and [`AGENT`] claims and completes over
/// the memory server: the task and its handoff. The task then waits for its verdict.
fn handed_in(root: &Path) -> (String, String) {
    let filed = ok(
        root,
        &[
            "task",
            "create",
            "Tidy the importer",
            "--domain",
            "dom-me",
            "--definition",
            "tidy",
        ],
        None,
    );
    let task = id_of(&filed);
    if json(&filed)["status"] == "proposed" {
        ok(root, &["task", "admit", &task], None);
    }
    let claim = as_agent(root, "claim", &serde_json::json!({"id": task})).unwrap();
    let generation = json(&claim)["generation"].clone();
    let write = serde_json::json!({
        "domain": "dom-me", "entry_type": "handoff", "body": "Changed src/importer.rs.\n",
    });
    let handoff = id_of(&as_agent(root, "write", &write).unwrap());
    let complete = serde_json::json!({"id": task, "generation": generation, "handoff": handoff});
    let parked = json(&as_agent(root, "complete", &complete).unwrap());
    assert_eq!(parked["task"]["status"], "blocked", "{parked}");
    (task, handoff)
}

/// The level of `tidy`, as `amem platform level` prints it.
fn level(root: &Path) -> serde_json::Value {
    json(&ok(root, &["platform", "level", "tidy"], None))["level"].clone()
}

/// `amem task verdict` takes exactly one of its two flags, and `--accept` closes the task.
fn step_verdict(root: &Path, task: &str) {
    let neither = fails(root, &["task", "verdict", task]);
    assert!(neither.contains("--accept"), "{neither}");
    let both = fails(root, &["task", "verdict", task, "--accept", "--reject"]);
    assert!(both.contains("cannot be used with"), "{both}");
    let given = json(&ok(root, &["task", "verdict", task, "--accept"], None));
    assert_eq!(given["accepted"], true, "{given}");
    assert_eq!(given["task"]["status"], "done", "{given}");
}

/// `amem platform grade` records a run for the level that it names, and `amem platform
/// raise` takes the level one step up on that run.
fn step_grade_and_raise(root: &Path, handoff: &str) {
    let grade = ["platform", "grade", "tidy", "--level", "review", "--answer"];
    let bad = fails(root, &[&grade[..], &["files"]].concat());
    assert!(bad.contains("is not <scenario>=<handoff entry>"), "{bad}");
    let answer = format!("files=which={handoff}");
    let graded = json(&ok(root, &[&grade[..], &[&answer]].concat(), None));
    assert_eq!(graded["pass"], true, "{graded}");
    assert_eq!(graded["record"]["level"], "review", "{graded}");
    let raised = json(&ok(root, &["platform", "raise", "tidy"], None));
    assert_eq!(raised["to"], "review", "{raised}");
    assert_eq!(raised["run"], graded["record"]["id"], "{raised}");
}

/// `amem task outcome` records the outcome that it names, and no other word.
fn step_outcome(root: &Path, task: &str) {
    let lost = fails(root, &["task", "outcome", task, "lost"]);
    assert!(lost.contains("is not reverted or rejected"), "{lost}");
    let outcome = json(&ok(root, &["task", "outcome", task, "reverted"], None));
    assert_eq!(outcome["outcome"], "reverted", "{outcome}");
    assert_eq!(outcome["task"], task, "{outcome}");
}

/// The owner at the command line gives a verdict, grades, raises, records an outcome
/// and reads the gates. Each command prints what the library returns.
#[test]
fn the_owner_gives_a_verdict_grades_raises_and_records_an_outcome_at_the_command_line() {
    let d = step_init();
    with_platform(&d.root);
    let tidy = "# Tidy a module\n\n```platform task tidy\npurpose = \"Tidy one module.\"\nacceptance = [\"The handoff names each file that changed.\"]\noutputs = [\"handoff\"]\nautonomy = \"checks\"\n```\n";
    let suite = "# The suite for tidy\n\n```platform suite tidy\n[pass]\nreview = 1\n\n[[scenario]]\nname = \"files=which\"\nask = \"Which files changed?\"\nexpect = [\"src/importer\"]\n```\n";
    let definition = verified_by_owner(&d.root, AGENT, "dom-plat", tidy);
    verified_by_owner(&d.root, EVALUATOR, "dom-held", suite);
    let (task, handoff) = handed_in(&d.root);
    assert_eq!(level(&d.root), "person");
    step_verdict(&d.root, &task);
    step_grade_and_raise(&d.root, &handoff);
    assert_eq!(level(&d.root), "review");
    step_outcome(&d.root, &task);
    assert_eq!(level(&d.root), "person");
    let gates = json(&ok(&d.root, &["platform", "gates", &definition], None));
    assert_eq!(gates["entry"], definition.as_str(), "{gates}");
    assert_eq!(gates["review"]["met"], true, "{gates}");
}

/// Five refusals of one agent in one clock hour of the decision log: `task stats`
/// lists the agent under `probing`, and the curator's telemetry raises one alarm. The
/// hours are in the log, so the test reads no clock.
#[test]
fn the_curator_s_telemetry_raises_a_probing_alarm_from_the_decision_log() {
    let d = step_init();
    let refusal = |minute: usize| {
        serde_json::json!({
            "at": format!("2026-11-15T14:{minute:02}:07Z"), "session": "s-1",
            "action": "promote", "principal": "agent:z", "class": "agent",
            "domain": "dom-me", "allow": false, "admit": false, "review": "human",
            "reason": "a reason",
        })
        .to_string()
    };
    let lines: Vec<String> = (0..5).map(refusal).collect();
    std::fs::create_dir_all(d.root.join("state")).unwrap();
    std::fs::write(
        d.root.join("state/decisions.jsonl"),
        lines.join("\n") + "\n",
    )
    .unwrap();
    let stats = json(&ok(&d.root, &["task", "stats"], None));
    assert_eq!(stats["probing"][0]["who"], "agent:z", "{stats}");
    assert_eq!(stats["probing"][0]["hour"], "2026-11-15T14", "{stats}");
    let telemetry = json(&ok(&d.root, &["curator", "--telemetry"], None));
    let alarms: Vec<&serde_json::Value> = telemetry["alarms"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|a| a["kind"] == "probing")
        .collect();
    let [alarm] = &alarms[..] else {
        panic!("one probing alarm: {telemetry}")
    };
    assert_eq!(alarm["who"], "agent:z");
    let why = alarm["why"].as_str().unwrap();
    assert!(why.contains("in the clock hour 2026-11-15T14"), "{why}");
}

/// Add tables to the grants file of a root. Each command opens the root anew.
fn grant(root: &Path, tables: &str) {
    let path = root.join("grants.toml");
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, format!("{text}\n{tables}")).unwrap();
}

/// One row of `amem limits --json`, by its limit and its unit.
fn limit_row(root: &Path, limit: &str, unit: &str) -> serde_json::Value {
    let rows = json(&ok(root, &["limits", "--json"], None));
    let found = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["limit"] == limit && r["unit"] == unit);
    found
        .unwrap_or_else(|| panic!("no row {limit} in {unit}: {rows}"))
        .clone()
}

/// `amem metrics` prints the 21 rows of the paper and the 10 targets, as JSON and as
/// text. On a root that completed no task, the first row says that it is not measured.
#[test]
fn metrics_prints_the_21_rows_of_the_paper_and_then_the_targets() {
    let d = step_init();
    let table = json(&ok(&d.root, &["metrics", "--json"], None));
    assert_eq!(table["domain"], "dom-me", "{table}");
    assert_eq!(table["window_days"], 7, "{table}");
    let rows = table["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 21, "{table}");
    assert_eq!(rows[0]["name"], "Cost per completed task");
    assert_eq!(rows[0]["measured"], false, "{table}");
    assert_eq!(table["targets"].as_array().unwrap().len(), 10, "{table}");
    let days = json(&ok(&d.root, &["metrics", "--days", "3", "--json"], None));
    assert_eq!(days["window_days"], 3, "{days}");
    let text = ok(&d.root, &["metrics"], None);
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines[0].starts_with("metrics of dom-me, the last 7 days, price vector "));
    assert_eq!(lines.len(), 1 + 21 + 1 + 10, "{text}");
    assert!(
        lines[1].starts_with("  Cost per completed task: not measured: "),
        "{text}"
    );
    assert_eq!(lines[22], "targets", "{text}");
    assert!(lines[23].starts_with("  Search latency, median: target "));
}

/// The owner raises the share of upkeep at the command line, and `amem limits` then
/// names the approval. The raise holds for the hours that the command names. The share
/// of throughput is raised for a principal.
#[test]
fn limits_raise_records_an_approval_that_limits_then_names() {
    use time::format_description::well_known::Rfc3339;
    let d = step_init();
    let unset = fails(&d.root, &["limits", "raise", "upkeep", "--to", "60"]);
    assert!(unset.contains("the grants set no limit upkeep"), "{unset}");
    grant(
        &d.root,
        "[limits.dom-me]\nupkeep_percent = 50\nclaims_per_hour = 2\n",
    );
    let before = limit_row(&d.root, "upkeep", "percent of completed tasks");
    assert_eq!(before["cap"], 50.0, "{before}");
    assert!(before["raised_by"].is_null(), "{before}");
    let raise = ["limits", "raise", "upkeep", "--to", "60", "--hours", "2"];
    let raised = json(&ok(&d.root, &raise, None));
    assert_eq!(raised["target"], "limit:upkeep", "{raised}");
    assert_eq!(raised["action"], "raise:60", "{raised}");
    let time =
        |key: &str| time::OffsetDateTime::parse(raised[key].as_str().unwrap(), &Rfc3339).unwrap();
    let life = (time("expires") - time("at")).whole_minutes();
    assert!((119..=120).contains(&life), "two hours: {raised}");
    let after = limit_row(&d.root, "upkeep", "percent of completed tasks");
    assert_eq!(
        (&after["cap"], &after["configured"]),
        (&60.0.into(), &50.0.into())
    );
    assert_eq!(after["raised_by"], raised["id"], "{after}");
    let id = raised["id"].as_str().unwrap();
    let text = ok(&d.root, &["limits"], None);
    let said = format!("; raised from 50 by the approval {id}");
    assert!(text.contains(&said), "{text}");
    let nobody = fails(&d.root, &["limits", "raise", "throughput", "--to", "9"]);
    assert!(nobody.contains("name the principal"), "{nobody}");
    let share = [
        "limits",
        "raise",
        "throughput",
        "--to",
        "9",
        "--for",
        "agent:x",
    ];
    assert_eq!(json(&ok(&d.root, &share, None))["scope"], "agent:x");
    let word = fails(&d.root, &["limits", "raise", "spend", "--to", "9"]);
    let no = "spend is no limit that an approval raises";
    assert!(word.contains(no), "{word}");
}

/// Put one model call of `m-1` on the meter of the egress now: a million tokens in, in
/// `session`, at `openai`.
fn metered(root: &Path, session: &str) {
    let at = std::time::SystemTime::now();
    let usage = dm_egress::Usage {
        model: Some("m-1".into()),
        input_tokens: 1_000_000,
        ..dm_egress::Usage::default()
    };
    let meter = dm_egress::Telemetry::open(&root.join("state/telemetry.db")).unwrap();
    meter.record(at, session, "openai", 200, &usage).unwrap();
}

/// The price vector `t1`: a million tokens in of `m-1` cost two dollars.
fn priced(root: &Path) {
    let prices = "version = \"t1\"\n[models.m-1]\ninput = 2.0\n";
    std::fs::write(root.join("prices.toml"), prices).unwrap();
}

/// A row of spend for one day has the count that the binary read off the meter of the
/// egress: the tokens of the last 24 hours, and their price in US cents. A row of
/// spend for one session has no count.
#[test]
fn limits_counts_the_spend_of_the_last_day_from_the_meter_of_the_egress() {
    let d = step_init();
    grant(
        &d.root,
        "[spend.openai]\nday_tokens = 4000000\nday_usd_cents = 800\nsession_tokens = 10\n",
    );
    let rows = || json(&ok(&d.root, &["limits", "--json"], None));
    let of = |rows: &serde_json::Value, unit: &str, scope: &str| {
        let all = rows.as_array().unwrap().iter();
        let found = all
            .clone()
            .find(|r| r["unit"] == unit && r["scope"] == scope);
        found
            .unwrap_or_else(|| panic!("no row of {scope} in {unit}: {rows}"))
            .clone()
    };
    let nothing = rows();
    assert_eq!(of(&nothing, "tokens", "openai, one day")["now"], 0.0);
    metered(&d.root, "s-1");
    metered(&d.root, "s-2");
    priced(&d.root);
    let spent = rows();
    let tokens = of(&spent, "tokens", "openai, one day");
    assert_eq!(
        tokens["now"], 2_000_000.0,
        "two calls in the last day: {tokens}"
    );
    assert_eq!(tokens["fraction"], 0.5, "{tokens}");
    let cents = of(&spent, "US cents", "openai, one day");
    assert_eq!(
        (&cents["now"], &cents["fraction"]),
        (&400.0.into(), &0.5.into())
    );
    let session = of(&spent, "tokens", "openai, one session");
    assert!(session["now"].is_null(), "{session}");
    let text = ok(&d.root, &["limits"], None);
    assert!(
        text.contains("400 of 800 US cents, openai, one day"),
        "{text}"
    );
}

/// A task that the owner claims at the command line and hands to the landing queue
/// `main`: the task, and the handoff that describes the change.
fn queued_for_landing(root: &Path) -> (String, String) {
    let filed = ok(root, &["task", "create", "Change the importer"], None);
    let task = id_of(&filed);
    if json(&filed)["status"] == "proposed" {
        ok(root, &["task", "admit", &task], None);
    }
    let claimed = json(&ok(root, &["task", "claim-next"], None));
    assert_eq!(claimed["id"], task.as_str(), "{claimed}");
    let generation = claimed["generation"].to_string();
    let handoff = id_of(&ok(
        root,
        &[
            "write",
            "--kind",
            "handoff",
            "--body",
            "Changed src/importer.rs.\n",
        ],
        None,
    ));
    let queue = [
        "task",
        "queue-landing",
        &task,
        "--queue",
        "main",
        "--handoff",
        &handoff,
    ];
    let unnamed = fails(root, &queue);
    assert!(
        unnamed.contains("name the generation of the claim"),
        "{unnamed}"
    );
    let with = [&queue[..], &["--generation", &generation]].concat();
    let parked = json(&ok(root, &with, None));
    assert_eq!(parked["status"], "blocked", "{parked}");
    assert_eq!(parked["waiting_on"], "landing:main", "{parked}");
    (task, handoff)
}

/// The owner queues a change, reads the queue, lands the change and reads the times of
/// the task, all at the command line. The metric table then has the time from the claim
/// to the landing. With the session of the handoff on the meter of the egress and its
/// model in the price vector, the first row is the cost that the binary computed.
#[test]
fn a_change_is_queued_and_landed_at_the_command_line_and_the_table_counts_it() {
    let d = step_init();
    grant(&d.root, "[landing.main]\ndomain = \"dom-me\"\n");
    let (task, handoff) = queued_for_landing(&d.root);
    let waiting = json(&ok(&d.root, &["landing", "show", "main"], None));
    assert_eq!(waiting["waiting"][0]["task"], task.as_str(), "{waiting}");
    assert_eq!(waiting["waiting"][0]["person"], true, "{waiting}");
    let landed = json(&ok(&d.root, &["landing", "land", "main"], None));
    assert_eq!(landed["id"], task.as_str(), "{landed}");
    assert_eq!(landed["status"], "done", "{landed}");
    assert_eq!(landed["handoff"], handoff.as_str(), "{landed}");
    let empty = fails(&d.root, &["landing", "land", "main"]);
    assert!(
        empty.contains("nothing waits in the landing queue main"),
        "{empty}"
    );
    let times = json(&ok(&d.root, &["task", "times", &task], None));
    assert_eq!(times["queue"], "main", "{times}");
    assert!(times["claim_to_landed_secs"].is_i64(), "{times}");
    let table = json(&ok(&d.root, &["metrics", "--json"], None));
    let row = &table["rows"][14];
    assert_eq!(row["name"], "Claim-to-landed time", "{table}");
    assert_eq!(row["measured"], true, "{row}");
    assert_eq!(
        table["rows"][0]["measured"], false,
        "nothing is on the meter"
    );
    costs_two_dollars(&d.root, &handoff);
}

/// With the session that wrote `handoff` on the meter of the egress and its model in the
/// price vector, the first row of the table is the cost that the binary computed.
fn costs_two_dollars(root: &Path, handoff: &str) {
    let writes = std::fs::read_to_string(root.join("state/writes.jsonl")).unwrap();
    let wrote = writes.lines().map(json).find(|w| w["id"] == handoff);
    let session = wrote.unwrap()["session"].as_str().unwrap().to_string();
    metered(root, &session);
    priced(root);
    let table = json(&ok(root, &["metrics", "--json"], None));
    let cost = &table["rows"][0];
    assert_eq!(cost["measured"], true, "{cost}");
    assert_eq!(cost["from"], "caller", "{cost}");
    assert_eq!(cost["value"]["tasks"], 1, "{cost}");
    assert_eq!(cost["value"]["usd"], 2.0, "{cost}");
    assert_eq!(cost["value"]["prices"], "t1", "{cost}");
    assert!(ok(root, &["cost"], None).ends_with("\nprices: t1\n"));
}

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

const EVAL_PLAN: &str = "preamble = \"Read the record. \"\n[plan]\nname = \"cli\"\ndomain = \"dom-me\"\nbaseline = \"no-memory\"\narms = [\"no-memory\", \"full\"]\nseeds = 2\nmargin = 0.1\njudge = \"regex\"\n[[task]]\nid = \"a\"\nprompt = \"What is A?\"\nexpect = \"A\"\n";

#[test]
fn eval_plan_prints_the_plan_with_its_digest_or_the_refusal() {
    let d = step_init();
    let file = d.root.join("plan.toml");
    std::fs::write(&file, EVAL_PLAN).unwrap();
    let file = file.to_str().unwrap();
    let read = json(&ok(&d.root, &["eval", "plan", file], None));
    assert_eq!(read["digest"].as_str().map(str::len), Some(64), "{read}");
    assert_eq!(read["plan"]["name"], "cli");
    assert_eq!(
        read["plan"]["arms"],
        serde_json::json!(["no-memory", "full"])
    );
    assert_eq!(read["plan"]["accepted_cost"], 1.0);
    // a comment does not move the digest, and the margin does
    std::fs::write(file, format!("# a comment\n{EVAL_PLAN}")).unwrap();
    let same = json(&ok(&d.root, &["eval", "plan", file], None));
    assert_eq!(same["digest"], read["digest"]);
    std::fs::write(file, EVAL_PLAN.replace("margin = 0.1", "margin = 0.2")).unwrap();
    let moved = json(&ok(&d.root, &["eval", "plan", file], None));
    assert_ne!(moved["digest"], read["digest"]);
    std::fs::write(file, EVAL_PLAN.replace("seeds = 2", "seeds = 1")).unwrap();
    let refused = fails(&d.root, &["eval", "plan", file]);
    assert!(refused.contains("two seeds or more"), "{refused}");
}

#[test]
fn eval_score_and_scorecards_reach_the_library() {
    let d = step_init();
    let plan = d.root.join("plan.toml");
    std::fs::write(&plan, EVAL_PLAN).unwrap();
    let outcomes = d.root.join("outcomes.json");
    std::fs::write(&outcomes, "[]").unwrap();
    let (plan, outcomes) = (plan.to_str().unwrap(), outcomes.to_str().unwrap());
    let refused = fails(&d.root, &["eval", "score", plan, outcomes]);
    assert!(refused.contains("one outcome or more"), "{refused}");
    std::fs::write(
        outcomes,
        r#"[{"session": "sess-none", "success": true, "cost": 1, "judge": "regex"}]"#,
    )
    .unwrap();
    let refused = fails(&d.root, &["eval", "score", plan, outcomes, "--keep"]);
    assert!(refused.contains("sess-none"), "{refused}");
    // a root with no platform domain keeps no scorecard
    let kept = json(&ok(&d.root, &["eval", "scorecards", "--limit", "3"], None));
    assert_eq!(kept, serde_json::json!([]));
}

#[test]
fn launch_takes_the_evaluation_flags_together_and_without_a_prompt() {
    let d = step_init();
    let plan = d.root.join("plan.toml");
    std::fs::write(&plan, EVAL_PLAN).unwrap();
    let plan = plan.to_str().unwrap();
    let launch = ["launch", "--harness", "claude", "--unconfined"];
    let run = ["--eval-plan", plan, "--eval-task", "a", "--eval-seed", "0"];
    let arm = ["--arm", "full", "--as-of", "2026-11-01T10:00:00Z"];
    let with = |more: &[&str]| fails(&d.root, &[&launch[..], more].concat());
    // the argument parser refuses these before anything starts
    let no_arm = with(&[&run[..], &arm[2..]].concat());
    assert!(no_arm.contains("--arm <ARM>"), "{no_arm}");
    let no_as_of = with(&[&run[..], &arm[..2]].concat());
    assert!(no_as_of.contains("--as-of <AS_OF>"), "{no_as_of}");
    let no_task = with(&[&run[..2], &run[4..], &arm[..]].concat());
    assert!(no_task.contains("--eval-task <EVAL_TASK>"), "{no_task}");
    let no_seed = with(&[&run[..4], &arm[..]].concat());
    assert!(no_seed.contains("--eval-seed <EVAL_SEED>"), "{no_seed}");
    for alone in [&run[2..4], &run[4..]] {
        let no_plan = with(&[alone, &arm[..]].concat());
        assert!(no_plan.contains("--eval-plan <EVAL_PLAN>"), "{no_plan}");
    }
    for flag in [
        &["--prompt", "x"][..],
        &["--prompt-file", "x"],
        &["--empty-memory"],
    ] {
        let both = with(&[&run[..], &arm[..], flag].concat());
        assert!(both.contains("cannot be used with"), "{both}");
    }
    // the plan decides the rest before the harness starts
    let no_such = [
        &run[..2],
        &["--eval-task", "c", "--eval-seed", "0"],
        &arm[..],
    ]
    .concat();
    let refused = with(&no_such);
    assert!(refused.contains("has no task c"), "{refused}");
    let other_arm = [&run[..], &["--arm", "raw-only"], &arm[2..]].concat();
    let refused = with(&other_arm);
    assert!(
        refused.contains("does not name the arm raw-only"),
        "{refused}"
    );
    let late_seed = [&run[..4], &["--eval-seed", "2"], &arm[..]].concat();
    let refused = with(&late_seed);
    assert!(refused.contains("2 is not one"), "{refused}");
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

/// One line of a drift file: a run of one probe, answered three times with
/// `input_tokens` input tokens.
fn drift_run(at: &str, input_tokens: u64) -> String {
    let sample = serde_json::json!({
        "digest": "x", "len": 8, "input_tokens": input_tokens, "output_tokens": 3,
        "latency_ms": 100, "model": "m", "system_fingerprint": "fp-1"
    });
    serde_json::json!({
        "at": at, "destination": "llmao", "model": "m", "repeat": 3,
        "probe_set": "probes/drift.toml",
        "pinned": {
            "amem_version": "0.1.0", "probe_set_digest": "abc",
            "request": {"path": "v1/chat/completions", "temperature": 0.0, "max_tokens": {"a": 64}}
        },
        "probes": [{
            "id": "a", "kind": "fingerprint", "prompt_digest": "p-a",
            "samples": [sample.clone(), sample.clone(), sample]
        }]
    })
    .to_string()
}

fn promote_call(id: &str) -> (&'static str, serde_json::Value) {
    let why = "read it against the notes";
    (
        "promote",
        serde_json::json!({"id": id, "to": "curated", "why": why}),
    )
}

/// Three runs of `llmao/m` under the root: two that agree, and one whose input tokens
/// changed. A check that files opens a drift window from the run before the one that
/// fired; a check that does not file opens none.
fn step_drift_opens_a_window(root: &Path) {
    let runs = root.join("state/drift/llmao");
    std::fs::create_dir_all(&runs).unwrap();
    let lines = [
        drift_run("2026-01-01T09:00:00Z", 20),
        drift_run("2026-01-02T09:00:00Z", 20),
        drift_run("2026-01-03T09:00:00Z", 21),
    ];
    std::fs::write(runs.join("m.jsonl"), lines.join("\n") + "\n").unwrap();
    let check = ["drift", "check", "--provider", "llmao", "--model", "m"];
    let list = ["drift", "window", "list"];
    let seen = json(&ok(root, &check, None));
    assert_eq!(seen["verdict"], "vendor drift", "{seen}");
    assert_eq!(seen["window"], serde_json::Value::Null, "{seen}");
    assert_eq!(json(&ok(root, &list, None)), serde_json::json!([]));

    let filing = [&check[..], &["--file"][..]].concat();
    let filed = json(&ok(root, &filing, None));
    assert!(filed["filed"].is_string(), "{filed}");
    let window = &filed["window"];
    assert_eq!(window["from"], "2026-01-02T09:00:00Z", "{filed}");
    assert_eq!(window["detectors"], serde_json::json!(["input_tokens"]));
    assert_eq!(window["until"], serde_json::Value::Null);
    // the same drift, found again: the same window
    let again = json(&ok(root, &filing, None));
    assert_eq!(again["window"], *window, "{again}");
    assert_eq!(json(&ok(root, &list, None)).as_array().unwrap().len(), 1);
}

/// An agent writes in the open window. The memory server and the command line refuse
/// to promote that entry, and a person's entry is promoted as before.
fn step_window_refuses_promotion(d: &Demo) {
    let body = "The import has five steps.\n";
    let write = ("write", serde_json::json!({ "body": body }));
    let wrote = serve(&d.root, "agent:codex", &[], &[], &[write]);
    let id = wrote.result(0)["id"].as_str().unwrap().to_string();
    let curator = serve(&d.root, "agent:curator", &[], &[], &[promote_call(&id)]);
    let (allowed, why) = &curator.calls[0];
    assert!(!allowed, "the promotion is refused: {why}");
    assert!(why.contains("drift window of llmao/m"), "{why}");
    let promote = ["promote", &id, "--to", "curated", "--why", "read"];
    let refused = fails(&d.root, &promote);
    assert!(refused.contains("drift window of llmao/m"), "{refused}");
    let mine = ["promote", &d.decision, "--to", "curated", "--why", "read"];
    assert_eq!(json(&ok(&d.root, &mine, None))["tier"], "curated");
}

/// The owner closes the window, and opens one by hand.
fn step_window_by_hand(root: &Path) {
    let close = ["drift", "window", "close", "--provider", "llmao"];
    let close = [&close[..], &["--model", "m", "--why", "pinned"][..]].concat();
    let closed = json(&ok(root, &close, None));
    assert!(closed["until"].is_string(), "{closed}");
    assert_eq!(closed["closed_why"], "pinned");
    assert!(fails(root, &close).contains("no drift window is open"));

    let open = ["drift", "window", "open", "--provider", "llmao", "--model"];
    let bad = [
        &open[..],
        &["m-2", "--why", "by hand", "--from", "then"][..],
    ]
    .concat();
    assert!(fails(root, &bad).contains("RFC 3339"));
    let from = "2026-02-01T00:00:00Z";
    let good = [&open[..], &["m-2", "--why", "by hand", "--from", from][..]].concat();
    let opened = json(&ok(root, &good, None));
    assert_eq!(opened["from"], from, "{opened}");
    assert_eq!(opened["detectors"], serde_json::json!([]));
}

/// Drift that a check confirms and files opens a drift window. The command line and
/// the memory server refuse to promote what an agent wrote in it. The owner closes it.
#[test]
fn a_drift_check_that_files_opens_a_window_that_freezes_promotion() {
    let mut d = step_init();
    step_write(&mut d);
    step_drift_opens_a_window(&d.root);
    step_window_refuses_promotion(&d);
    step_window_by_hand(&d.root);
}

/// `amem reenact state` gives the replay state of a tape: complete while nothing moved,
/// incomplete when the record moved. The session that asks takes the settings of the
/// tape, so the settings of a tape under an arm are not named as moved.
#[test]
fn reenact_state_gives_the_replay_state_of_a_tape() {
    let mut d = step_init();
    step_write(&mut d);
    let flags = ["--arm", "raw-only"];
    let calls = [("t0", serde_json::json!({})), search_call()];
    serve(&d.root, "agent:codex", &flags, &[], &calls);
    let tapes = json(&ok(&d.root, &["reenact", "tapes"], None));
    let tapes = tapes.as_array().unwrap();
    assert_eq!(tapes.len(), 1, "{tapes:?}");
    let session = tapes[0]["session"].as_str().unwrap().to_string();
    assert_eq!(tapes[0]["settings"]["max_tier"], "raw", "{tapes:?}");
    // two writes, each an offer and a version: the last version is event 4
    assert_eq!(tapes[0]["manifest"]["log"]["dom-me"], 4, "{tapes:?}");

    let state = json(&ok(&d.root, &["reenact", "state", &session], None));
    assert_eq!(state["availability"], "complete", "{state}");
    assert_eq!(state["moved"], serde_json::json!([]), "{state}");
    assert_eq!(state["manifest"]["settings"]["max_tier"], "raw", "{state}");
    assert_eq!(state["manifest"]["log"]["dom-me"], 4, "{state}");

    // the record moves: after the read of the search, event 5, the offer and the version
    ok(&d.root, &["write"], Some("The export has two steps.\n"));
    let state = json(&ok(&d.root, &["reenact", "state", &session], None));
    assert_eq!(state["availability"], "incomplete", "{state}");
    let moved = serde_json::json!([{"what": "log dom-me", "then": "4", "now": "7"}]);
    assert_eq!(state["moved"], moved, "{state}");
    assert_eq!(
        state["why"],
        serde_json::json!(["The log dom-me moved: it was 4 and it is 7 now."])
    );
    assert!(fails(&d.root, &["reenact", "state", "sess-none"]).contains("no tape"));
}

const SECTIONS: &str = "The import runs on the staging host.\n\n## Context\n\nThe import job needs a host with the loader installed.\n\n## Decision\n\nThe import job runs on the staging host every night.\n";

/// A decision of the owner with two sections, and a fact about the same thing.
fn decision_and_fact(root: &Path) -> (String, String) {
    let title = "Where the import runs";
    let decision = ["write", "--kind", "decision", "--title", title];
    let decision = id_of(&ok(root, &decision, Some(SECTIONS)));
    let note = "The import took four hours on the staging host last night.\n";
    let fact = ["write", "--kind", "fact", "--title", "Import duration"];
    (decision, id_of(&ok(root, &fact, Some(note))))
}

/// `amem get` takes an address. An anchor prints the section alone. A version prints the
/// text of that version, and `--json` says when it is no longer the current one.
#[test]
fn get_prints_a_section_and_an_earlier_version_by_address() {
    let d = step_init();
    let (id, _) = decision_and_fact(&d.root);
    let section = ok(&d.root, &["get", &format!("{id}#decision")], None);
    assert_eq!(
        section,
        "## Decision\n\nThe import job runs on the staging host every night.\n"
    );
    let err = fails(&d.root, &["get", &format!("{id}#options")]);
    assert!(err.contains("context, decision"), "{err}");
    let whole = json(&ok(&d.root, &["get", &id, "--json"], None));
    assert_eq!(whole["body"], SECTIONS);
    assert!(whole.get("current").is_none(), "{whole}");
    let first = whole["version"].as_str().unwrap().to_string();
    session(&d.root, "agent:curator", &[promote_call(&id)]);
    let at = format!("{id}@{first}");
    let old = json(&ok(&d.root, &["get", &at, "--json"], None));
    assert_eq!(old["current"], false, "{old}");
    assert_eq!(old["trust_tier"], "raw");
    assert_eq!(old["version"], first.as_str());
    let now = json(&ok(&d.root, &["get", &id, "--json"], None));
    assert_eq!(now["trust_tier"], "curated");
    assert_ne!(now["version"], first.as_str());
    let text = memory(&d.root, &["get", &at], None);
    let (out, err) = (
        String::from_utf8_lossy(&text.stdout),
        String::from_utf8_lossy(&text.stderr),
    );
    assert!(out.starts_with("---\nid: "), "{out}");
    assert!(out.contains("trust_tier: raw"), "{out}");
    assert!(err.contains("an earlier version"), "{err}");
}

/// The keys of a JSON object, sorted.
fn keys(v: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort_unstable();
    keys
}

/// `amem search` with a filter prints what the tool answers: the same hits, with the
/// same keys. Each flag narrows the search.
#[test]
fn search_takes_the_filters_of_the_tool_and_prints_its_hits() {
    let d = step_init();
    let (decision, _) = decision_and_fact(&d.root);
    let both = json(&ok(&d.root, &["search", "import staging"], None));
    assert_eq!(both.as_array().unwrap().len(), 2, "{both}");
    let line = ["search", "import staging", "--type", "decision"];
    let line = json(&ok(&d.root, &line, None));
    let args = serde_json::json!({"query": "import staging", "filters": {"type": "decision"}});
    let tool = json(&as_agent(&d.root, "search", &args).unwrap());
    assert_eq!(line.as_array().unwrap().len(), 1, "{line}");
    assert_eq!(line[0]["id"], decision.as_str());
    assert_eq!(line, tool["hits"]);
    assert!(keys(&line[0]).contains(&"snippet"), "{line}");
    let err = fails(&d.root, &["search", "import", "--min-tier", "gold"]);
    assert!(err.contains("--min-tier"), "{err}");
    for none in [
        ["--since", "2999-01-01T00:00:00Z"],
        ["--author", "agent:nobody"],
        ["--min-tier", "curated"],
    ] {
        let found = ok(&d.root, &["search", "import", none[0], none[1]], None);
        assert_eq!(json(&found), serde_json::json!([]), "{none:?}");
    }
}

/// `amem recall` with an answer kind or the tokens of a context prints what the tool
/// answers: the same keys and the same plan. A recall with neither has no plan.
#[test]
fn recall_takes_the_answer_kind_and_the_context_and_prints_the_plan_of_the_tool() {
    let d = step_init();
    decision_and_fact(&d.root);
    let question = "where does the import run";
    let line = ["recall", question, "--answer-kind", "fact"];
    let line = json(&ok(&d.root, &line, None));
    let args = serde_json::json!({"question": question, "answer_kind": "fact"});
    let tool = json(&as_agent(&d.root, "recall", &args).unwrap());
    assert_eq!(keys(&line), keys(&tool));
    assert_eq!(line["plan"], tool["plan"]);
    assert_eq!(line["plan"]["answer_kind"], "fact");
    let full = ["recall", question, "--context-tokens", "120000"];
    let full = json(&ok(&d.root, &full, None));
    assert_eq!(full["plan"]["context_tokens"], 120_000);
    let plain = json(&ok(&d.root, &["recall", question], None));
    assert!(plain.get("plan").is_none(), "{plain}");
    let err = fails(&d.root, &["recall", question, "--answer-kind", "essay"]);
    assert!(err.contains("is not fact, list or overview"), "{err}");
}

/// `amem export` without a destination keeps the bundle under the root and prints
/// where.
#[test]
fn export_without_a_destination_keeps_the_bundle_under_the_root() {
    let d = step_init();
    decision_and_fact(&d.root);
    let out = ok(&d.root, &["export", "--max-label", "internal"], None);
    let (said, at) = out.trim_end().split_once(", kept at ").expect(&out);
    assert_eq!(
        said,
        "exported 2 entries from dom-me, highest label internal"
    );
    let kept = Path::new(at);
    // the command prints the path as the system resolves it, so resolve the root too
    let exports = d.root.join("state/exports").canonicalize().unwrap();
    let under = kept.strip_prefix(&exports).expect(at);
    assert_eq!(under.components().count(), 2, "{at}");
    assert!(under.ends_with("dom-me-internal"), "{at}");
    assert!(kept.join("manifest.yaml").is_file(), "{at}");
}

/// `amem serve --person` is the session of the owner in person: the command line
/// refuses it another author, a position or an arm. Its `approve` records the approval
/// that a session with `--author` is refused, the owner's author included.
#[test]
fn serve_person_takes_no_author_and_approves_what_a_served_session_does_not() {
    let d = step_init();
    for flags in [
        ["--author", "agent:x"],
        ["--position", "pos-release"],
        ["--arm", "full"],
    ] {
        let err = fails(&d.root, &["serve", "--person", flags[0], flags[1]]);
        assert!(err.contains("cannot be used with"), "{flags:?}: {err}");
        assert!(err.contains("--person"), "{flags:?}: {err}");
    }
    let (_, id) = decision_and_fact(&d.root);
    let erase = serde_json::json!({"id": id, "reason": "mistaken"});
    let refused = as_agent(&d.root, "erase", &erase).unwrap_err();
    let named = refused
        .split_once("Task ")
        .and_then(|(_, r)| r.split_once(' '));
    let task = named.expect(&refused).0;
    let approve = serde_json::json!({ "task": task });
    for flags in [["--author", AGENT], ["--author", "human:test:me"]] {
        let why = as_served(&d.root, &flags, "approve", &approve).unwrap_err();
        assert!(why.contains("approves nothing"), "{flags:?}: {why}");
    }
    let listed = serde_json::json!({ "id": id });
    let none = json(&as_served(&d.root, &["--person"], "approvals", &listed).unwrap());
    assert_eq!(none["approvals"], serde_json::json!([]));
    let signed = json(&as_served(&d.root, &["--person"], "approve", &approve).unwrap());
    assert_eq!(signed["outcome"], "approved", "{signed}");
    assert_eq!(signed["approval"]["principal"], "human:test:me");
    let one = json(&as_served(&d.root, &["--person"], "approvals", &listed).unwrap());
    assert_eq!(one["approvals"][0]["in_force"], true, "{one}");
    let receipt = json(&as_agent(&d.root, "erase", &erase).unwrap());
    assert_eq!(receipt["erased_by"], AGENT);
}
