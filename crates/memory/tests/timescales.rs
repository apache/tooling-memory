//! The commands for the long timescales, run against the built binary: the registry
//! and its check, the proposals to a personal domain, the recovery bundle with its
//! restore and its restore drill, the declared outage, and the horizon scanner.

// An integration test is test code, but clippy's allow-unwrap-in-tests only sees
// #[cfg(test)] modules; here a panic is the failure being reported.
#![allow(clippy::unwrap_used)]

use serde_json::{json, Value};
use std::path::Path;
use std::process::{Command, Output, Stdio};

const OWNER: &str = "human:test,2026-11-01:me";
const SCANNER: &str = "human:test,2026-11-01:scout";

fn amem(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_amem"))
        .arg("--root")
        .arg(root)
        .args(args)
        .env_remove("MEMORY_ROOT")
        .env_remove("MEMORY_TASK")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

/// The JSON that a command prints.
fn ok(root: &Path, args: &[&str]) -> Value {
    let out = amem(root, args);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "amem {args:?} failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("not json ({e}): {text}"))
}

/// The entry that `amem get` prints, as Markdown.
fn entry(root: &Path, id: &str) -> String {
    let out = amem(root, &["get", id]);
    assert!(out.status.success(), "{out:?}");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// What a command that fails says.
fn fails(root: &Path, args: &[&str]) -> String {
    let out = amem(root, args);
    assert!(!out.status.success(), "amem {args:?} should have failed");
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// A root for [`OWNER`] with the domain `dom-me`, and the registry `text` if given.
fn root_with(registry: Option<&str>) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let out = amem(
        dir.path(),
        &["init", "--owner", OWNER, "--domain", "dom-me"],
    );
    assert!(out.status.success(), "{out:?}");
    if let Some(text) = registry {
        std::fs::write(dir.path().join("registry.toml"), text).unwrap();
    }
    dir
}

/// One unit that lists `dom-me`, with a horizon scanner, and `owner` as its owners.
fn unit(owner: &str) -> String {
    format!("[units.u1]\ndomains = [\"dom-me\"]\nhorizon_scanner = \"{SCANNER}\"\n{owner}")
}

/// Tool calls as `author` in one session, through `amem serve` over stdio: for each,
/// the JSON result, or the text of the refusal.
fn session(root: &Path, author: &str, calls: &[(&str, Value)]) -> Vec<Result<Value, String>> {
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
    let hello = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                   "clientInfo": {"name": "test", "version": "0"}}});
    writeln!(stdin, "{hello}").unwrap();
    stdout.read_line(&mut line).unwrap();
    let ready = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
    writeln!(stdin, "{ready}").unwrap();
    let mut results = Vec::new();
    for (n, (tool, arguments)) in calls.iter().enumerate() {
        let call = json!({"jsonrpc": "2.0", "id": n + 2, "method": "tools/call",
            "params": {"name": tool, "arguments": arguments}});
        writeln!(stdin, "{call}").unwrap();
        line.clear();
        stdout.read_line(&mut line).unwrap();
        let answer: Value = serde_json::from_str(&line).unwrap();
        results.push(match answer["error"]["message"].as_str() {
            Some(refused) => Err(refused.to_string()),
            None => {
                let text = answer["result"]["content"][0]["text"].as_str().unwrap();
                Ok(serde_json::from_str(text).unwrap_or_else(|_| json!(text)))
            }
        });
    }
    drop(stdin);
    child.wait().unwrap();
    results
}

/// One tool call as `author`.
fn call(root: &Path, author: &str, tool: &str, arguments: Value) -> Result<Value, String> {
    session(root, author, &[(tool, arguments)]).remove(0)
}

fn tasks(root: &Path, status: &str) -> Vec<Value> {
    let listed = ok(root, &["task", "list", "--status", status]);
    listed.as_array().unwrap().clone()
}

/// The tasks with one origin, proposed or open.
fn filed(root: &Path, origin: &str) -> Vec<Value> {
    let mut all = tasks(root, "proposed");
    all.extend(tasks(root, "open"));
    all.retain(|t| t["origin"] == origin);
    all
}

#[test]
fn init_gives_a_dated_owner_by_default_and_takes_both_forms() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("default");
    assert!(amem(&root, &["init"]).status.success());
    let grants = std::fs::read_to_string(root.join("grants.toml")).unwrap();
    let owner = grants
        .lines()
        .find_map(|l| l.strip_prefix("owner = "))
        .unwrap()
        .trim_matches('"')
        .to_string();
    let resolved = ok(&root, &["registry", "resolve", &owner]);
    assert_eq!(resolved["authority"], "local", "{owner}");
    assert_eq!(resolved["dated"], true, "{owner}");
    assert_eq!(resolved["is"], owner.as_str());
    assert_eq!(resolved["registry"], false);
    // the root works for that owner: a write lands and a search finds it
    let body = "The service listens on 8080.";
    let written = ok(&root, &["write", "--title", "The port", "--body", body]);
    let got = entry(&root, written["id"].as_str().unwrap());
    assert!(got.contains(&owner), "{got}");
    let hits = ok(&root, &["search", "listens 8080"]);
    assert_eq!(hits.as_array().unwrap().len(), 1, "{hits}");
    let plain = dir.path().join("plain");
    assert!(amem(&plain, &["init", "--owner", "human:test:me"])
        .status
        .success());
    let resolved = ok(&plain, &["registry", "resolve", "human:test:me"]);
    assert_eq!(resolved["dated"], false);
    assert_eq!(resolved["held"], Value::Null);
    let refused = fails(&plain, &["registry", "resolve", "agent:x"]);
    assert!(refused.contains("a person is"), "{refused}");
}

#[test]
fn registry_check_prints_each_flag_and_with_file_files_it_once() {
    let none = root_with(None);
    let bare = ok(none.path(), &["registry", "check", "--file"]);
    assert_eq!(
        bare,
        json!({"registry": false, "flags": [], "filed": [], "already_filed": 0})
    );
    let owner = format!("[[units.u1.owners]]\nid = \"{OWNER}\"\nuntil = 2026-12-01\n");
    let dir = root_with(Some(&unit(&owner)));
    let root = dir.path();
    let on = |day: &str| ok(root, &["registry", "check", "--today", day]);
    assert_eq!(on("2026-12-01")["flags"], json!([]), "the last day");
    let after = on("2026-12-02");
    assert_eq!(after["registry"], true);
    assert_eq!(
        after["flags"],
        json!([{"flag": "ownerless", "domain": "dom-me"}])
    );
    assert_eq!(after["filed"], json!([]), "nothing is filed without --file");
    assert!(filed(root, "registry:ownerless").is_empty());
    let file = ["registry", "check", "--file", "--today", "2026-12-02"];
    let first = ok(root, &file);
    assert_eq!(first["filed"].as_array().unwrap().len(), 1, "{first}");
    let task = filed(root, "registry:ownerless");
    assert_eq!(task.len(), 1);
    assert_eq!(task[0]["id"], first["filed"][0]);
    let again = ok(root, &file);
    assert_eq!(again["filed"], json!([]));
    assert_eq!(again["already_filed"], 1);
    assert_eq!(filed(root, "registry:ownerless").len(), 1);
    let bad = fails(root, &["registry", "check", "--today", "tomorrow"]);
    assert!(bad.contains("is not a date"), "{bad}");
}

#[test]
fn registry_resolve_follows_an_assertion_to_the_identifier_of_now() {
    let old = "human:old,2019-05-01:me";
    let text = format!("[[assertions]]\nwas = \"{old}\"\nis = \"{OWNER}\"\nsince = 2026-11-01\n");
    let dir = root_with(Some(&text));
    let found = ok(dir.path(), &["registry", "resolve", old]);
    assert_eq!(found["id"], old);
    assert_eq!(found["is"], OWNER);
    assert_eq!(found["authority"], "test");
    assert_eq!(found["held"], "2026-11-01");
    assert_eq!(found["user_id"], "me");
    // the memory server gives an agent the same answer
    let same = call(
        dir.path(),
        "agent:x",
        "resolve_identity",
        json!({"id": old}),
    );
    assert_eq!(same.unwrap(), found);
}

fn propose(root: &Path, body: &str) -> String {
    let held = call(root, "agent:x", "write", json!({ "body": body }));
    held.expect_err("a write to a personal domain is held")
}

fn proposals(root: &Path) -> Vec<Value> {
    let listed = ok(root, &["proposal", "list"]);
    listed["proposals"].as_array().unwrap().clone()
}

/// The id of the one proposal that waits.
fn waiting(root: &Path) -> String {
    let listed = proposals(root);
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0]["author"], "agent:x");
    assert!(listed[0].get("body").is_none(), "{listed:?}");
    listed[0]["id"].as_str().unwrap().to_string()
}

/// A proposal that the owner admits is in the record, with its author.
fn a_proposal_is_admitted(root: &Path) {
    let commits = "# Commits\n\nThe person prefers small commits that a reviewer reads.\n";
    let held = propose(root, commits);
    assert!(held.contains("held as a proposal"), "{held}");
    let first = waiting(root);
    let admitted = ok(root, &["proposal", "admit", &first]);
    assert_eq!(admitted["admitted"], true);
    assert_eq!(admitted["written"]["id"], first.as_str());
    assert!(entry(root, &first).contains("author: agent:x"));
    assert!(proposals(root).is_empty());
}

/// A proposal that the owner refuses is removed, and is not held again.
fn a_proposal_is_refused(root: &Path) {
    let reviews = "# Reviews\n\nThe person reads a review in the morning.\n";
    propose(root, reviews);
    let second = waiting(root);
    let refused = ok(root, &["proposal", "refuse", &second]);
    assert_eq!(refused["admitted"], false);
    assert_eq!(refused["written"], Value::Null);
    let again = propose(root, reviews);
    assert!(again.contains("refused this proposal before"), "{again}");
    assert!(proposals(root).is_empty());
}

/// A proposal expires after the period of its domain, and nothing is kept of it.
fn a_proposal_expires(root: &Path) {
    let lunch = "# Lunch\n\nThe person has lunch at noon.\n";
    propose(root, lunch);
    let third = waiting(root);
    let today = ok(root, &["proposal", "expire"]);
    assert_eq!(today["expired"], json!([]), "the period is not over");
    let later = ok(root, &["proposal", "expire", "--today", "2099-01-01"]);
    assert_eq!(later["expired"], json!([third]));
    assert!(proposals(root).is_empty());
    // the same proposal is held again
    let held = propose(root, lunch);
    assert!(held.contains("held as a proposal"), "{held}");
    let gone = fails(root, &["proposal", "admit", &third]);
    assert!(gone.contains(&third), "{gone}");
}

#[test]
fn a_proposal_is_listed_admitted_refused_and_expired_from_the_command_line() {
    let text = format!("[domains.dom-me]\nperson = \"{OWNER}\"\nproposal_days = 7\n");
    let dir = root_with(Some(&text));
    a_proposal_is_admitted(dir.path());
    a_proposal_is_refused(dir.path());
    a_proposal_expires(dir.path());
}

/// An admitted task that `agent:x` holds under a claim, and the generation of the claim.
fn claimed_task(root: &Path) -> (String, u64) {
    let created = ok(root, &["task", "create", "Move the relay"]);
    let id = created["id"].as_str().unwrap().to_string();
    if created["status"] == "proposed" {
        ok(root, &["task", "admit", &id]);
    }
    let claim = call(root, "agent:x", "claim", json!({ "id": id })).unwrap();
    (id, claim["generation"].as_u64().unwrap())
}

fn case<'a>(drill: &'a Value, name: &str) -> &'a Value {
    let cases = drill["cases"].as_array().unwrap();
    cases
        .iter()
        .find(|c| c["name"] == name)
        .unwrap_or_else(|| panic!("no case {name}: {drill}"))
}

/// A root with a claimed task and a note, a bundle of it, and the note erased after
/// the bundle: the root, the bundle, the task and the note.
fn bundled() -> (tempfile::TempDir, std::path::PathBuf, String, String) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("live");
    let init = amem(&root, &["init", "--owner", OWNER, "--domain", "dom-me"]);
    assert!(init.status.success(), "{init:?}");
    let (task, _) = claimed_task(&root);
    let body = "Written in error: the relay moved to room 4.12.";
    let note = ok(
        &root,
        &["write", "--title", "The relay moved", "--body", body],
    );
    let note = note["id"].as_str().unwrap().to_string();
    let bundle = dir.path().join("bundle");
    let made = ok(&root, &["recover", "bundle", bundle.to_str().unwrap()]);
    assert_eq!(made["made_by"], OWNER, "{made}");
    assert!(bundle.join("recovery.yaml").is_file());
    let erased = amem(&root, &["erase", &note, "--reason", "mistaken"]);
    assert!(erased.status.success(), "{erased:?}");
    (dir, bundle, task, note)
}

/// What the drill reports for a bundle with a claimed task and a later erasure.
fn the_drill_reports_the_cases(drill: &Value, task: &str) {
    assert_eq!(drill["passed"], true, "{drill}");
    assert_eq!(drill["epoch"], 1);
    let unfinished = case(drill, "unfinished-task");
    assert_eq!(unfinished["outcome"], "recovered", "{drill}");
    assert!(unfinished["detail"].as_str().unwrap().contains(task));
    let erasure = case(drill, "erasure-after-backup");
    assert_eq!(erasure["outcome"], "recovered", "{drill}");
    for name in ["cross-domain-dependency", "uncertain-external-action"] {
        assert_eq!(case(drill, name)["outcome"], "absent", "{drill}");
    }
}

/// A drill with `--scratch`: the JSON it prints, or what it says when it fails.
fn drill_into(root: &Path, bundle: &Path, scratch: &Path) -> Output {
    let (bundle, scratch) = (bundle.to_str().unwrap(), scratch.to_str().unwrap());
    amem(root, &["recover", "drill", bundle, "--scratch", scratch])
}

#[test]
fn recover_drill_restores_into_a_scratch_root_and_leaves_the_live_root_as_it_was() {
    let (dir, bundle, task, _note) = bundled();
    let root = dir.path().join("live");
    let grants = std::fs::read_to_string(root.join("grants.toml")).unwrap();
    let events = || ok(&root, &["log", "show"]).as_array().unwrap().len();
    let before = events();
    let drill = ok(&root, &["recover", "drill", bundle.to_str().unwrap()]);
    the_drill_reports_the_cases(&drill, &task);
    // the scratch root is a new directory under the recovery directory of the live root
    let scratch = std::path::PathBuf::from(drill["scratch"].as_str().unwrap());
    // the command prints the path as the system resolves it, so resolve the root too
    let recovery = root.join("state/recovery").canonicalize().unwrap();
    assert!(scratch.starts_with(&recovery), "{drill}");
    assert!(scratch.join("grants.toml").is_file());
    // the live root: the same grants, no new event, and the claim still in force
    let grants_now = std::fs::read_to_string(root.join("grants.toml")).unwrap();
    assert_eq!(grants_now, grants);
    assert_eq!(events(), before);
    let claimed = tasks(&root, "claimed");
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0]["id"], task.as_str());
    let epoch = std::fs::read_to_string(root.join("claim-epoch")).unwrap();
    assert_eq!(epoch.trim(), "1");
}

#[test]
fn recover_drill_refuses_a_scratch_root_that_holds_something_and_takes_one_that_is_named() {
    let (dir, bundle, _task, _note) = bundled();
    let root = dir.path().join("live");
    // the live root first of all
    let refused = drill_into(&root, &bundle, &root);
    assert!(!refused.status.success(), "{refused:?}");
    let said = String::from_utf8_lossy(&refused.stderr);
    assert!(said.contains("holds a root already"), "{said}");
    assert!(
        !root.join("claim-epoch").exists(),
        "a refused drill takes no epoch"
    );
    let named = dir.path().join("scratch");
    let first = drill_into(&root, &bundle, &named);
    assert!(first.status.success(), "{first:?}");
    let first: Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(first["epoch"], 1);
    assert_eq!(first["scratch"], named.to_str().unwrap());
    // each restore takes the next claim epoch
    let second = ok(&root, &["recover", "drill", bundle.to_str().unwrap()]);
    assert_eq!(second["epoch"], 2);
}

#[test]
fn recover_restore_makes_a_root_that_is_not_there_from_a_bundle_and_the_journal() {
    let (dir, bundle, task, note) = bundled();
    let root = dir.path().join("live");
    let journal = dir.path().join("journal");
    let kept = ok(&root, &["recover", "journal", journal.to_str().unwrap()]);
    assert_eq!(kept["receipts"], 1, "{kept}");
    let new = dir.path().join("restored");
    let authority = dir.path().join("claim-epoch");
    let grants = root.join("grants.toml");
    let args = [
        "recover",
        "restore",
        bundle.to_str().unwrap(),
        "--grants",
        grants.to_str().unwrap(),
        "--journal",
        journal.to_str().unwrap(),
        "--epoch-authority",
        authority.to_str().unwrap(),
    ];
    let restored = ok(&new, &args);
    assert_eq!(restored["epoch"], 1, "{restored}");
    assert_eq!(restored["reopened"], json!([task]));
    assert_eq!(restored["erased"], json!([note]));
    assert_eq!(std::fs::read_to_string(&authority).unwrap().trim(), "1");
    // the restored root serves: the task is open again, and the erased note is erased
    let open = tasks(&new, "open");
    assert_eq!(open.len(), 1);
    assert_eq!(open[0]["id"], task.as_str());
    let hits = ok(&new, &["search", "relay moved room"]);
    assert_eq!(hits, json!([]));
    // a second restore into the same root is refused, and so is a command that needs
    // a root on a directory that is not there
    let refused = fails(&new, &args);
    assert!(refused.contains("holds a root already"), "{refused}");
    let absent = fails(&dir.path().join("nowhere"), &["outage", "status"]);
    assert!(absent.contains("no record at"), "{absent}");
}

#[test]
fn an_outage_is_declared_and_ended_from_the_command_line_and_the_queue_lands() {
    let dir = root_with(None);
    let root = dir.path();
    assert_eq!(ok(root, &["outage", "status"])["outage"], Value::Null);
    let declared = ok(root, &["outage", "declare", "--reason", "the ledger moves"]);
    assert_eq!(declared["reason"], "the ledger moves");
    assert_eq!(declared["declared_by"], OWNER);
    let status = ok(root, &["outage", "status"]);
    assert_eq!(status["outage"], declared);
    let waits = fails(root, &["search", "anything"]);
    assert!(waits.contains("an outage is declared"), "{waits}");
    // one session of an agent: its status, a read that waits, and a handoff that is queued
    let handoff = "# Handoff\n\nThe relay is half done. The tests of the retry are next.\n";
    let calls = [
        ("outage_status", json!({})),
        ("search", json!({"query": "relay"})),
        ("write", json!({"body": handoff, "entry_type": "handoff"})),
    ];
    let mut answers = session(root, "agent:x", &calls);
    let queued = answers.pop().unwrap().unwrap_err();
    assert!(queued.contains("The handoff is queued"), "{queued}");
    let read = answers.pop().unwrap().unwrap_err();
    assert!(read.contains("an outage is declared"), "{read}");
    let seen = answers.pop().unwrap().unwrap();
    assert_eq!(seen["outage"]["reason"], "the ledger moves");
    assert_eq!(seen["t0_loaded"], json!([]));
    let ended = ok(root, &["outage", "end"]);
    assert_eq!(ended["handoffs"].as_array().unwrap().len(), 1, "{ended}");
    assert_eq!(ended["failed"], json!([]));
    assert_eq!(ok(root, &["outage", "status"])["outage"], Value::Null);
    let landed = entry(root, ended["handoffs"][0].as_str().unwrap());
    assert!(landed.contains("author: agent:x"), "{landed}");
    assert!(landed.contains("The relay is half done"), "{landed}");
}

#[test]
fn horizon_schedule_files_the_tasks_of_a_period_once() {
    let owner = format!("[[units.u1.owners]]\nid = \"{OWNER}\"\n");
    let dir = root_with(Some(&unit(&owner)));
    let root = dir.path();
    let args = ["horizon", "schedule", "--today", "2026-11-25"];
    let first = ok(root, &args);
    let scheduled = first["scheduled"].as_array().unwrap();
    assert_eq!(scheduled.len(), 1, "{first}");
    assert_eq!(scheduled[0]["unit"], "u1");
    assert_eq!(scheduled[0]["scanner"], SCANNER);
    assert_eq!(scheduled[0]["filed"], true);
    let again = ok(root, &args);
    assert_eq!(again["scheduled"][0]["filed"], false);
    assert_eq!(again["scheduled"][0]["evidence"], scheduled[0]["evidence"]);
    assert_eq!(filed(root, "horizon:evidence").len(), 1);
    // the review task waits for the evidence task
    let review = tasks(root, "blocked");
    assert_eq!(review.len(), 1, "{review:?}");
    assert_eq!(review[0]["origin"], "horizon:review");
    assert_eq!(review[0]["id"], scheduled[0]["review"]);
    // the memory server refuses an agent in the words of the library
    let refused = call(root, "agent:x", "horizon_schedule", json!({})).unwrap_err();
    assert!(refused.contains("only a person schedules"), "{refused}");
}
