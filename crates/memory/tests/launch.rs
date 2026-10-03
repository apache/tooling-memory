//! The launcher, driven with a fake harness: the unconfined path everywhere, the confined
//! path where this kernel allows unprivileged namespaces.

// An integration test is test code; a panic here is the failure being reported.
#![allow(clippy::unwrap_used)]
// The fake harness is a shell script and the egress socket is a unix socket.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn memory() -> Command {
    Command::new(env!("CARGO_BIN_EXE_memory"))
}

/// A fake `claude` that writes its environment and arguments to a file in the working
/// tree, asks the egress for T0 over MCP, and exits with a distinctive code.
fn fake_claude(dir: &Path) -> PathBuf {
    let bins = dir.join("bins");
    std::fs::create_dir_all(&bins).unwrap();
    let script = r#"#!/bin/sh
{
  echo "cwd=$(pwd)"
  env | grep -E '^(ANTHROPIC_BASE_URL|ANTHROPIC_AUTH_TOKEN|HOME|PATH)=' | sort
  echo "args=$*"
  base="${ANTHROPIC_BASE_URL%/anthropic}"
  echo "mcp=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$base/mcp" -H 'content-type: application/json' -H 'accept: application/json, text/event-stream' -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"fake","version":"0"}}}')"
  echo "nowhere=$(curl -s -o /dev/null -w '%{http_code}' "$base/nowhere/x")"
  echo "ifaces=$(tail -n +3 /proc/net/dev 2>/dev/null | cut -d: -f1 | tr -d ' ' | tr '\n' ',')"
  echo "hosthome=$(ls /home 2>/dev/null | tr '\n' ',')"
} > ./fake-claude.out 2>&1
exit 42
"#;
    let p = bins.join("claude");
    std::fs::write(&p, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bins
}

fn path_with(bins: &Path) -> std::ffi::OsString {
    let old = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![bins.to_path_buf()];
    paths.extend(std::env::split_paths(&old));
    std::env::join_paths(paths).unwrap()
}

fn init_root(dir: &Path) -> PathBuf {
    let root = dir.join("memory");
    let out = memory()
        .args([
            "--root",
            root.to_str().unwrap(),
            "init",
            "--owner",
            "human:test:me",
            "--domain",
            "dom-me",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    root
}

fn launch(root: &Path, bins: &Path, work: &Path, extra: &[&str]) -> Output {
    let mut cmd = memory();
    cmd.args([
        "--root",
        root.to_str().unwrap(),
        "launch",
        "--harness",
        "claude",
        "--auth",
        "key",
    ]);
    cmd.args(["--workdir", work.to_str().unwrap()]);
    cmd.args(extra);
    cmd.args(["--", "-p", "hello"]);
    cmd.env("PATH", path_with(bins));
    cmd.env("ANTHROPIC_API_KEY", "sk-ant-placeholder");
    cmd.stdin(Stdio::null());
    cmd.output().unwrap()
}

fn read_out(work: &Path) -> String {
    std::fs::read_to_string(work.join("fake-claude.out")).unwrap_or_default()
}

#[test]
fn unconfined_launch_wires_the_egress_and_propagates_the_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    let root = init_root(dir.path());
    let bins = fake_claude(dir.path());
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();

    let out = launch(&root, &bins, &work, &["--unconfined"]);
    assert_eq!(
        out.status.code(),
        Some(42),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = read_out(&work);
    assert!(
        text.contains("ANTHROPIC_BASE_URL=http://127.0.0.1:"),
        "{text}"
    );
    assert!(
        text.contains("ANTHROPIC_AUTH_TOKEN=egress"),
        "a placeholder, not the key: {text}"
    );
    assert!(
        !text.contains("sk-ant-placeholder"),
        "the key never reaches the harness: {text}"
    );
    assert!(
        text.contains("mcp=200"),
        "memory over MCP through the egress: {text}"
    );
    assert!(text.contains("nowhere=404"), "{text}");
    assert!(text.contains("args=--mcp-config"), "{text}");
    assert!(text.contains("-p hello"), "{text}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("model calls via anthropic at ceiling internal"),
        "{stderr}"
    );

    let usage = memory()
        .args(["--root", root.to_str().unwrap(), "usage"])
        .output()
        .unwrap();
    assert!(usage.status.success());
    assert!(String::from_utf8_lossy(&usage.stdout).contains("no model calls recorded"));
}

#[test]
fn launch_refuses_without_the_credential_its_mode_needs() {
    let dir = tempfile::tempdir().unwrap();
    let root = init_root(dir.path());
    let bins = fake_claude(dir.path());
    let out = memory()
        .args([
            "--root",
            root.to_str().unwrap(),
            "launch",
            "--harness",
            "claude",
            "--auth",
            "key",
            "--unconfined",
        ])
        .env("PATH", path_with(&bins))
        .env_remove("ANTHROPIC_API_KEY")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("ANTHROPIC_API_KEY"));
    let out = memory()
        .args([
            "--root",
            root.to_str().unwrap(),
            "launch",
            "--harness",
            "claude",
            "--auth",
            "gateway",
            "--unconfined",
        ])
        .env("PATH", path_with(&bins))
        .env_remove("LLMAO_KEY")
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stderr).contains("LLMAO_KEY"));
}

/// Whether this kernel lets an unprivileged process create user and network namespaces.
/// Ubuntu's AppArmor restriction and some CI kernels say no; the confined test is then
/// reported as skipped rather than failed.
#[cfg(target_os = "linux")]
fn namespaces_available() -> bool {
    Command::new("unshare")
        .args(["--user", "--map-root-user", "--net", "--mount", "true"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(target_os = "linux")]
#[test]
fn confined_launch_has_no_network_and_no_host_home() {
    if !namespaces_available() {
        eprintln!("skipping: unprivileged namespaces are not available here");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = init_root(dir.path());
    // the fake harness has to live where the sandbox can see it: in the working tree
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let bins = fake_claude(&work);

    let out = launch(&root, &bins, &work, &[]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(42), "{stderr}");
    let text = read_out(&work);
    assert!(
        text.contains("mcp=200"),
        "the egress is reachable through the relay: {text}"
    );
    assert!(text.contains("ifaces=lo,"), "only loopback inside: {text}");
    assert!(!text.contains("ifaces=lo,eth"), "{text}");
    assert!(
        text.contains("hosthome=user,"),
        "the host's /home is not there: {text}"
    );
    assert!(text.contains("HOME=/home/user"), "{text}");
    assert!(text.contains(&format!("cwd={}", work.display())), "{text}");
    assert!(stderr.contains("confined to"), "{stderr}");
}

// ---- non-interactive: a prompt, and a task from the ledger ----

/// A fake harness from a shell body. It receives the launcher's arguments; `$*` holds
/// the prompt among them.
fn fake_harness(dir: &Path, body: &str) -> PathBuf {
    let bins = dir.join("bins");
    std::fs::create_dir_all(&bins).unwrap();
    let p = bins.join("claude");
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    bins
}

/// Shell that speaks MCP to the egress with curl: initialize, write a handoff, complete
/// the task named in the prompt.
const COMPLETES_ITS_TASK: &str = r#"
task=$(printf '%s' "$*" | grep -o 'task-[a-z0-9]*-[a-z0-9]*' | head -1)
base="${ANTHROPIC_BASE_URL%/anthropic}"
H1='content-type: application/json'; H2='accept: application/json, text/event-stream'
sid=$(curl -s -D - -o /dev/null -X POST "$base/mcp" -H "$H1" -H "$H2" -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"fake","version":"0"}}}' | tr -d '\r' | awk 'tolower($1)=="mcp-session-id:"{print $2}')
curl -s -o /dev/null -X POST "$base/mcp" -H "$H1" -H "$H2" -H "mcp-session-id: $sid" -d '{"jsonrpc":"2.0","method":"notifications/initialized"}'
hid=$(curl -s -X POST "$base/mcp" -H "$H1" -H "$H2" -H "mcp-session-id: $sid" -d '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"write","arguments":{"body":"Import finished, all five steps.","entry_type":"handoff","title":"Import done"}}}' | grep -o 'mem-[a-z0-9]*-[a-z0-9]*' | head -1)
curl -s -o /dev/null -X POST "$base/mcp" -H "$H1" -H "$H2" -H "mcp-session-id: $sid" -d "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"complete\",\"arguments\":{\"id\":\"$task\",\"handoff\":\"$hid\"}}}"
echo "finished $task with $hid"
"#;

fn cli_text(root: &Path, args: &[&str]) -> String {
    let out = memory()
        .args(["--root", root.to_str().unwrap()])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn cli(root: &Path, args: &[&str]) -> serde_json::Value {
    let text = cli_text(root, args);
    serde_json::from_str(&text).unwrap_or(serde_json::Value::Null)
}

fn file_task(root: &Path, title: &str) -> String {
    let t = cli(
        root,
        &[
            "task",
            "create",
            title,
            "--priority",
            "5",
            "--body",
            "Steps 3 to 5.",
        ],
    );
    let id = t["id"].as_str().unwrap().to_string();
    cli(root, &["task", "admit", &id]);
    id
}

fn work(root: &Path, bins: &Path, workdir: &Path) -> Output {
    let mut cmd = memory();
    cmd.args([
        "--root",
        root.to_str().unwrap(),
        "work",
        "--harness",
        "claude",
        "--auth",
        "key",
        "--unconfined",
    ]);
    cmd.args([
        "--workdir",
        workdir.to_str().unwrap(),
        "--lease-seconds",
        "60",
    ]);
    cmd.env("PATH", path_with(bins));
    cmd.env("ANTHROPIC_API_KEY", "sk-ant-placeholder");
    cmd.stdin(Stdio::null());
    cmd.output().unwrap()
}

#[test]
fn work_leaves_a_completed_task_complete() {
    let dir = tempfile::tempdir().unwrap();
    let root = init_root(dir.path());
    let bins = fake_harness(dir.path(), COMPLETES_ITS_TASK);
    let work_dir = dir.path().join("work");
    std::fs::create_dir_all(&work_dir).unwrap();
    let id = file_task(&root, "Finish the import");

    let out = work(&root, &bins, &work_dir);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{stderr}\n{stdout}");
    assert!(
        stderr.contains(&format!("claimed {id}: Finish the import")),
        "{stderr}"
    );
    assert!(stderr.contains("completed, handoff mem-"), "{stderr}");
    let t = cli(&root, &["task", "get", &id]);
    assert_eq!(t["status"], "completed");
    let h = t["handoff"].as_str().unwrap();
    let e = cli_text(&root, &["get", h]);
    assert!(e.contains("all five steps"), "{e}");
}

#[test]
fn work_files_the_output_and_releases_when_the_harness_stops_short() {
    let dir = tempfile::tempdir().unwrap();
    let root = init_root(dir.path());
    let bins = fake_harness(
        dir.path(),
        "echo \"args=$*\" > ./fake.out; echo 'I looked at step 3 and stopped.'; exit 0",
    );
    let work_dir = dir.path().join("work");
    std::fs::create_dir_all(&work_dir).unwrap();
    let id = file_task(&root, "Finish the import");

    let out = work(&root, &bins, &work_dir);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(4), "{stderr}");
    assert!(
        stderr.contains("released; its output is filed as handoff"),
        "{stderr}"
    );
    let t = cli(&root, &["task", "get", &id]);
    assert_eq!(t["status"], "admitted", "back in the pool: {t}");
    let h = t["handoff"]
        .as_str()
        .expect("the handoff rides with the task");
    let e = cli_text(&root, &["get", h]);
    assert!(e.contains("I looked at step 3"), "{e}");
    // the harness saw its headless form, the task, and T0
    let args = std::fs::read_to_string(work_dir.join("fake.out")).unwrap();
    assert!(args.contains("-p "), "{args}");
    assert!(args.contains(&id), "{args}");
    assert!(
        args.contains("--dangerously-skip-permissions"),
        "the sandbox is the boundary: {args}"
    );
    assert!(args.contains("# T0 for dom-me"), "{args}");
    assert!(args.contains("do not call `heartbeat`"), "{args}");

    // the next claimant is told where the last attempt stopped
    let out = work(&root, &bins, &work_dir);
    assert_eq!(out.status.code(), Some(4));
    let args = std::fs::read_to_string(work_dir.join("fake.out")).unwrap();
    assert!(args.contains("Where the last attempt stopped"), "{args}");
    assert!(args.contains("I looked at step 3"), "{args}");
}

#[test]
fn work_with_nothing_claimable_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let root = init_root(dir.path());
    let bins = fake_harness(dir.path(), "exit 0");
    let work_dir = dir.path().join("work");
    std::fs::create_dir_all(&work_dir).unwrap();
    let out = work(&root, &bins, &work_dir);
    assert_eq!(out.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&out.stderr).contains("nothing claimable"));
}

#[test]
fn a_prompted_launch_runs_headless_and_insists_on_a_handoff() {
    let dir = tempfile::tempdir().unwrap();
    let root = init_root(dir.path());
    let bins = fake_harness(dir.path(), "echo \"args=$*\"; echo 'the answer'; exit 0");
    let work_dir = dir.path().join("work");
    std::fs::create_dir_all(&work_dir).unwrap();
    let run = |extra: &[&str]| {
        let mut cmd = memory();
        cmd.args([
            "--root",
            root.to_str().unwrap(),
            "launch",
            "--harness",
            "claude",
            "--auth",
            "key",
            "--unconfined",
        ]);
        cmd.args([
            "--workdir",
            work_dir.to_str().unwrap(),
            "--prompt",
            "say the answer",
        ]);
        cmd.args(extra);
        cmd.env("PATH", path_with(&bins));
        cmd.env("ANTHROPIC_API_KEY", "sk-ant-placeholder");
        cmd.stdin(Stdio::null());
        cmd.output().unwrap()
    };
    let out = run(&[]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(4), "{stderr}");
    assert!(
        stdout.contains("args=") && stdout.contains("-p say the answer"),
        "{stdout}"
    );
    assert!(
        stdout.contains("the answer"),
        "the harness's output is printed: {stdout}"
    );
    assert!(stderr.contains("left no handoff"), "{stderr}");
    let out = run(&["--no-handoff"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ---- the wslc backend's inside half, driven by a fake host ----

/// `memory inside` writes run requests for the host and waits for exit codes. This fake
/// host answers them from the test, which proves the protocol and the plan the inside
/// builds for the harness image without a container runtime.
/// The host's side of the protocol: wait for the request file.
fn await_request(req_path: &Path) -> serde_json::Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !req_path.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    serde_json::from_str(&std::fs::read_to_string(req_path).unwrap()).unwrap()
}

/// What a request for the harness image must say.
fn check_image_request(req: &serde_json::Value) {
    let argv: Vec<&str> = req["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(argv[0], "/opt/memory/memory", "{argv:?}");
    assert!(
        argv.contains(&"/usr/local/bin/claude"),
        "the harness at the image's path: {argv:?}"
    );
    assert!(
        argv.contains(&"-p") && argv.contains(&"say the answer"),
        "{argv:?}"
    );
    assert_eq!(req["cwd"], "/work");
    assert_eq!(req["home"], "state/sandbox/claude");
    assert_eq!(req["interactive"], false);
    assert_eq!(
        req["env"]["ANTHROPIC_BASE_URL"],
        "http://127.0.0.1:4100/anthropic"
    );
    assert_eq!(req["env"]["HOME"], "/home/user");
}

#[test]
fn inside_writes_image_requests_and_reports_done() {
    let dir = tempfile::tempdir().unwrap();
    let root = init_root(dir.path());
    let requests = dir.path().join("volume");
    std::fs::create_dir_all(&requests).unwrap();
    let host_home = dir.path().join("host-home");
    std::fs::create_dir_all(host_home.join(".claude")).unwrap();
    std::fs::write(host_home.join(".claude/.credentials.json"), "{}").unwrap();

    let child = memory()
        .args([
            "--root",
            root.to_str().unwrap(),
            "inside",
            "--harness",
            "claude",
            "--auth",
            "subscription",
        ])
        .args([
            "--requests",
            requests.to_str().unwrap(),
            "--host-home",
            host_home.to_str().unwrap(),
        ])
        .args(["--prompt", "say the answer", "--no-handoff"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let req = await_request(&requests.join("req-0.json"));
    check_image_request(&req);
    std::fs::write(requests.join("req-0.out"), "the answer\n").unwrap();
    std::fs::write(requests.join("req-0.exit"), "0\n").unwrap();

    let out = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    assert!(
        stdout.contains("the answer"),
        "the captured output is printed: {stdout}"
    );
    assert!(
        stderr.contains("in the harness image, working tree at /work"),
        "{stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(requests.join("done"))
            .unwrap()
            .trim(),
        "0"
    );
}

// ---- hooks: a handoff the harness did not write ----

/// A fake harness that behaves like Claude Code's hooks: before "compaction" and at
/// session end it runs `memory hook claude` with the event on standard input and a
/// transcript on disk, and writes no handoff of its own.
const FIRES_HOOKS: &str = r#"
t=./transcript.jsonl
printf '%s\n' '{"type":"user","message":{"role":"user","content":"Finish the import."}}' '{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Step 3 done, starting step 4."}]}}' > "$t"
printf '{"hook_event_name":"PreCompact","session_id":"fake-session-1","transcript_path":"%s","trigger":"auto"}' "$t" | "$MEMORY_BIN" hook claude
printf '{"hook_event_name":"SessionEnd","session_id":"fake-session-1","transcript_path":"%s","reason":"other"}' "$t" | "$MEMORY_BIN" hook claude
echo "the answer"
"#;

#[test]
fn hook_notes_become_handoffs_when_the_run_left_none() {
    let dir = tempfile::tempdir().unwrap();
    let root = init_root(dir.path());
    let bins = fake_harness(dir.path(), FIRES_HOOKS);
    let work_dir = dir.path().join("work");
    std::fs::create_dir_all(&work_dir).unwrap();
    let mut cmd = memory();
    cmd.args([
        "--root",
        root.to_str().unwrap(),
        "launch",
        "--harness",
        "claude",
        "--auth",
        "key",
        "--unconfined",
    ]);
    cmd.args([
        "--workdir",
        work_dir.to_str().unwrap(),
        "--prompt",
        "finish the import",
    ]);
    cmd.env("PATH", path_with(&bins));
    cmd.env("ANTHROPIC_API_KEY", "sk-ant-placeholder");
    cmd.env("MEMORY_BIN", env!("CARGO_BIN_EXE_memory"));
    cmd.stdin(Stdio::null());
    let out = cmd.output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(0),
        "the session-end note is the handoff of last resort: {stderr}"
    );
    assert!(
        stderr.contains("from the harness's hook (precompact)"),
        "{stderr}"
    );
    assert!(
        stderr.contains("from the harness's hook (sessionend)"),
        "{stderr}"
    );
    let list = cli(&root, &["list", "--kind", "handoff"]);
    let titles: Vec<String> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["title"].as_str().unwrap().to_string())
        .collect();
    assert!(
        titles
            .iter()
            .any(|t| t.starts_with("Session fake-ses before compaction (auto)")),
        "{titles:?}"
    );
    assert!(
        titles
            .iter()
            .any(|t| t.starts_with("Session fake-ses at session end (other)")),
        "{titles:?}"
    );
    let id = list[0]["id"].as_str().unwrap();
    let body = cli_text(&root, &["get", id]);
    assert!(
        body.contains("Step 3 done, starting step 4."),
        "the transcript's tail is in the handoff: {body}"
    );
    assert!(body.contains("Left by the launcher's hook"), "{body}");
    // nothing left waiting
    assert!(
        std::fs::read_dir(root.join("state/sandbox/claude/.memory-outbox"))
            .map(|d| d.count() == 0)
            .unwrap_or(true)
    );
}
