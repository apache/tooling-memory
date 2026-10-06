//! `amem` as an operator meets it: `doctor` on a host it has to find things on, and the
//! bare command starting the web launcher, finding it again, and opening the browser.

// An integration test is test code; a panic here is the failure being reported.
#![allow(clippy::unwrap_used)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// Every variable the launcher reads, cleared so the host's own do not leak in.
const CREDENTIALS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "GEMINI_API_KEY",
    "XAI_API_KEY",
    "DASHSCOPE_API_KEY",
    "META_API_KEY",
    "LLMAO_KEY",
    "LLMAO_URL",
    "GH_TOKEN",
    "GOOGLE_CLOUD_PROJECT",
    "LITELLM_MASTER_KEY",
    "POSTGRES_PASSWORD",
    "MEMORY_ROOT",
    "BROWSER",
    "WSL_DISTRO_NAME",
];

/// A host of its own: a home, a PATH holding only what the test puts there, a record.
struct Host {
    dir: tempfile::TempDir,
}

impl Host {
    fn new() -> Host {
        let h = Host {
            dir: tempfile::tempdir().unwrap(),
        };
        for d in ["home", "bin", "work"] {
            std::fs::create_dir_all(h.path(d)).unwrap();
        }
        h
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    fn root(&self) -> PathBuf {
        self.path("home/memory")
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_amem"));
        for k in CREDENTIALS {
            c.env_remove(k);
        }
        c.env("HOME", self.path("home"))
            .env("PATH", self.path("bin"))
            .current_dir(self.path("work"))
            .args(args)
            .stdin(Stdio::null());
        c
    }

    fn run(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut c = self.cmd(args);
        for (k, v) in env {
            c.env(k, v);
        }
        c.output().unwrap()
    }

    fn init(&self) {
        let out = self.run(&["init", "--domain", "dom-me"], &[]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn file(&self, rel: &str, text: &str) {
        let p = self.path(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    #[cfg(unix)]
    fn script(&self, name: &str, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        let p = self.path(&format!("bin/{name}"));
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(unix)]
    fn mode(&self, rel: &str, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(self.path(rel), std::fs::Permissions::from_mode(mode)).unwrap();
    }
}

/// A gateway that lists two models, on a port of its own.
fn gateway() -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    std::thread::spawn(move || {
        for s in l.incoming() {
            let Ok(mut s) = s else { continue };
            let mut buf = [0u8; 8192];
            let _ = s.read(&mut buf);
            let body = r#"{"data":[{"id":"open-model-a"},{"id":"open-model-b"}]}"#;
            let _ = write!(
                s,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    format!("http://{addr}")
}

/// A port nothing listens on.
fn closed_port() -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    drop(l);
    format!("http://{addr}")
}

fn doctor_json(h: &Host, env: &[(&str, &str)]) -> serde_json::Value {
    let out = h.run(&["doctor", "--json"], env);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).unwrap();
    for secret in ["sk-from-env", "sk-from-file", "pat-from-file"] {
        assert!(
            !text.contains(secret),
            "doctor printed a credential: {text}"
        );
    }
    serde_json::from_str(&text).unwrap()
}

fn find<'a>(report: &'a serde_json::Value, area: &str, subject: &str) -> &'a serde_json::Value {
    report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["area"] == area && c["subject"] == subject)
        .unwrap_or_else(|| panic!("no {area}/{subject} in {report:#}"))
}

/// A host with a record, Claude Code's and Gemini CLI's logins, `claude` on the PATH,
/// and a credentials file naming a key, the gateway at `gateway_url`, and a line it skips.
#[cfg(unix)]
fn doctor_host(gateway_url: &str) -> Host {
    let h = Host::new();
    h.init();
    h.file("home/.claude/.credentials.json", "{}");
    h.file("home/.gemini/oauth_creds.json", "{}");
    h.script("claude", "echo '9.9.9 (Claude Code)'");
    h.file(
        "home/memory/credentials.env",
        &format!(
            "# keys\nOPENAI_API_KEY=sk-from-file\nexport LLMAO_KEY=\"pat-from-file\"\nLLMAO_URL={gateway_url}\nLD_PRELOAD=/tmp/x.so\n"
        ),
    );
    h.mode("home/memory/credentials.env", 0o600);
    h
}

fn detail(report: &serde_json::Value, area: &str, subject: &str) -> String {
    find(report, area, subject)["detail"]
        .as_str()
        .unwrap()
        .to_string()
}

#[cfg(unix)]
#[test]
fn doctor_names_each_credential_by_where_it_came_from_and_never_its_value() {
    let h = doctor_host(&gateway());
    let r = doctor_json(&h, &[("ANTHROPIC_API_KEY", "sk-from-env")]);
    assert_eq!(find(&r, "record", "root")["status"], "pass");
    assert_eq!(find(&r, "record", "grants")["status"], "pass");
    let file = find(&r, "credentials", "credentials.env");
    assert_eq!(file["status"], "warn", "{file}");
    assert!(
        file["fix"].as_str().unwrap().contains("LD_PRELOAD"),
        "{file}"
    );
    assert!(detail(&r, "credentials", "ANTHROPIC_API_KEY").starts_with("from environment"));
    assert!(detail(&r, "credentials", "OPENAI_API_KEY").starts_with("from credentials.env"));
    assert_eq!(find(&r, "credentials", "XAI_API_KEY")["status"], "absent");
    // no gh on this PATH and no GH_TOKEN: no GitHub login
    assert_eq!(find(&r, "credentials", "GH_TOKEN")["status"], "absent");
}

#[cfg(unix)]
#[test]
fn doctor_finds_logins_harnesses_and_the_gateway_and_says_what_can_launch() {
    let h = doctor_host(&gateway());
    let r = doctor_json(&h, &[("ANTHROPIC_API_KEY", "sk-from-env")]);
    assert_eq!(find(&r, "logins", "claude")["status"], "pass");
    assert_eq!(find(&r, "logins", "gemini")["status"], "pass");
    assert_eq!(find(&r, "logins", "codex")["status"], "absent");
    assert_eq!(find(&r, "harnesses", "claude")["status"], "pass");
    assert!(detail(&r, "harnesses", "claude").contains("9.9.9"));
    assert_eq!(find(&r, "harnesses", "codex")["status"], "absent");
    assert_eq!(find(&r, "gateway", "llmao")["status"], "pass");
    assert!(detail(&r, "gateway", "llmao").contains("2 models"));
    assert_eq!(find(&r, "web", "launcher")["status"], "absent");
    let ready = &r["ready"];
    assert_eq!(ready["claude"]["subscription"]["ok"], true);
    assert_eq!(ready["claude"]["key"]["ok"], true);
    assert_eq!(ready["claude"]["gateway"]["ok"], true);
    assert_eq!(
        ready["codex"]["subscription"]["missing"],
        "codex not installed"
    );
}

#[cfg(unix)]
#[test]
fn a_gateway_that_does_not_answer_is_a_failure_and_closes_the_gateway_column() {
    let h = doctor_host(&closed_port());
    let r = doctor_json(&h, &[]);
    assert_eq!(find(&r, "gateway", "llmao")["status"], "fail");
    assert_eq!(
        r["ready"]["claude"]["gateway"]["missing"],
        "gateway not answering"
    );
    assert_eq!(r["ready"]["claude"]["key"]["missing"], "ANTHROPIC_API_KEY");
}

#[cfg(unix)]
#[test]
fn a_credentials_file_others_can_read_is_refused_and_counts_for_nothing() {
    let h = doctor_host(&closed_port());
    h.mode("home/memory/credentials.env", 0o644);
    let r = doctor_json(&h, &[]);
    let file = find(&r, "credentials", "credentials.env");
    assert_eq!(file["status"], "fail");
    assert!(
        file["fix"].as_str().unwrap().contains("chmod 600"),
        "{file}"
    );
    assert_eq!(find(&r, "credentials", "LLMAO_KEY")["status"], "absent");

    // the text form, offline: the table a person reads
    let out = h.run(
        &["doctor", "--offline"],
        &[("ANTHROPIC_API_KEY", "sk-from-env")],
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("ready to launch"), "{text}");
    assert!(text.contains("claude") && text.contains("yes"), "{text}");
    assert!(text.contains("chmod 600"), "{text}");
    assert!(!text.contains("sk-from-env"), "{text}");
}

#[test]
fn doctor_without_a_record_says_how_to_make_one() {
    let h = Host::new();
    let r = doctor_json(&h, &[]);
    let root = find(&r, "record", "root");
    assert_eq!(root["status"], "fail");
    assert!(
        root["fix"].as_str().unwrap().contains("amem init"),
        "{root}"
    );
    assert_eq!(
        find(&r, "credentials", "credentials.env")["status"],
        "absent"
    );
}

/// Wait until `f` gives something, or fail after twenty seconds.
fn eventually<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// What `f` gives within `limit`, or nothing.
fn eventually_or_none<T>(limit: Duration, mut f: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if let Some(v) = f() {
            return Some(v);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

fn web_state(root: &Path) -> serde_json::Value {
    std::fs::read_to_string(root.join("state/web.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(serde_json::Value::Null)
}

#[cfg(unix)]
#[test]
fn amem_starts_the_launcher_finds_it_again_and_opens_the_browser_on_its_link() {
    let h = Host::new();
    h.init();
    let opened = h.path("opened");
    h.script("browser", &format!("echo \"$1\" >> '{}'", opened.display()));
    let browser = h.path("bin/browser").display().to_string();
    let mut server = h.cmd(&["web", "--listen", "127.0.0.1:0"]);
    server
        .env("BROWSER", &browser)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut server = server.spawn().unwrap();

    let url = eventually("the state file", || {
        web_state(&h.root())["url"].as_str().map(str::to_string)
    });
    let token = web_state(&h.root())["token"].as_str().unwrap().to_string();
    assert_eq!(token.len(), 64);
    let first = eventually("the browser", || {
        std::fs::read_to_string(&opened)
            .ok()
            .filter(|t| !t.is_empty())
    });
    assert_eq!(first.trim(), format!("{url}/?token={token}"));

    // run again with nothing named: the one already up is found and opened, not a second
    // server started, which would run until stopped
    let mut again = h.cmd(&[]);
    again
        .env("BROWSER", &browser)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut again = again.spawn().unwrap();
    let finished = eventually_or_none(Duration::from_secs(10), || again.try_wait().unwrap());
    if finished.is_none() {
        let _ = again.kill();
        let _ = again.wait();
        let _ = server.kill();
        panic!("amem is still running: it started a second server instead of finding the first");
    }
    let out = again.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains(&format!("{url}/?token={token}")), "{err}");
    assert!(
        !err.contains("web launcher over"),
        "a second server started: {err}"
    );
    eventually("the second open", || {
        let t = std::fs::read_to_string(&opened).ok()?;
        (t.lines().count() == 2).then_some(())
    });
    let r = doctor_json(&h, &[]);
    let web = find(&r, "web", "launcher");
    assert_eq!(web["status"], "pass");
    assert!(web["detail"].as_str().unwrap().contains(&url), "{web}");

    // stopped with ctrl-c, the address goes and the token stays for the next server
    let pid = server.id().to_string();
    assert!(Command::new("kill")
        .args(["-INT", &pid])
        .status()
        .unwrap()
        .success());
    let status = server.wait().unwrap();
    assert!(status.success(), "{status:?}");
    let state = web_state(&h.root());
    assert!(state["url"].is_null(), "{state}");
    assert_eq!(state["token"], token.as_str());
    let mut err = String::new();
    server
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut err)
        .unwrap();
    assert!(err.contains("web launcher over"), "{err}");
}

#[test]
fn the_launcher_refuses_the_network_and_a_root_that_is_not_a_record() {
    let h = Host::new();
    h.init();
    let out = h.run(&["web", "--listen", "0.0.0.0:0", "--no-open"], &[]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not a loopback address"));

    std::fs::create_dir_all(h.path("empty")).unwrap();
    let empty = h.path("empty").display().to_string();
    let out = h.run(&["--root", &empty, "web", "--no-open"], &[]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("no record at") && err.contains("amem doctor"),
        "{err}"
    );
}
