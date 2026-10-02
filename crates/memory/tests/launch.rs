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
