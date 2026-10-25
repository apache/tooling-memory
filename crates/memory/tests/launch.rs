//! The launcher, driven with a fake harness: the unconfined path everywhere, the confined
//! path where this kernel allows unprivileged namespaces.

// An integration test is test code; a panic here is the failure being reported.
#![allow(clippy::unwrap_used)]
// The fake harness is a shell script and the egress socket is a unix socket.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn memory() -> Command {
    Command::new(env!("CARGO_BIN_EXE_amem"))
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
    the_run_is_kept_with_what_it_cost_the_host(&root);
}

/// One run, kept with its wall clock and what the kernel counted, and shown by `cost`.
fn the_run_is_kept_with_what_it_cost_the_host(root: &Path) {
    let cost = memory()
        .args(["--root", root.to_str().unwrap(), "cost", "--json"])
        .output()
        .unwrap();
    assert!(cost.status.success());
    let r: serde_json::Value = serde_json::from_slice(&cost.stdout).unwrap();
    let s = &r["sessions"][0];
    assert_eq!(s["runs"], 1, "{r}");
    assert_eq!(s["measured"], true, "{r}");
    assert!(s["wall_ms"].as_u64().unwrap() > 0, "{r}");
    assert!(s["max_rss_kib"].as_u64().unwrap() > 0, "{r}");
    let text = memory()
        .args(["--root", root.to_str().unwrap(), "cost"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&text.stdout).contains("host resources"));
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
/// Ubuntu's AppArmor restriction says no by default; a confined test is then reported as
/// skipped rather than failed, except on CI, which is set up to run it and fails instead.
#[cfg(target_os = "linux")]
fn namespaces_available() -> bool {
    let available = Command::new("unshare")
        .args(["--user", "--map-root-user", "--net", "--mount", "true"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    assert!(
        available || std::env::var_os("CI").is_none(),
        "unprivileged namespaces are not available, and on CI the confined tests must run"
    );
    available
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
    let left: Vec<_> = std::fs::read_dir(root.join("state/run"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("egress-"))
        .collect();
    assert!(
        left.is_empty(),
        "the session takes its socket away: {left:?}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn a_session_s_projection_holds_its_own_render_and_nothing_another_left() {
    if !namespaces_available() {
        eprintln!("skipping: unprivileged namespaces are not available here");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = init_root(dir.path());
    // what another session of the same harness, at a higher ceiling, left in the
    // directory sessions of this harness once shared
    let shared = root.join("state/projection/claude/dom-other");
    std::fs::create_dir_all(&shared).unwrap();
    std::fs::write(
        shared.join("README.md"),
        "- [The key](mem-x.md) · restricted: the signing key lives in the vault\n",
    )
    .unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let bins = fake_harness(
        &work,
        "{ echo \"seen=$(ls /memory | tr '\\n' ',')\"; cat /memory/dom-other/README.md 2>&1; } > ./fake-claude.out\nexit 0",
    );

    let out = launch(&root, &bins, &work, &[]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    let text = read_out(&work);
    assert!(
        text.contains("seen=dom-me,"),
        "its own render is mounted: {text}"
    );
    assert!(
        !text.contains("signing key"),
        "another session's render is not this one's to read: {text}"
    );
    // and the run takes its own render away with it
    let left: Vec<_> = std::fs::read_dir(root.join("state/projection/claude"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(left, vec!["dom-other".to_string()], "{left:?}");
}

#[cfg(target_os = "linux")]
#[test]
fn a_launch_that_fails_after_rendering_leaves_no_render_behind() {
    if !namespaces_available() {
        eprintln!("skipping: unprivileged namespaces are not available here");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = init_root(dir.path());
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    // no claude anywhere: the render is made, then the plan cannot find the harness
    let empty = dir.path().join("empty-bin");
    std::fs::create_dir_all(&empty).unwrap();
    let mut cmd = memory();
    cmd.args([
        "--root",
        root.to_str().unwrap(),
        "launch",
        "--harness",
        "claude",
    ])
    .args(["--auth", "key", "--workdir", work.to_str().unwrap()])
    .env("PATH", &empty)
    .env("ANTHROPIC_API_KEY", "sk-ant-placeholder")
    .stdin(Stdio::null());
    let out = cmd.output().unwrap();
    assert!(!out.status.success());
    let left: Vec<_> = std::fs::read_dir(root.join("state/projection/claude"))
        .map(|d| d.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    assert!(left.is_empty(), "a render outlived its session: {left:?}");
}

/// A root whose egress socket, `state/run/egress-<pid>.sock`, is longer than any system
/// lets a unix socket path be.
fn root_too_long_for_its_socket(dir: &Path) -> PathBuf {
    let deep = dir.join("d".repeat(120));
    std::fs::create_dir_all(&deep).unwrap();
    init_root(&deep)
}

#[cfg(target_os = "linux")]
#[test]
fn a_confined_launch_whose_socket_cannot_be_bound_fails_and_says_why() {
    if !namespaces_available() {
        eprintln!("skipping: unprivileged namespaces are not available here");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = root_too_long_for_its_socket(dir.path());
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let bins = fake_claude(&work);

    let out = launch(&root, &bins, &work, &[]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(
        !work.join("fake-claude.out").exists(),
        "the harness did not start: {stderr}"
    );
    let prefix = root.join("state/run/egress-").display().to_string();
    let at = stderr.find(&prefix).expect(&stderr);
    let end = stderr[at..].find(".sock").expect(&stderr) + ".sock".len();
    let socket = &stderr[at..at + end];
    assert!(
        stderr.contains(&format!("{} bytes", socket.len())),
        "the error names the socket's length: {stderr}"
    );
}

#[test]
fn an_unconfined_launch_needs_no_socket_and_runs_from_a_long_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = root_too_long_for_its_socket(dir.path());
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
    assert!(read_out(&work).contains("mcp=200"), "{}", read_out(&work));
}

#[test]
fn an_unconfined_launch_binds_no_socket() {
    let dir = tempfile::tempdir().unwrap();
    let root = init_root(dir.path());
    let run = root.join("state/run");
    let bins = fake_harness(
        dir.path(),
        &format!("ls -a '{}' > ./run.ls; exit 0", run.display()),
    );
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();

    let out = launch(&root, &bins, &work, &["--unconfined"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    let seen = std::fs::read_to_string(work.join("run.ls")).unwrap();
    assert!(
        !seen.contains("egress-"),
        "state/run while the harness ran: {seen}"
    );
}

/// `amem inside` with this requests directory, a subscription login in a host home, and
/// a prompt; the caller answers the requests.
fn inside(root: &Path, requests: &Path, host_home: &Path) -> std::process::Child {
    std::fs::create_dir_all(host_home.join(".claude")).unwrap();
    std::fs::write(host_home.join(".claude/.credentials.json"), "{}").unwrap();
    memory()
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
        .unwrap()
}

#[test]
fn inside_with_a_requests_directory_too_long_for_its_socket_fails_and_says_why() {
    let dir = tempfile::tempdir().unwrap();
    let root = init_root(dir.path());
    let requests = dir.path().join("v".repeat(120));
    std::fs::create_dir_all(&requests).unwrap();
    let socket = requests.join("egress.sock");

    let out = inside(&root, &requests, &dir.path().join("host-home"))
        .wait_with_output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains(&format!("{} bytes", socket.as_os_str().len())),
        "the error names the socket's length: {stderr}"
    );
    assert!(
        !requests.join("req-0.json").exists(),
        "no run was asked for"
    );
}

#[test]
fn a_relay_that_cannot_listen_does_not_run_the_harness() {
    let dir = tempfile::tempdir().unwrap();
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let listen = taken.local_addr().unwrap().to_string();
    let marker = dir.path().join("ran");
    let out = memory()
        .args(["relay", "--socket"])
        .arg(dir.path().join("egress.sock"))
        .args(["--listen", &listen, "--", "touch"])
        .arg(&marker)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(!marker.exists(), "the harness ran without a route out");
    assert!(stderr.contains(&listen), "{stderr}");
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

/// How many times `stderr` says that a process died of `signal`, numbered `number`.
fn deaths_by(stderr: &str, signal: &str, number: i32) -> usize {
    stderr
        .lines()
        .filter(|l| l.contains(&format!("killed by {signal} (signal {number})")))
        .count()
}

#[test]
fn an_unconfined_harness_killed_by_a_signal_exits_128_and_its_number_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let root = init_root(dir.path());
    let bins = fake_harness(dir.path(), "kill -ABRT $$");
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();

    let out = launch(&root, &bins, &work, &["--unconfined"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(128 + 6), "{stderr}");
    assert_eq!(deaths_by(&stderr, "SIGABRT", 6), 1, "{stderr}");
}

#[test]
fn the_relay_reports_a_harness_killed_by_a_signal_as_128_and_its_number() {
    let dir = tempfile::tempdir().unwrap();
    let out = memory()
        .args(["relay", "--socket"])
        .arg(dir.path().join("egress.sock"))
        .args(["--listen", "127.0.0.1:0", "--", "sh", "-c", "kill -KILL $$"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(128 + 9), "{stderr}");
    assert_eq!(deaths_by(&stderr, "SIGKILL", 9), 1, "{stderr}");
}

#[cfg(target_os = "linux")]
#[test]
fn a_confined_harness_killed_by_a_signal_exits_128_and_its_number_and_says_so_once() {
    if !namespaces_available() {
        eprintln!("skipping: unprivileged namespaces are not available here");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = init_root(dir.path());
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let bins = fake_harness(&work, "kill -ABRT $$");

    let out = launch(&root, &bins, &work, &[]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(128 + 6), "{stderr}");
    assert_eq!(deaths_by(&stderr, "SIGABRT", 6), 1, "{stderr}");
}

#[test]
fn a_harness_ended_by_sigint_or_sigpipe_exits_128_and_its_number_quietly() {
    for (signal, number) in [("INT", 2), ("PIPE", 13)] {
        let dir = tempfile::tempdir().unwrap();
        let out = memory()
            .args(["relay", "--socket"])
            .arg(dir.path().join("egress.sock"))
            .args(["--listen", "127.0.0.1:0", "--", "sh", "-c"])
            .arg(format!("kill -{signal} $$"))
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(
            out.status.code(),
            Some(128 + number),
            "SIG{signal}: {stderr}"
        );
        assert!(!stderr.contains("killed by"), "SIG{signal}: {stderr}");
    }
}

/// The pid of the relay whose harness command line names `harness`, from `/proc`.
#[cfg(target_os = "linux")]
fn relay_of(harness: &Path) -> Option<String> {
    let harness = harness.display().to_string();
    std::fs::read_dir("/proc").ok()?.flatten().find_map(|e| {
        let pid = e.file_name().to_string_lossy().into_owned();
        let cmdline = std::fs::read(e.path().join("cmdline")).ok()?;
        let args: Vec<String> = cmdline
            .split(|b| *b == 0)
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect();
        (args.get(1).is_some_and(|a| a == "relay") && args.iter().any(|a| a.contains(&harness)))
            .then_some(pid)
    })
}

#[cfg(target_os = "linux")]
#[test]
fn a_confined_relay_killed_by_a_signal_exits_128_and_its_number_and_says_so_once() {
    if !namespaces_available() {
        eprintln!("skipping: unprivileged namespaces are not available here");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = init_root(dir.path());
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let bins = fake_harness(&work, "touch ./started; exec sleep 60");

    let mut cmd = memory();
    cmd.args(["--root", root.to_str().unwrap(), "launch"])
        .args(["--harness", "claude", "--auth", "key"])
        .args(["--workdir", work.to_str().unwrap(), "--", "-p", "hello"])
        .env("PATH", path_with(&bins))
        .env("ANTHROPIC_API_KEY", "sk-ant-placeholder")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = cmd.spawn().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !work.join("started").exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let relay = relay_of(&bins).expect("the relay is running");
    let killed = Command::new("kill")
        .args(["-KILL", &relay])
        .status()
        .unwrap();
    assert!(killed.success());

    let out = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(128 + 9), "{stderr}");
    let said = stderr
        .lines()
        .filter(|l| l.contains("the relay was killed by SIGKILL (signal 9)"))
        .count();
    assert_eq!(said, 1, "{stderr}");
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

/// A confined prompted run captures the harness's standard output through a pipe, which
/// holds 64 KiB. A harness that writes more than that finishes, and all of what it wrote
/// reaches the launcher's own output. Bounded in time: a launch that never returns is
/// this test failing, and its process group is killed so nothing outlives the test.
#[cfg(target_os = "linux")]
#[test]
fn a_confined_prompted_run_captures_more_than_a_pipe_holds() {
    use std::os::unix::process::CommandExt;
    use std::time::{Duration, Instant};
    if !namespaces_available() {
        eprintln!("skipping: unprivileged namespaces are not available here");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let _reap = Reap(dir.path().to_path_buf());
    let root = init_root(dir.path());
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let bins = fake_harness(
        &work,
        "printf 'begin\\n'; head -c 200000 /dev/zero | tr '\\0' a; printf '\\nend\\n'; exit 0",
    );
    let stdout = dir.path().join("stdout");
    let stderr = dir.path().join("stderr");
    let mut cmd = memory();
    cmd.args([
        "--root",
        root.to_str().unwrap(),
        "launch",
        "--harness",
        "claude",
        "--auth",
        "key",
    ])
    .args(["--workdir", work.to_str().unwrap()])
    .args(["--prompt", "hello", "--no-handoff"])
    .env("PATH", path_with(&bins))
    .env("ANTHROPIC_API_KEY", "sk-ant-placeholder")
    .stdin(Stdio::null())
    .stdout(std::fs::File::create(&stdout).unwrap())
    .stderr(std::fs::File::create(&stderr).unwrap())
    .process_group(0);
    let mut child = cmd.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(90);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if Instant::now() > deadline {
            Command::new("kill")
                .args(["-KILL", "--", &format!("-{}", child.id())])
                .status()
                .ok();
            child.wait().ok();
            panic!(
                "the launch had not returned after 90 s, with {} bytes out: {}",
                std::fs::metadata(&stdout).map(|m| m.len()).unwrap_or(0),
                std::fs::read_to_string(&stderr).unwrap_or_default()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let err = std::fs::read_to_string(&stderr).unwrap();
    assert_eq!(status.code(), Some(0), "{err}");
    assert!(err.contains("confined to"), "{err}");
    let out = std::fs::read_to_string(&stdout).unwrap();
    let want = format!("begin\n{}\nend\n", "a".repeat(200_000));
    assert!(
        out.contains(&want),
        "the harness's output, whole: {} bytes out",
        out.len()
    );
}

// ---- a launcher told to stop, and one killed ----

/// A confined prompted launch, with its temporary
/// directory at `tmp`, so the scratch directory it makes is where the test looks. Its
/// output goes to `<work>.out` and `<work>.err`.
#[cfg(target_os = "linux")]
fn start_confined(root: &Path, work: &Path, tmp: &Path) -> std::process::Child {
    start_amem(
        root,
        work,
        tmp,
        &["launch", "--prompt", "hello", "--no-handoff"],
    )
}

/// `amem <args>` for the claude harness on a key, in `work`, as [`start_confined`] runs it.
#[cfg(target_os = "linux")]
fn start_amem(root: &Path, work: &Path, tmp: &Path, args: &[&str]) -> std::process::Child {
    amem_in(root, work, tmp, args)
        .stdout(std::fs::File::create(work.with_extension("out")).unwrap())
        .spawn()
        .unwrap()
}

/// As [`start_amem`], with standard output a pipe whose reader is already gone: a
/// launcher piped into a program that has exited.
#[cfg(target_os = "linux")]
fn start_amem_into_a_closed_pipe(
    root: &Path,
    work: &Path,
    tmp: &Path,
    args: &[&str],
) -> std::process::Child {
    let mut child = amem_in(root, work, tmp, args)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    child
}

/// The command [`start_amem`] runs, its standard output not yet set.
#[cfg(target_os = "linux")]
fn amem_in(root: &Path, work: &Path, tmp: &Path, args: &[&str]) -> Command {
    let mut cmd = memory();
    cmd.args(["--root", root.to_str().unwrap()])
        .args(args)
        .args(["--harness", "claude", "--auth", "key"])
        .args(["--workdir", work.to_str().unwrap()]);
    cmd.env("PATH", path_with(&work.join("bins")))
        .env("ANTHROPIC_API_KEY", "sk-ant-placeholder")
        .env("TMPDIR", tmp)
        .stdin(Stdio::null())
        .stderr(std::fs::File::create(work.with_extension("err")).unwrap());
    cmd
}

/// A working tree whose harness says it started, then runs until it is killed.
#[cfg(target_os = "linux")]
fn sleeper(dir: &Path, name: &str) -> PathBuf {
    let work = dir.join(name);
    std::fs::create_dir_all(&work).unwrap();
    fake_harness(&work, "touch ./started\nsleep 300");
    work
}

/// A launch whose harness has started and runs until it is killed, and its working tree.
#[cfg(target_os = "linux")]
fn running_sleeper(
    root: &Path,
    dir: &Path,
    name: &str,
    tmp: &Path,
) -> (std::process::Child, PathBuf) {
    let work = sleeper(dir, name);
    let launch = start_confined(root, &work, tmp);
    assert!(
        started(&work, 60),
        "{}",
        std::fs::read_to_string(work.with_extension("err")).unwrap()
    );
    (launch, work)
}

/// Send `child` the signal named `sig`, as `kill` names it.
#[cfg(target_os = "linux")]
fn signal(child: &std::process::Child, sig: &str) {
    let ok = Command::new("kill")
        .args([&format!("-{sig}"), &child.id().to_string()])
        .status()
        .unwrap();
    assert!(ok.success());
}

/// Wait for `child` to exit, `secs` at most.
#[cfg(target_os = "linux")]
fn exited(child: &mut std::process::Child, secs: u64) -> Option<std::process::ExitStatus> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    while std::time::Instant::now() < deadline {
        if let Some(s) = child.try_wait().unwrap() {
            return Some(s);
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    None
}

/// Wait for the harness in `work` to have started, `secs` at most.
#[cfg(target_os = "linux")]
fn started(work: &Path, secs: u64) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    while std::time::Instant::now() < deadline {
        if work.join("started").exists() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    false
}

/// The processes whose command line names something under `dir`, as pid and command
/// line.
#[cfg(target_os = "linux")]
fn processes_under(dir: &Path) -> Vec<(String, String)> {
    let needle = dir.to_string_lossy().into_owned();
    std::fs::read_dir("/proc")
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|pid| pid.bytes().all(|b| b.is_ascii_digit()))
        .filter_map(|pid| {
            let c = std::fs::read(Path::new("/proc").join(&pid).join("cmdline")).ok()?;
            Some((pid, String::from_utf8_lossy(&c).replace('\0', " ")))
        })
        .filter(|(_, c)| c.contains(&needle))
        .collect()
}

/// Whether no process names anything under `dir` within `secs`.
#[cfg(target_os = "linux")]
fn gone_within(dir: &Path, secs: u64) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    while std::time::Instant::now() < deadline {
        if processes_under(dir).is_empty() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    false
}

/// The mounts under `dir`, from this process's mount table: no mountpoint is touched,
/// since one a launcher left may not answer.
#[cfg(target_os = "linux")]
fn mounts_under(dir: &Path) -> Vec<String> {
    let needle = format!("{}/", dir.display());
    std::fs::read_to_string("/proc/self/mountinfo")
        .unwrap()
        .lines()
        .filter_map(|l| l.split(' ').nth(4))
        .filter(|m| m.starts_with(&needle))
        .map(str::to_string)
        .collect()
}

/// The names in `dir`, listed without a stat of any of them.
#[cfg(target_os = "linux")]
fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|d| {
            d.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// Kills, when dropped, every process whose command line names something under its
/// directory, and lazily unmounts what is left there: a test that fails leaves nothing
/// running or mounted. A run's harness goes with its relay, pid 1 of its namespace.
#[cfg(target_os = "linux")]
struct Reap(PathBuf);

#[cfg(target_os = "linux")]
impl Drop for Reap {
    fn drop(&mut self) {
        for (pid, _) in processes_under(&self.0) {
            Command::new("kill")
                .args(["-KILL", &pid])
                .stderr(Stdio::null())
                .status()
                .ok();
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
        for m in mounts_under(&self.0) {
            Command::new("fusermount3").args(["-uz", &m]).status().ok();
        }
    }
}

/// What a launch leaves: the processes of its run, its mount, its scratch directory,
/// its socket and its render. A SIGTERM ends the run and removes every one, as a run
/// that ends of itself does, and the launcher exits as a process the signal ended.
#[cfg(target_os = "linux")]
#[test]
fn a_launcher_told_to_stop_ends_its_run_and_leaves_nothing() {
    if !namespaces_available() {
        eprintln!("skipping: unprivileged namespaces are not available here");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = init_root(dir.path());
    let tmp = dir.path().join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let _reap = Reap(dir.path().to_path_buf());
    let (mut launch, work) = running_sleeper(&root, dir.path(), "work", &tmp);
    assert!(
        !mounts_under(&root).is_empty(),
        "the projection is a live mount"
    );
    assert_eq!(names_in(&tmp).len(), 1, "the run has its scratch directory");

    signal(&launch, "TERM");
    let status = exited(&mut launch, 30);

    let err = std::fs::read_to_string(work.with_extension("err")).unwrap();
    assert_eq!(status.and_then(|s| s.code()), Some(128 + 15), "{err}");
    // said once, as the stop; the kill that ends the sandbox is not a death to report
    assert_eq!(err.matches("SIGTERM").count(), 1, "{err}");
    assert!(!err.contains("killed by"), "{err}");
    assert_eq!(processes_under(dir.path()), Vec::new());
    assert_eq!(mounts_under(&root), Vec::<String>::new());
    assert_eq!(
        names_in(&tmp),
        Vec::<String>::new(),
        "the scratch directory"
    );
    assert_eq!(names_in(&root.join("state/run")), Vec::<String>::new());
    assert_eq!(
        names_in(&root.join("state/mount/claude")),
        Vec::<String>::new()
    );
    assert_eq!(
        names_in(&root.join("state/projection/claude")),
        Vec::<String>::new()
    );
}

/// SIGKILL `killed`, a running launch in `work`, and check what it leaves: no process of
/// its run, which dies with it, and its scratch directory, socket and mount.
#[cfg(target_os = "linux")]
fn killed_by_sigkill(root: &Path, tmp: &Path, killed: &mut std::process::Child, work: &Path) {
    signal(killed, "KILL");
    killed.wait().unwrap();
    // the sandbox dies with its launcher: the intermediate, the relay, the harness
    assert!(gone_within(work, 5), "{:?}", processes_under(work));
    let killed_socket = format!("egress-{}.sock", killed.id());
    assert!(names_in(&root.join("state/run")).contains(&killed_socket));

    let killed_scratch = format!("memory-sandbox-{}", killed.id());
    assert!(names_in(tmp).contains(&killed_scratch));
    assert_eq!(
        mounts_under(root).len(),
        2,
        "the killed launch's mount is left"
    );
}

/// SIGKILL cannot be caught: a launcher killed by it leaves its scratch directory and its
/// mount, with nothing running to serve it. The next launch sweeps both, and what a
/// killed launcher of long ago left, and keeps what a live launcher holds.
#[cfg(target_os = "linux")]
#[test]
fn the_next_launch_sweeps_what_a_killed_one_left_and_keeps_a_live_one_s() {
    if !namespaces_available() {
        eprintln!("skipping: unprivileged namespaces are not available here");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = init_root(dir.path());
    let tmp = dir.path().join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    // left by a launcher long gone: its pid is no process now
    let mut gone = Command::new("/bin/sh").args(["-c", ":"]).spawn().unwrap();
    let gone_pid = gone.id();
    gone.wait().unwrap();
    std::fs::create_dir(tmp.join(format!("memory-sandbox-{gone_pid}"))).unwrap();
    let _reap = Reap(dir.path().to_path_buf());
    let (mut live, _) = running_sleeper(&root, dir.path(), "live", &tmp);
    let live_mounts = mounts_under(&root);
    assert_eq!(live_mounts.len(), 1);
    let (mut killed, killed_work) = running_sleeper(&root, dir.path(), "killed", &tmp);
    assert_eq!(mounts_under(&root).len(), 2);
    killed_by_sigkill(&root, &tmp, &mut killed, &killed_work);

    let next_work = dir.path().join("next");
    std::fs::create_dir_all(&next_work).unwrap();
    fake_harness(&next_work, "exit 0");
    let mut next = start_confined(&root, &next_work, &tmp);
    let status = exited(&mut next, 60);
    let err = std::fs::read_to_string(next_work.with_extension("err")).unwrap();
    assert_eq!(status.and_then(|s| s.code()), Some(0), "{err}");

    assert_eq!(
        names_in(&tmp),
        vec![format!("memory-sandbox-{}", live.id())],
        "the killed launch's scratch directory and the old one are swept"
    );
    assert_eq!(
        mounts_under(&root),
        live_mounts,
        "the killed launch's mount is swept and the live launch's stays"
    );
    let live_session = Path::new(&live_mounts[0]).file_name().unwrap();
    let live_session = live_session.to_string_lossy().into_owned();
    assert_eq!(
        names_in(&root.join("state/mount/claude")),
        vec![live_session.clone(), format!("{live_session}.lock")],
        "the live launch's mountpoint and its record"
    );
    assert_eq!(
        names_in(&root.join("state/run")),
        vec![format!("egress-{}.sock", live.id())],
        "the killed launch's socket is swept and the live launch's stays"
    );
    assert!(
        live.try_wait().unwrap().is_none(),
        "the live launch runs on"
    );
    // and it still ends cleanly
    signal(&live, "TERM");
    assert_eq!(exited(&mut live, 30).and_then(|s| s.code()), Some(128 + 15));
    assert_eq!(mounts_under(&root), Vec::<String>::new());
    assert_eq!(names_in(&tmp), Vec::<String>::new());
}

/// A terminal that hangs up sends SIGHUP to its session's leader, and refuses every write
/// after it. The launcher still ends the run and cleans up: no panic on a write to the
/// terminal that is gone stops it first.
#[cfg(target_os = "linux")]
#[test]
fn a_launcher_whose_terminal_hangs_up_still_cleans_up() {
    if !namespaces_available() {
        eprintln!("skipping: unprivileged namespaces are not available here");
        return;
    }
    if !script_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = init_root(dir.path());
    let tmp = dir.path().join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let work = sleeper(dir.path(), "work");
    let _reap = Reap(dir.path().to_path_buf());
    // the launcher as the session leader of a terminal of its own, which script makes
    let line = format!(
        "exec {} --root {} launch --prompt hello --no-handoff --harness claude --auth key --workdir {}",
        env!("CARGO_BIN_EXE_amem"),
        root.display(),
        work.display()
    );
    let mut terminal = Command::new("script")
        .args(["-qfec", &line, "/dev/null"])
        .env("PATH", path_with(&work.join("bins")))
        .env("ANTHROPIC_API_KEY", "sk-ant-placeholder")
        .env("TMPDIR", &tmp)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    assert!(started(&work, 60));
    assert!(!mounts_under(&root).is_empty());

    // the terminal goes: its other end closes, and the kernel hangs it up
    signal(&terminal, "KILL");
    terminal.wait().unwrap();

    assert!(
        gone_within(dir.path(), 30),
        "{:?}",
        processes_under(dir.path())
    );
    assert_eq!(mounts_under(&root), Vec::<String>::new());
    assert_eq!(
        names_in(&tmp),
        Vec::<String>::new(),
        "the scratch directory"
    );
    assert_eq!(
        names_in(&root.join("state/run")),
        Vec::<String>::new(),
        "the socket"
    );
    assert_eq!(
        names_in(&root.join("state/mount/claude")),
        Vec::<String>::new()
    );
    assert_eq!(
        names_in(&root.join("state/projection/claude")),
        Vec::<String>::new(),
        "the render"
    );
}

/// An unconfined run is told to stop as a confined one is: at once, with 143.
#[cfg(target_os = "linux")]
#[test]
fn an_unconfined_launcher_told_to_stop_ends_its_run_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let root = init_root(dir.path());
    let tmp = dir.path().join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    // exec, so the process the launcher spawned is the one that sleeps
    fake_harness(&work, "touch ./started\nexec sleep 300");
    let _reap = Reap(dir.path().to_path_buf());
    let mut launch = start_amem(
        &root,
        &work,
        &tmp,
        &[
            "launch",
            "--unconfined",
            "--prompt",
            "hello",
            "--no-handoff",
        ],
    );
    assert!(
        started(&work, 60),
        "{}",
        std::fs::read_to_string(work.with_extension("err")).unwrap()
    );
    signal(&launch, "TERM");
    let status = exited(&mut launch, 10);
    let err = std::fs::read_to_string(work.with_extension("err")).unwrap();
    assert_eq!(status.and_then(|s| s.code()), Some(128 + 15), "{err}");
    assert_eq!(names_in(&root.join("state/run")), Vec::<String>::new());
}

/// A launcher that is stopped (Ctrl-Z) does not answer for its mount. Another launch
/// does not ask it anything: it starts, runs and ends, and the stopped launcher's mount
/// is kept for it.
#[cfg(target_os = "linux")]
#[test]
fn a_stopped_launcher_s_mount_is_kept_and_not_asked() {
    if !namespaces_available() {
        eprintln!("skipping: unprivileged namespaces are not available here");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = init_root(dir.path());
    let tmp = dir.path().join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let _reap = Reap(dir.path().to_path_buf());
    let (mut stopped, _) = running_sleeper(&root, dir.path(), "stopped", &tmp);
    let its_mounts = mounts_under(&root);
    assert_eq!(its_mounts.len(), 1);
    signal(&stopped, "STOP");

    let next_work = dir.path().join("next");
    std::fs::create_dir_all(&next_work).unwrap();
    fake_harness(&next_work, "exit 0");
    let mut next = start_confined(&root, &next_work, &tmp);
    let status = exited(&mut next, 60);
    signal(&stopped, "CONT");
    let err = std::fs::read_to_string(next_work.with_extension("err")).unwrap();
    assert_eq!(status.and_then(|s| s.code()), Some(0), "{err}");
    assert_eq!(
        mounts_under(&root),
        its_mounts,
        "the stopped launcher's mount stays"
    );

    signal(&stopped, "TERM");
    assert_eq!(
        exited(&mut stopped, 30).and_then(|s| s.code()),
        Some(128 + 15)
    );
    assert_eq!(mounts_under(&root), Vec::<String>::new());
}

/// `amem work --loop` told to stop mid-run settles the task it holds, as when the harness
/// stops of itself: back in the pool with a handoff. It claims nothing more.
#[cfg(target_os = "linux")]
#[test]
fn a_worker_told_to_stop_releases_its_task_with_a_handoff_and_claims_no_other() {
    if !namespaces_available() {
        eprintln!("skipping: unprivileged namespaces are not available here");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = init_root(dir.path());
    let tmp = dir.path().join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let ids = [file_task(&root, "First"), file_task(&root, "Second")];
    let work = sleeper(dir.path(), "work");
    let _reap = Reap(dir.path().to_path_buf());
    let mut worker = start_amem(
        &root,
        &work,
        &tmp,
        &["work", "--loop", "--lease-seconds", "60"],
    );
    assert!(
        started(&work, 60),
        "{}",
        std::fs::read_to_string(work.with_extension("err")).unwrap()
    );
    signal(&worker, "TERM");
    let status = exited(&mut worker, 30);
    let err = std::fs::read_to_string(work.with_extension("err")).unwrap();
    assert_eq!(status.and_then(|s| s.code()), Some(128 + 15), "{err}");

    let claimed: Vec<&String> = ids
        .iter()
        .filter(|id| err.contains(&format!("claimed {id}")))
        .collect();
    assert_eq!(claimed.len(), 1, "one task claimed: {err}");
    for id in &ids {
        let t = cli(&root, &["task", "get", id]);
        assert_eq!(t["status"], "admitted", "in the pool: {t}");
        if claimed.contains(&id) {
            assert!(t["handoff"].as_str().is_some(), "with a handoff: {t}");
        } else {
            assert!(t["handoff"].is_null(), "never claimed: {t}");
        }
    }
}

/// A worker in `dir/work` whose harness prints, says it started, then runs until killed;
/// one task filed for it.
#[cfg(target_os = "linux")]
fn a_task_and_a_printing_sleeper(root: &Path, dir: &Path) -> (String, PathBuf) {
    let id = file_task(root, "First");
    let work = dir.join("work");
    std::fs::create_dir_all(&work).unwrap();
    fake_harness(&work, "echo working\ntouch ./started\nsleep 300");
    (id, work)
}

/// Whether task `id` is back in the pool, and with a handoff.
#[cfg(target_os = "linux")]
fn released(root: &Path, id: &str) -> (bool, bool) {
    let t = cli(root, &["task", "get", id]);
    (t["status"] == "admitted", t["handoff"].as_str().is_some())
}

/// A launcher piped into a reader that has gone: the answer it prints after a stop
/// goes nowhere, and the worker settles its task and the launch finishes all the same.
#[cfg(target_os = "linux")]
#[test]
fn a_launcher_whose_output_reader_has_gone_still_settles_and_cleans_up() {
    if !namespaces_available() {
        eprintln!("skipping: unprivileged namespaces are not available here");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = init_root(dir.path());
    let tmp = dir.path().join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let (id, work) = a_task_and_a_printing_sleeper(&root, dir.path());
    let _reap = Reap(dir.path().to_path_buf());
    for args in [
        &["work", "--loop", "--lease-seconds", "60"][..],
        &["launch", "--prompt", "hello", "--no-handoff"][..],
    ] {
        std::fs::remove_file(work.join("started")).ok();
        let mut launch = start_amem_into_a_closed_pipe(&root, &work, &tmp, args);
        assert!(started(&work, 60), "{args:?}");
        signal(&launch, "TERM");
        let status = exited(&mut launch, 30);
        let err = std::fs::read_to_string(work.with_extension("err")).unwrap();
        assert_eq!(
            status.and_then(|s| s.code()),
            Some(128 + 15),
            "{args:?}: {err}"
        );
        assert_eq!(
            names_in(&root.join("state/run")),
            Vec::<String>::new(),
            "{err}"
        );
        assert_eq!(
            names_in(&root.join("state/projection/claude")),
            Vec::<String>::new()
        );
    }
    assert_eq!(released(&root, &id), (true, true), "back with a handoff");
}

/// A launcher started to ignore SIGHUP, under `nohup` or by a shell without job control,
/// goes on ignoring it: the run is not stopped.
#[cfg(target_os = "linux")]
#[test]
fn a_launcher_started_to_ignore_sighup_is_not_stopped_by_it() {
    if !namespaces_available() {
        eprintln!("skipping: unprivileged namespaces are not available here");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = init_root(dir.path());
    let tmp = dir.path().join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let work = sleeper(dir.path(), "work");
    let _reap = Reap(dir.path().to_path_buf());
    let line = format!(
        "trap '' HUP; exec {} --root {} launch --prompt hello --no-handoff --harness claude --auth key --workdir {}",
        env!("CARGO_BIN_EXE_amem"),
        root.display(),
        work.display()
    );
    let mut launch = Command::new("/bin/sh")
        .args(["-c", &line])
        .env("PATH", path_with(&work.join("bins")))
        .env("ANTHROPIC_API_KEY", "sk-ant-placeholder")
        .env("TMPDIR", &tmp)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(work.with_extension("err")).unwrap())
        .spawn()
        .unwrap();
    assert!(started(&work, 60));

    signal(&launch, "HUP");
    std::thread::sleep(std::time::Duration::from_secs(1));

    let err = std::fs::read_to_string(work.with_extension("err")).unwrap();
    assert!(
        launch.try_wait().unwrap().is_none(),
        "the run goes on: {err}"
    );
    assert_eq!(mounts_under(&root).len(), 1);
    signal(&launch, "TERM");
    assert_eq!(
        exited(&mut launch, 30).and_then(|s| s.code()),
        Some(128 + 15)
    );
}

/// A headless unconfined run that left a child in the background, holding its output
/// open: a stop kills the child too, and the worker settles its task and exits.
#[cfg(target_os = "linux")]
#[test]
fn an_unconfined_worker_told_to_stop_ends_what_its_harness_left_running() {
    let dir = tempfile::tempdir().unwrap();
    let root = init_root(dir.path());
    let tmp = dir.path().join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let id = file_task(&root, "First");
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    fake_harness(
        &work,
        "touch ./started\nsh -c \"sleep 300; echo $PWD\" &\nwait",
    );
    let _reap = Reap(dir.path().to_path_buf());
    let mut worker = start_amem(
        &root,
        &work,
        &tmp,
        &["work", "--unconfined", "--loop", "--lease-seconds", "60"],
    );
    assert!(
        started(&work, 60),
        "{}",
        std::fs::read_to_string(work.with_extension("err")).unwrap()
    );
    signal(&worker, "TERM");
    let status = exited(&mut worker, 15);
    let err = std::fs::read_to_string(work.with_extension("err")).unwrap();
    assert_eq!(status.and_then(|s| s.code()), Some(128 + 15), "{err}");
    assert!(gone_within(&work, 5), "{:?}", processes_under(&work));
    assert_eq!(released(&root, &id), (true, true), "back with a handoff");
}

/// Whether script(1) is here to make a terminal with; on CI it must be.
#[cfg(target_os = "linux")]
fn script_available() -> bool {
    let here = Command::new("script").arg("--version").output().is_ok();
    assert!(
        here || std::env::var_os("CI").is_none(),
        "util-linux script makes the terminal here, and on CI it must be there"
    );
    if !here {
        eprintln!("skipping: no script(1) to make a terminal with");
    }
    here
}

/// `amem <args>` for the claude harness in `work`, as the session leader of a terminal
/// of its own that script(1) makes, its standard error to `<work>.err`. script exits
/// with the launcher's code, and keeps what the terminal showed in `<work>.tty`.
#[cfg(target_os = "linux")]
fn on_a_terminal(root: &Path, work: &Path, tmp: &Path, args: &str) -> std::process::Child {
    let line = format!(
        "exec {} --root {} {args} --harness claude --auth key --workdir {} 2>{}",
        env!("CARGO_BIN_EXE_amem"),
        root.display(),
        work.display(),
        work.with_extension("err").display()
    );
    Command::new("script")
        .args(["-qfec", &line])
        .arg(work.with_extension("tty"))
        .env("PATH", path_with(&work.join("bins")))
        .env("ANTHROPIC_API_KEY", "sk-ant-placeholder")
        .env("TMPDIR", tmp)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

/// The pid of the launcher running on `root`.
#[cfg(target_os = "linux")]
fn launcher_of(root: &Path) -> String {
    let bin = env!("CARGO_BIN_EXE_amem");
    let found: Vec<String> = processes_under(root)
        .into_iter()
        .filter(|(_, c)| c.starts_with(bin) && c.contains(" --root "))
        .map(|(pid, _)| pid)
        .collect();
    assert_eq!(found.len(), 1, "{found:?}");
    found[0].clone()
}

/// A headless unconfined run on a terminal stays in the terminal's foreground group: a
/// harness that sets the terminal, as a password prompt does, is not stopped for it.
#[cfg(target_os = "linux")]
#[test]
fn a_headless_unconfined_run_on_a_terminal_may_use_the_terminal() {
    if !script_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = init_root(dir.path());
    let tmp = dir.path().join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    fake_harness(
        &work,
        "stty -echo </dev/tty; stty echo </dev/tty; echo answered",
    );
    let _reap = Reap(dir.path().to_path_buf());
    let mut terminal = on_a_terminal(
        &root,
        &work,
        &tmp,
        "launch --unconfined --prompt hello --no-handoff",
    );
    let status = exited(&mut terminal, 30);
    let err = std::fs::read_to_string(work.with_extension("err")).unwrap_or_default();
    assert_eq!(status.and_then(|s| s.code()), Some(0), "{err}");
    let shown = std::fs::read_to_string(work.with_extension("tty")).unwrap_or_default();
    assert!(shown.contains("answered"), "{shown}");
}

/// A worker on a terminal whose harness left a child in the background, holding the
/// run's output open: its task and a launcher in `dir/work`.
#[cfg(target_os = "linux")]
fn a_worker_on_a_terminal_with_a_background_child(
    root: &Path,
    dir: &Path,
    tmp: &Path,
) -> (String, PathBuf, std::process::Child) {
    let id = file_task(root, "First");
    let work = dir.join("work");
    std::fs::create_dir_all(&work).unwrap();
    fake_harness(
        &work,
        "touch ./started\nsh -c \"sleep 300; echo $PWD\" &\nwait",
    );
    let terminal = on_a_terminal(
        root,
        &work,
        tmp,
        "work --unconfined --loop --lease-seconds 60",
    );
    assert!(
        started(&work, 60),
        "{}",
        std::fs::read_to_string(work.with_extension("err")).unwrap_or_default()
    );
    (id, work, terminal)
}

/// On a terminal the harness is in the terminal's group, which a stop does not kill
/// whole; what it left in the background, holding the output open, does not keep the
/// worker from settling its task and exiting.
#[cfg(target_os = "linux")]
#[test]
fn a_worker_on_a_terminal_told_to_stop_ends_though_its_harness_left_a_child() {
    if !script_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = init_root(dir.path());
    let tmp = dir.path().join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let _reap = Reap(dir.path().to_path_buf());
    let (id, work, mut terminal) =
        a_worker_on_a_terminal_with_a_background_child(&root, dir.path(), &tmp);
    let launcher = launcher_of(&root);
    Command::new("kill")
        .args(["-TERM", &launcher])
        .status()
        .unwrap();
    let status = exited(&mut terminal, 15);
    let err = std::fs::read_to_string(work.with_extension("err")).unwrap_or_default();
    assert_eq!(status.and_then(|s| s.code()), Some(128 + 15), "{err}");
    assert_eq!(released(&root, &id), (true, true), "back with a handoff");
}

/// A headless unconfined harness that exits at once, leaving a child that holds its
/// output and ignores it: the run is still in flight while its output is read, so a stop
/// then kills the child with the harness's process group.
#[cfg(target_os = "linux")]
#[test]
fn a_stop_kills_what_an_exited_harness_left_holding_its_output() {
    let dir = tempfile::tempdir().unwrap();
    let root = init_root(dir.path());
    let tmp = dir.path().join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let id = file_task(&root, "First");
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    fake_harness(
        &work,
        "sh -c \"sleep 300; echo $PWD\" &\ntouch ./started\nexit 0",
    );
    let _reap = Reap(dir.path().to_path_buf());
    let mut worker = start_amem(
        &root,
        &work,
        &tmp,
        &["work", "--unconfined", "--loop", "--lease-seconds", "60"],
    );
    assert!(started(&work, 60));
    // the harness has gone; its child holds the output
    std::thread::sleep(std::time::Duration::from_millis(300));
    let child = |dir: &Path| {
        processes_under(dir)
            .into_iter()
            .any(|(_, c)| c.contains("sleep 300; echo"))
    };
    assert!(child(&work), "{:?}", processes_under(&work));
    signal(&worker, "TERM");
    let status = exited(&mut worker, 15);
    let err = std::fs::read_to_string(work.with_extension("err")).unwrap();
    assert_eq!(status.and_then(|s| s.code()), Some(128 + 15), "{err}");
    assert!(gone_within(&work, 5), "{:?}", processes_under(&work));
    assert_eq!(released(&root, &id), (true, true), "back with a handoff");
}

/// Told twice to stop, a worker does not wait for the run's cleanup, but it leaves
/// nothing no sweep removes: its task back in the pool, its render and its socket gone.
/// The run's output held open by what its harness left in the background holds the
/// cleanup open, so the second signal comes before the task is settled: the task comes
/// back with no handoff, which only this path does.
#[cfg(target_os = "linux")]
#[test]
fn a_worker_told_twice_to_stop_releases_its_task_and_removes_its_render() {
    if !script_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = init_root(dir.path());
    let tmp = dir.path().join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let _reap = Reap(dir.path().to_path_buf());
    let (id, work, mut terminal) =
        a_worker_on_a_terminal_with_a_background_child(&root, dir.path(), &tmp);
    use nix::sys::signal::{kill, Signal};
    let launcher = nix::unistd::Pid::from_raw(launcher_of(&root).parse().unwrap());
    kill(launcher, Signal::SIGINT).unwrap();
    // the second from the loop that sees the first in progress, with no process to
    // start between: the run's output, held open by what the harness left, keeps the
    // cleanup waiting for the read's grace, seconds, which this comes well within
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let read_err = || std::fs::read_to_string(work.with_extension("err")).unwrap_or_default();
    while !read_err().contains("ending the run and cleaning up") {
        assert!(std::time::Instant::now() < deadline, "{}", read_err());
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    kill(launcher, Signal::SIGINT).unwrap();
    let status = exited(&mut terminal, 30);
    let err = read_err();
    assert_eq!(status.and_then(|s| s.code()), Some(128 + 2), "{err}");
    assert_eq!(
        released(&root, &id),
        (true, false),
        "back, with no handoff: {err}"
    );
    assert_eq!(
        names_in(&root.join("state/projection/claude")),
        Vec::<String>::new(),
        "the render"
    );
    assert_eq!(
        names_in(&root.join("state/run")),
        Vec::<String>::new(),
        "the socket"
    );
}

/// Under umask 000 the launcher's own scratch directory is still its own: a confined run
/// is not refused for it.
#[cfg(target_os = "linux")]
#[test]
fn a_confined_run_under_umask_000_runs() {
    if !namespaces_available() {
        eprintln!("skipping: unprivileged namespaces are not available here");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = init_root(dir.path());
    let tmp = dir.path().join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    fake_harness(&work, "exit 0");
    let _reap = Reap(dir.path().to_path_buf());
    let line = format!(
        "umask 000; exec {} --root {} launch --prompt hello --no-handoff --harness claude --auth key --workdir {}",
        env!("CARGO_BIN_EXE_amem"),
        root.display(),
        work.display()
    );
    let out = Command::new("/bin/sh")
        .args(["-c", &line])
        .env("PATH", path_with(&work.join("bins")))
        .env("ANTHROPIC_API_KEY", "sk-ant-placeholder")
        .env("TMPDIR", &tmp)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{err}");
    assert!(err.contains("confined to"), "{err}");
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

    let child = inside(&root, &requests, &dir.path().join("host-home"));

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
    assert!(
        !requests.join("egress.sock").exists(),
        "the session takes its socket away with it"
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
    cmd.env("MEMORY_BIN", env!("CARGO_BIN_EXE_amem"));
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

// ---- mining through the harness itself, as a subscription login requires ----

/// A fake harness: a normal prompt gets a normal answer; a mining run, recognisable by the
/// model flag, gets a JSON array of proposals, as a model would print it.
const MINES_WHEN_ASKED: &str = r#"
case "$*" in
  *--model*) printf '%s\n' '[{"kind":"fact","title":"The import has five steps","body":"Steps one to five; the fourth, digests, is the slow one and failed before."}]' ;;
  *) echo "the answer" ;;
esac
"#;

#[test]
fn mining_can_go_through_the_harness_on_a_small_model() {
    let dir = tempfile::tempdir().unwrap();
    let root = init_root(dir.path());
    let bins = fake_harness(dir.path(), MINES_WHEN_ASKED);
    let work_dir = dir.path().join("work");
    std::fs::create_dir_all(&work_dir).unwrap();
    // a transcript for the mining run to read, where Claude Code would leave one
    let projects = root.join("state/sandbox/claude/.claude/projects/p");
    std::fs::create_dir_all(&projects).unwrap();
    std::fs::write(
        projects.join("s.jsonl"),
        r#"{"type":"user","message":{"role":"user","content":"Finish the import; step four is digests."}}"#,
    )
    .unwrap();
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
        "--no-handoff",
    ]);
    cmd.args([
        "--mine",
        "--mine-via",
        "harness",
        "--mine-model",
        "claude-haiku-4-5-20251001",
    ]);
    cmd.env("PATH", path_with(&bins));
    cmd.env("ANTHROPIC_API_KEY", "sk-ant-placeholder");
    cmd.stdin(Stdio::null());
    let out = cmd.output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    assert!(stderr.contains("mined fact"), "{stderr}");
    assert!(stderr.contains("mined 1 entries"), "{stderr}");
    let list = cli(&root, &["list", "--kind", "fact"]);
    let titles: Vec<String> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["title"].as_str().unwrap().to_string())
        .collect();
    assert!(
        titles.iter().any(|t| t == "The import has five steps"),
        "{titles:?}"
    );
    // mined first, then taken out of the home: the record holds it from here
    assert!(!projects.join("s.jsonl").exists(), "{stderr}");
    assert!(stderr.contains("removed; the record keeps it"), "{stderr}");
}

// ---- re-enactment: a prompted session run again against its tape ----

/// Shell that speaks MCP to the egress: T0, then a handoff carrying what the working
/// tree says.
const READS_THE_TREE: &str = r#"
base="${ANTHROPIC_BASE_URL%/anthropic}"
H1='content-type: application/json'; H2='accept: application/json, text/event-stream'
sid=$(curl -s -D - -o /dev/null -X POST "$base/mcp" -H "$H1" -H "$H2" -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"fake","version":"0"}}}' | tr -d '\r' | awk 'tolower($1)=="mcp-session-id:"{print $2}')
curl -s -o /dev/null -X POST "$base/mcp" -H "$H1" -H "$H2" -H "mcp-session-id: $sid" -d '{"jsonrpc":"2.0","method":"notifications/initialized"}'
curl -s -o /dev/null -X POST "$base/mcp" -H "$H1" -H "$H2" -H "mcp-session-id: $sid" -d '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"t0","arguments":{}}}'
steps=$(cat steps.txt)
curl -s -X POST "$base/mcp" -H "$H1" -H "$H2" -H "mcp-session-id: $sid" -d "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"write\",\"arguments\":{\"body\":\"The import has $steps.\",\"entry_type\":\"handoff\",\"title\":\"Import\"}}}" | grep -o 'mem-[a-z0-9]*-[a-z0-9]*' | head -1
"#;

/// `git` on the repository named, whatever a hook running the tests set for its own.
fn git_cmd() -> Command {
    let mut cmd = Command::new("git");
    for v in [
        "GIT_DIR",
        "GIT_INDEX_FILE",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_PREFIX",
    ] {
        cmd.env_remove(v);
    }
    cmd
}

fn git_in(dir: &Path, args: &[&str]) {
    let out = git_cmd()
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.org"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A git repository under `dir` whose one commit says the import has five steps; it and
/// its commit.
fn repo_of_five_steps(dir: &Path) -> (PathBuf, String) {
    let repo = dir.join("work");
    std::fs::create_dir_all(&repo).unwrap();
    git_in(&repo, &["init", "-q"]);
    std::fs::write(repo.join("steps.txt"), "five steps").unwrap();
    git_in(&repo, &["add", "steps.txt"]);
    git_in(&repo, &["commit", "-q", "-m", "five"]);
    let head = git_cmd()
        .args(["-C", repo.to_str().unwrap(), "rev-parse", "HEAD"])
        .output()
        .unwrap()
        .stdout;
    (repo, String::from_utf8(head).unwrap().trim().to_string())
}

/// `amem` on `root` with the fake harness first on the path and a placeholder key.
fn amem_with(root: &Path, bins: &Path, args: &[&str]) -> Output {
    let mut cmd = memory();
    cmd.args(["--root", root.to_str().unwrap()]).args(args);
    cmd.env("PATH", path_with(bins));
    cmd.env("ANTHROPIC_API_KEY", "sk-ant-placeholder");
    cmd.stdin(Stdio::null());
    cmd.output().unwrap()
}

/// A prompted launch in `repo` and the session its tape names, checked against `head`.
fn taped_session(root: &Path, bins: &Path, repo: &Path, head: &str) -> String {
    let out = amem_with(
        root,
        bins,
        &[
            "launch",
            "--harness",
            "claude",
            "--auth",
            "key",
            "--unconfined",
            "--workdir",
            repo.to_str().unwrap(),
            "--prompt",
            "finish the import",
        ],
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let tapes = cli(root, &["reenact", "tapes"]);
    let tape = &tapes[0];
    assert_eq!(tape["prompt"], "finish the import");
    assert_eq!(tape["harness"], "claude");
    assert_eq!(tape["commit"], head);
    assert_eq!(tape["calls"], 2, "{tapes}");
    tape["session"].as_str().unwrap().to_string()
}

fn re_enact(
    root: &Path,
    bins: &Path,
    session: &str,
    repo: &Path,
    more: &[&str],
) -> serde_json::Value {
    let mut args = vec![
        "reenact",
        "run",
        session,
        "--auth",
        "key",
        "--unconfined",
        "--repo",
        repo.to_str().unwrap(),
    ];
    args.extend_from_slice(more);
    let out = amem_with(root, bins, &args);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn a_prompted_session_is_re_enacted_at_its_commit_against_its_tape() {
    let dir = tempfile::tempdir().unwrap();
    let root = init_root(dir.path());
    let bins = fake_harness(dir.path(), READS_THE_TREE);
    let (repo, head) = repo_of_five_steps(dir.path());
    let session = taped_session(&root, &bins, &repo, &head);
    // the tree moves on; the re-run works at the session's commit all the same
    std::fs::write(repo.join("steps.txt"), "six steps").unwrap();
    git_in(&repo, &["commit", "-q", "-am", "six"]);
    let r = re_enact(&root, &bins, &session, &repo, &[]);
    assert_eq!(r["diverged"], false, "{r}");
    assert_eq!(
        (r["steps_to_divergence"].as_u64(), r["calls"].as_u64()),
        (Some(2), Some(2))
    );
    assert_eq!(
        (&r["handoff"], &r["exit"], &r["alignment"]),
        (&true.into(), &0.into(), &1.0.into())
    );
    assert_eq!(r["tree"]["commit"], head.as_str());
    // nothing written: one handoff, the original's
    assert_eq!(
        cli(&root, &["list", "--kind", "handoff"])
            .as_array()
            .unwrap()
            .len(),
        1
    );
    // the checkout is gone, and the re-run kept no tape of its own
    let siblings: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.contains("reenact"))
        .collect();
    assert!(siblings.is_empty(), "{siblings:?}");
    assert_eq!(
        cli(&root, &["reenact", "tapes"]).as_array().unwrap().len(),
        1
    );
    let report = cli_text(&root, &["reenact", "report", &session]);
    assert!(
        report.contains("1 runs") && report.contains("claude"),
        "{report}"
    );
}

#[test]
fn a_kept_tree_is_the_commit_and_repeated_runs_are_not_a_subscription_s() {
    let dir = tempfile::tempdir().unwrap();
    let root = init_root(dir.path());
    let bins = fake_harness(dir.path(), READS_THE_TREE);
    let (repo, head) = repo_of_five_steps(dir.path());
    let session = taped_session(&root, &bins, &repo, &head);
    std::fs::write(repo.join("steps.txt"), "six steps").unwrap();
    git_in(&repo, &["commit", "-q", "-am", "six"]);
    let r = re_enact(&root, &bins, &session, &repo, &["--keep-tree"]);
    let kept = PathBuf::from(r["tree"]["kept_at"].as_str().unwrap());
    assert_eq!(
        std::fs::read_to_string(kept.join("steps.txt")).unwrap(),
        "five steps"
    );
    // a repository of its own, so git works in it where only it is mounted
    assert!(kept.join(".git").is_dir());
    std::fs::remove_dir_all(&kept).unwrap();
    // the run's home is gone with the run
    let runs = root.join("state/reenact").join(&session);
    assert!(!runs.exists(), "{}", runs.display());
    let out = amem_with(
        &root,
        &bins,
        &[
            "reenact",
            "run",
            &session,
            "--auth",
            "subscription",
            "--runs",
            "2",
        ],
    );
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("API key or the gateway"));
}

/// The projection as a live mount: a shell `cat` inside the sandbox, which no hook
/// sees, is a `get` the memory serves and logs. Where this host has no FUSE the launch
/// says the projection is a render, and the test says it skipped the rest; on CI, which
/// has FUSE, that is a failure.
#[cfg(target_os = "linux")]
#[test]
fn a_file_read_inside_the_sandbox_is_a_get_the_memory_serves() {
    if !namespaces_available() {
        eprintln!("skipping: unprivileged namespaces are not available here");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = init_root(dir.path());
    let written = cli(
        &root,
        &[
            "write",
            "--title",
            "The vault",
            "--body",
            "The signing key lives in the vault.",
        ],
    );
    let id = written["id"].as_str().unwrap().to_string();
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let bins = fake_harness(
        &work,
        &format!(
            "{{ cat /memory/dom-me/{id}.md; echo \"write=$(echo x 2>/dev/null > /memory/dom-me/new.md && echo ok || echo refused)\"; }} > ./fake-claude.out 2>&1\nexit 0"
        ),
    );

    let out = launch(&root, &bins, &work, &[]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    let text = read_out(&work);
    assert!(
        text.contains("The signing key lives in the vault."),
        "the entry is read as a file: {text}"
    );
    assert!(text.contains("write=refused"), "{text}");
    let reads = dm_server::reads::all(&root).unwrap();
    let gets: Vec<_> = reads.iter().filter(|r| r.tool == "get").collect();
    if stderr.contains("is a render:") {
        assert!(
            std::env::var_os("CI").is_none(),
            "on CI the projection must be a live mount: {stderr}"
        );
        eprintln!("skipping the rest: {stderr}");
        assert!(gets.is_empty(), "no hook, no mount, no get: {gets:?}");
        return;
    }
    assert!(stderr.contains("is a live mount"), "{stderr}");
    // the fake harness has no hooks: only the mount could have made this get
    assert_eq!(gets.len(), 1, "{reads:?}");
    assert_eq!(gets[0].ids, vec![id]);
    // and the mountpoint went with the session
    let left = std::fs::read_dir(root.join("state/mount/claude"))
        .map(|d| d.count())
        .unwrap_or(0);
    assert_eq!(left, 0, "a mountpoint outlived its session");
}
