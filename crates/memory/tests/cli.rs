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
    ok(&d.root, &["task", "heartbeat", &id], None);
    let done = json(&ok(&d.root, &["task", "complete", &id, &d.handoff], None));
    assert_eq!(done["status"], "completed");
    assert_eq!(done["handoff"], d.handoff);
    let completed = json(&ok(
        &d.root,
        &["task", "list", "--status", "completed"],
        None,
    ));
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
    assert!(out.contains("imported 2 entries, skipped 0"), "{out}");
    let out = ok(&other, &["import", bundle_arg], None);
    assert!(out.contains("imported 0 entries, skipped 2"), "{out}");
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
