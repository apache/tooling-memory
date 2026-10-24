//! Running a probe inside the namespaces sandbox: this test binary itself, bound in
//! read-only, with one ignored test selected as the program to run.

// Each test binary uses what it needs of this module.
#![allow(dead_code)]

use dm_core::Label;
use dm_sandbox::{Layout, Mount, Plan};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The prefix of each line a probe reports on, which sets it apart from libtest's own.
pub const REPORT: &str = "probe: ";

/// The host's system, read-only, and its merged-usr links, so the test binary runs.
fn system() -> (Vec<Mount>, Vec<(PathBuf, PathBuf)>) {
    let mut mounts = Vec::new();
    let mut links = Vec::new();
    for d in ["usr", "bin", "sbin", "lib", "lib64", "lib32"] {
        let p = Path::new("/").join(d);
        if let Ok(to) = std::fs::read_link(&p) {
            links.push((PathBuf::from(d), to));
        } else if p.is_dir() {
            mounts.push(Mount {
                source: p.clone(),
                target: p,
                read_only: true,
            });
        }
    }
    (mounts, links)
}

/// A plan that runs the ignored test `probe` of this binary inside the sandbox, with
/// `mounts` after the system's and `env` as the probe's whole environment.
pub fn plan(probe: &str, mounts: Vec<Mount>, env: BTreeMap<String, String>) -> Plan {
    let exe = std::env::current_exe().unwrap();
    let dir = exe.parent().unwrap().to_path_buf();
    let (mut all, links) = system();
    all.push(Mount {
        source: dir.clone(),
        target: dir,
        read_only: true,
    });
    all.extend(mounts);
    Plan {
        mounts: all,
        env,
        argv: vec![
            exe.display().to_string(),
            "--exact".into(),
            probe.into(),
            "--ignored".into(),
            "--nocapture".into(),
            "--test-threads=1".into(),
        ],
        cwd: PathBuf::from("/tmp"),
        hostname: "probe".into(),
        handling_cap: Label::Public,
        links,
        layout: Layout::namespaces(),
        host_home: None,
    }
}

/// What each line of a probe's report said, by name. libtest's `test <name> ... ` comes
/// before the first line, on the same line.
pub fn report(out: &[u8]) -> BTreeMap<String, String> {
    String::from_utf8_lossy(out)
        .lines()
        .filter_map(|l| l.split_once(REPORT).map(|(_, r)| r))
        .filter_map(|l| l.split_once(' '))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// A report line: `ok` for a call that succeeded, else the errno it failed with.
pub fn say(name: &str, result: Result<(), std::io::Error>) {
    let what = match result {
        Ok(()) => "ok".to_string(),
        Err(e) => e.raw_os_error().map_or(e.to_string(), |n| n.to_string()),
    };
    println!("{REPORT}{name} {what}");
}

/// The result of a raw syscall that returns a descriptor, closing the descriptor.
pub fn opened(r: libc::c_long) -> Result<(), std::io::Error> {
    if r < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if let Ok(fd) = libc::c_int::try_from(r) {
        // SAFETY: closing a descriptor the probe's own call just returned.
        unsafe { libc::close(fd) };
    }
    Ok(())
}
