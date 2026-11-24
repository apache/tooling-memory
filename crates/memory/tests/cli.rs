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
