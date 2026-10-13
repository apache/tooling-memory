//! Shared by the browser tests: the Chrome or Chromium they drive.

// Test support; a missing browser is reported as the panic says.
#![allow(clippy::unwrap_used, dead_code)]

use std::path::{Path, PathBuf};

/// A Chrome or Chromium to drive: `CHROME` in the environment, else where the usual
/// installs put it, including the Playwright browser directory.
pub fn chrome() -> PathBuf {
    if let Some(c) = std::env::var_os("CHROME") {
        return PathBuf::from(c);
    }
    let mut candidates: Vec<PathBuf> = vec![
        "/usr/bin/google-chrome".into(),
        "/usr/bin/google-chrome-stable".into(),
        "/usr/bin/chromium".into(),
        "/usr/bin/chromium-browser".into(),
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome".into(),
        r"C:\Program Files\Google\Chrome\Application\chrome.exe".into(),
        r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe".into(),
    ];
    // Playwright's browsers: where its install put them, its default cache, and the CI
    // image's directory; the layout is chrome-linux64 now and was chrome-linux
    let mut pw: Vec<PathBuf> = std::env::var_os("PLAYWRIGHT_BROWSERS_PATH")
        .map(PathBuf::from)
        .into_iter()
        .collect();
    if let Some(home) = std::env::var_os("HOME") {
        pw.push(PathBuf::from(home).join(".cache/ms-playwright"));
    }
    pw.push("/opt/pw-browsers".into());
    for dir in pw {
        let Ok(dirs) = std::fs::read_dir(dir) else {
            continue;
        };
        for d in dirs.flatten() {
            if d.file_name().to_string_lossy().starts_with("chromium-") {
                candidates.push(d.path().join("chrome-linux64/chrome"));
                candidates.push(d.path().join("chrome-linux/chrome"));
            }
        }
    }
    // a browser counts only if it starts: one present without the libraries it needs
    // falls through to the container, which carries them
    let launches = |p: &PathBuf| {
        p.is_file()
            && std::process::Command::new(p)
                .args(["--headless=new", "--version"])
                .output()
                .is_ok_and(|o| o.status.success())
    };
    candidates
        .into_iter()
        .find(launches)
        .or_else(chrome_in_a_container)
        .expect("no Chrome or Chromium that starts, and no container runtime that runs; install one, or set CHROME=<path>, for the reader test")
}

/// The Playwright image's Chrome, through scripts/chrome-container.sh, when a container
/// runtime answers: a host with no Chrome, or one without the libraries it needs. Linux
/// only, where the container's host network is this host's and reaches the test's server;
/// elsewhere a container runs in a virtual machine whose loopback is its own.
fn chrome_in_a_container() -> Option<PathBuf> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    // `info` asks the daemon, so a runtime that is installed but cannot run a container
    // leaves the clear message above rather than a failed `run`
    let runs = |runtime: &str| {
        std::process::Command::new(runtime)
            .arg("info")
            .output()
            .is_ok_and(|o| o.status.success())
    };
    (runs("podman") || runs("docker"))
        .then(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/chrome-container.sh"))
}
