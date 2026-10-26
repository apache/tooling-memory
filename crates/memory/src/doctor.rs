//! `amem doctor`: what this host can run, before anything is launched. The root and its
//! grants, the credentials and logins found, the harnesses on the PATH and in the image,
//! the backend `auto` picks, whether the gateway answers and whether the web launcher is
//! up; then, per harness and auth mode, whether a launch has what it needs. Credentials
//! are named, never shown.

use crate::credentials::{self, KNOWN};
use crate::{backend_of, image, open, AuthMode, Backend, BackendArg, Fallible};
use dm_sandbox::Harness;
use dm_server::Memory;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

/// How one check came out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// There and working.
    Pass,
    /// There, with something to fix.
    Warn,
    /// There and broken.
    Fail,
    /// Not there, which may be fine.
    Absent,
}

impl Status {
    fn word(self) -> &'static str {
        match self {
            Status::Pass => "ok",
            Status::Warn => "warn",
            Status::Fail => "fail",
            Status::Absent => "--",
        }
    }
}

/// One line of the report.
#[derive(Clone, Debug, Serialize)]
pub struct Check {
    /// The section: record, credentials, logins, harnesses, backend, gateway, web.
    pub area: &'static str,
    /// What was checked.
    pub subject: String,
    /// How it came out.
    pub status: Status,
    /// What was found.
    pub detail: String,
    /// What to do about it, when there is something to do.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

fn check(
    area: &'static str,
    subject: impl Into<String>,
    status: Status,
    detail: impl Into<String>,
) -> Check {
    Check {
        area,
        subject: subject.into(),
        status,
        detail: detail.into(),
        fix: None,
    }
}

impl Check {
    fn fix(mut self, f: impl Into<String>) -> Check {
        self.fix = Some(f.into());
        self
    }
}

/// Whether one harness can run under one auth mode, and if not, what it lacks.
#[derive(Clone, Debug, Serialize)]
pub struct Ready {
    /// Whether a launch has what it needs.
    pub ok: bool,
    /// What it lacks, or `n/a` where the harness has no such mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub missing: Option<String>,
}

/// The whole report.
#[derive(Debug, Serialize)]
pub struct Report {
    /// The root it was made for.
    pub root: String,
    /// Every check, in order.
    pub checks: Vec<Check>,
    /// Harness, then auth mode, to whether it can run.
    pub ready: BTreeMap<String, BTreeMap<String, Ready>>,
}

/// What the report learns that the readiness table is computed from.
#[derive(Default)]
struct Found {
    /// Credential names with a value, from anywhere.
    set: Vec<String>,
    /// Harness binaries the chosen backend can run.
    runnable: Vec<Harness>,
    /// Harnesses with their login under the home.
    logged_in: Vec<Harness>,
    /// A GitHub token, from GH_TOKEN or gh.
    github: bool,
    /// Whether the gateway answered; `None` when it was not asked.
    gateway: Option<bool>,
}

fn record(root: &Path, checks: &mut Vec<Check>) -> bool {
    let Ok(m) = Memory::open(root) else {
        checks.push(
            check(
                "record",
                "root",
                Status::Fail,
                format!("no record at {}", root.display()),
            )
            .fix("amem init, or give --root (or MEMORY_ROOT) for a record elsewhere"),
        );
        return false;
    };
    let g = m.grants();
    checks.push(check(
        "record",
        "root",
        Status::Pass,
        format!(
            "{}: owner {}, domains {}",
            root.display(),
            g.owner,
            g.domains.keys().cloned().collect::<Vec<_>>().join(", ")
        ),
    ));
    let ceilings = g
        .destinations
        .iter()
        .map(|(d, c)| format!("{d} {}", c.ceiling))
        .collect::<Vec<_>>();
    let c = if ceilings.is_empty() {
        check(
            "record",
            "grants",
            Status::Warn,
            "no destinations: nothing may be sent to a model",
        )
        .fix("add [destinations.<name>] with a ceiling to grants.toml")
    } else {
        check(
            "record",
            "grants",
            Status::Pass,
            format!("handling {}; ceilings {}", g.handling, ceilings.join(", ")),
        )
    };
    checks.push(c);
    true
}

fn credentials_file(root: &Path, checks: &mut Vec<Check>) -> Option<credentials::Read> {
    match credentials::read(root) {
        Ok(None) => {
            checks.push(
                check(
                    "credentials",
                    credentials::FILE,
                    Status::Absent,
                    "none; only the environment is read",
                )
                .fix(format!(
                    "keys go in {} as NAME=value, mode 600",
                    root.join(credentials::FILE).display()
                )),
            );
            None
        }
        Ok(Some(r)) => {
            let names: Vec<&str> = r.pairs.iter().map(|(k, _)| k.as_str()).collect();
            let status = if r.problems.is_empty() {
                Status::Pass
            } else {
                Status::Warn
            };
            let mut c = check(
                "credentials",
                credentials::FILE,
                status,
                format!(
                    "{}; names {}",
                    if credentials::CHECKS_PERMISSIONS {
                        "owner-only"
                    } else {
                        "permissions not checked on this platform"
                    },
                    if names.is_empty() {
                        "nothing".into()
                    } else {
                        names.join(", ")
                    }
                ),
            );
            if !r.problems.is_empty() {
                c = c.fix(r.problems.join("; "));
            }
            checks.push(c);
            Some(r)
        }
        Err(e) => {
            checks.push(check("credentials", credentials::FILE, Status::Fail, "not loaded").fix(e));
            None
        }
    }
}

fn credential_names(root: &Path, at_start: &[String], checks: &mut Vec<Check>, found: &mut Found) {
    let file = credentials_file(root, checks);
    for (name, opens) in KNOWN {
        match credentials::source(name, file.as_ref(), at_start) {
            Some(from) => {
                found.set.push((*name).to_string());
                checks.push(check(
                    "credentials",
                    *name,
                    Status::Pass,
                    format!("from {from}: {opens}"),
                ));
            }
            None if *name == "GH_TOKEN" => {
                // gh keeps its token in the keyring; asking it is how a login is found
                found.github = credentials::gh_token().is_some();
                checks.push(if found.github {
                    check(
                        "credentials",
                        *name,
                        Status::Pass,
                        format!("from `gh auth token`: {opens}"),
                    )
                } else {
                    check(
                        "credentials",
                        *name,
                        Status::Absent,
                        format!("not set: {opens}"),
                    )
                    .fix("for copilot, COPILOT_GITHUB_TOKEN in credentials.env; else gh auth login, or GH_TOKEN there")
                });
            }
            None => checks.push(check(
                "credentials",
                *name,
                Status::Absent,
                format!("not set: {opens}"),
            )),
        }
    }
    found.github |= found.set.iter().any(|n| n == "GH_TOKEN");
}

/// When copilot runs here on the GitHub login, `COPILOT_GITHUB_TOKEN`'s line says what
/// that costs: the login's token reaches every repository the login can, from inside
/// the sandbox. Decided once the harnesses are known, so a host without copilot is not
/// told about it.
fn copilot_token(checks: &mut [Check], found: &Found) {
    let on_login = found.runnable.contains(&Harness::Copilot)
        && found.github
        && !found.set.iter().any(|n| n == "COPILOT_GITHUB_TOKEN");
    if !on_login {
        return;
    }
    if let Some(c) = checks
        .iter_mut()
        .find(|c| c.area == "credentials" && c.subject == "COPILOT_GITHUB_TOKEN")
    {
        *c = check(
            "credentials",
            "COPILOT_GITHUB_TOKEN",
            Status::Warn,
            "not set: copilot gets your GitHub login's token, which reaches every repository the login can",
        )
        .fix("a fine-grained token with only the Copilot Requests permission, as COPILOT_GITHUB_TOKEN in credentials.env");
    }
}

/// The credential each tap's grant names, which only the launcher's environment holds:
/// a tap without it runs with none, and a hosted tool's server is called with none.
fn tap_credentials(root: &Path, at_start: &[String], checks: &mut Vec<Check>) {
    let Ok(g) = dm_policy::Grants::load(&root.join(dm_server::GRANTS_FILE)) else {
        return;
    };
    for (name, tap) in &g.taps {
        let Some(var) = &tap.credential else { continue };
        let subject = format!("taps.{name}");
        let set = credentials::present(std::env::var(var).ok()).is_some();
        // a name the launcher reads may have come from credentials.env, loaded at start
        let from = if KNOWN.iter().any(|(k, _)| k == var) && !at_start.iter().any(|n| n == var) {
            credentials::FILE
        } else {
            "environment"
        };
        checks.push(if set {
            check(
                "credentials",
                subject,
                Status::Pass,
                format!("{var} from {from}"),
            )
        } else {
            let then = if tap.hosted.is_some() {
                "its tool server is called with no credential"
            } else {
                "it runs without one"
            };
            let then = if var == "GH_TOKEN" {
                format!("{then}, except on the wslc backend, which fills it from `gh auth token`")
            } else {
                then.to_string()
            };
            check(
                "credentials",
                subject,
                Status::Warn,
                format!("{var} is not set: {then}"),
            )
            .fix(format!("set {var} in the launcher's environment"))
        });
    }
}

fn logins(checks: &mut Vec<Check>, found: &mut Found) {
    let Some(home) = crate::home_dir() else {
        checks.push(check(
            "logins",
            "home",
            Status::Fail,
            "no HOME to look for logins under",
        ));
        return;
    };
    for h in Harness::ALL {
        let Some(login) = h.login() else { continue };
        if home.join(login.file).is_file() {
            found.logged_in.push(h);
            checks.push(check(
                "logins",
                h.binary(),
                Status::Pass,
                format!("~/{}", login.file),
            ));
        } else {
            checks.push(
                check(
                    "logins",
                    h.binary(),
                    Status::Absent,
                    format!("no ~/{}", login.file),
                )
                .fix(login.how),
            );
        }
    }
}

/// The first line `<binary> --version` prints, within a few seconds.
async fn version(binary: &str) -> Option<String> {
    let mut cmd = tokio::process::Command::new(binary);
    // asking a harness its version needs none of the launcher's credentials
    for (k, _) in KNOWN {
        cmd.env_remove(k);
    }
    cmd.arg("--version")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_secs(5), cmd.output())
        .await
        .ok()?
        .ok()?;
    let text = String::from_utf8_lossy(if out.stdout.is_empty() {
        &out.stderr
    } else {
        &out.stdout
    })
    .into_owned();
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
}

fn on_path(binary: &str) -> bool {
    image::on_path(binary)
        || (cfg!(windows)
            && [".exe", ".cmd"]
                .iter()
                .any(|x| image::on_path(&format!("{binary}{x}"))))
}

/// Whether this host lets an unprivileged process enter the user namespace the native
/// backends build their sandbox in. Probed rather than assumed: on Ubuntu 23.10 and
/// later the namespace is created and then has no `CAP_SYS_ADMIN`, so without this the
/// failure lands in the middle of a launch as a bare "Permission denied" that names
/// neither AppArmor nor user namespaces.
#[cfg(target_os = "linux")]
fn user_namespaces(backend: Backend, checks: &mut Vec<Check>) {
    if !matches!(backend, Backend::Namespaces | Backend::Image) {
        return;
    }
    checks.push(match dm_sandbox::linux::availability() {
        Ok(()) => check(
            "backend",
            "user namespaces",
            Status::Pass,
            "an unprivileged user namespace can be created and mapped",
        ),
        Err(why) => check("backend", "user namespaces", Status::Fail, why).fix(
            "sudo install -m 644 packaging/apparmor/amem /etc/apparmor.d/amem && \
             sudo apparmor_parser -r /etc/apparmor.d/amem; or launch with --unconfined",
        ),
    });
}

/// Nothing to report: the backends that need a user namespace are Linux's.
#[cfg(not(target_os = "linux"))]
fn user_namespaces(_backend: Backend, _checks: &mut Vec<Check>) {}

/// How the projection reaches the sandbox: served live where the host has FUSE, rendered
/// where not. Only where a backend of this host mounts it.
fn projection(backend: Backend, checks: &mut Vec<Check>) {
    if !matches!(backend, Backend::Namespaces | Backend::Image) {
        return;
    }
    checks.push(match dm_mount::availability() {
        Ok(()) => check(
            "backend",
            "projection",
            Status::Pass,
            "a live mount: every open of an entry in the sandbox is a get the memory serves",
        ),
        Err(why) => check(
            "backend",
            "projection",
            Status::Warn,
            format!("a render, with the hook turning reads into gets: {why}"),
        ),
    });
}

async fn harnesses(root: &Path, checks: &mut Vec<Check>, found: &mut Found) {
    // the backend auto picks depends on the harness, since an unpacked image may lack
    // one; the summary line speaks for Claude Code, the loop below asks per harness
    let backend = backend_of(BackendArg::Auto, false, root, Harness::Claude);
    let rootfs = image::present(root);
    let in_image = |b: &str| {
        rootfs
            .as_ref()
            .is_some_and(|r| r.join("usr/local/bin").join(b).symlink_metadata().is_ok())
    };
    let backend_detail = match (backend, &rootfs) {
        (Backend::Image, Some(r)) => format!("auto is the unpacked image at {}", r.display()),
        (Backend::Namespaces, _) => {
            "auto is namespaces over this host's binaries; `make unpack` makes it the image".into()
        }
        (Backend::Wslc, _) => "auto is wslc: the image in a WSL container".into(),
        (Backend::Seatbelt, _) => {
            "auto is Seatbelt over this host's binaries; dry-coded, not yet run on a Mac".into()
        }
        _ => "auto runs unconfined: no backend for this platform".into(),
    };
    let status = if backend == Backend::None {
        Status::Warn
    } else {
        Status::Pass
    };
    checks.push(check("backend", "auto", status, backend_detail));
    user_namespaces(backend, checks);
    projection(backend, checks);
    let list: Vec<Harness> = Harness::ALL
        .into_iter()
        .filter(|h| *h != Harness::Command)
        .collect();
    let versions = futures_versions(&list).await;
    for (h, v) in list.into_iter().zip(versions) {
        let b = h.binary();
        let host = on_path(b);
        let image = in_image(b);
        let runnable = match backend_of(BackendArg::Auto, false, root, h) {
            Backend::Image | Backend::Wslc => image,
            _ => host,
        };
        if runnable {
            found.runnable.push(h);
        }
        let mut where_ = Vec::new();
        if host {
            where_.push(format!(
                "on PATH{}",
                v.map(|v| format!(" ({v})")).unwrap_or_default()
            ));
        }
        if image {
            where_.push("in the image".to_string());
        }
        let c = match (runnable, where_.is_empty()) {
            (true, _) => check("harnesses", b, Status::Pass, where_.join(" and ")),
            (false, false) => check(
                "harnesses",
                b,
                Status::Warn,
                format!(
                    "{}, but not where the backend runs it",
                    where_.join(" and ")
                ),
            ),
            (false, true) => check("harnesses", b, Status::Absent, "not installed"),
        };
        checks.push(c);
    }
}

async fn futures_versions(list: &[Harness]) -> Vec<Option<String>> {
    let tasks: Vec<_> = list
        .iter()
        .map(|h| {
            let b = h.binary().to_string();
            tokio::spawn(async move {
                if on_path(&b) {
                    version(&b).await
                } else {
                    None
                }
            })
        })
        .collect();
    let mut out = Vec::with_capacity(tasks.len());
    for t in tasks {
        out.push(t.await.ok().flatten());
    }
    out
}

async fn gateway(
    root: &Path,
    offline: bool,
    have_record: bool,
    checks: &mut Vec<Check>,
    found: &mut Found,
) {
    let url = std::env::var("LLMAO_URL").unwrap_or_else(|_| "http://127.0.0.1:4000".into());
    if !found.set.iter().any(|n| n == "LLMAO_KEY") {
        checks.push(
            check("gateway", "llmao", Status::Absent, format!("no LLMAO_KEY for {url}"))
                .fix("make local-key for the local LiteLLM, or the key the gateway's front desk gave you"),
        );
        return;
    }
    if offline || !have_record {
        checks.push(check(
            "gateway",
            "llmao",
            Status::Warn,
            format!("LLMAO_KEY set for {url}; not asked whether it answers"),
        ));
        return;
    }
    let asked = tokio::time::timeout(
        Duration::from_secs(10),
        crate::list_models(root, AuthMode::Gateway, "llmao"),
    )
    .await;
    let c = match asked {
        Ok(Ok((_, models))) => {
            found.gateway = Some(true);
            check(
                "gateway",
                "llmao",
                Status::Pass,
                format!("{url} answers: {} models", models.len()),
            )
        }
        Ok(Err(e)) => {
            found.gateway = Some(false);
            let c = check("gateway", "llmao", Status::Fail, format!("{url}: {e}"));
            if std::env::var_os("LLMAO_URL").is_none() {
                // the commonest cause: a PAT for the Foundation's gateway, with the
                // address left to its default, the local LiteLLM
                c.fix("LLMAO_URL is not set, so the key goes to the local LiteLLM; for the Foundation's gateway put LLMAO_URL=https://llm.apache.org in credentials.env")
            } else {
                c
            }
        }
        Err(_) => {
            found.gateway = Some(false);
            check(
                "gateway",
                "llmao",
                Status::Fail,
                format!("{url}: no answer in ten seconds"),
            )
        }
    };
    checks.push(c);
}

async fn web(root: &Path, checks: &mut Vec<Check>) {
    let loaded = match dm_web::state::load(root) {
        Ok(l) => l,
        Err(e) => {
            checks.push(check(
                "web",
                "launcher",
                Status::Fail,
                format!("not checked: {e}"),
            ));
            return;
        }
    };
    if let Some(old) = loaded.exposed {
        let running = match &old.url {
            Some(url) => open::running(url, &old.token, root).await,
            None => None,
        };
        let detail = match running {
            Some(r) => format!("running at {}, with a token others can read", r.url),
            None => "its state file is readable by others".to_string(),
        };
        checks.push(
            check("web", "launcher", Status::Fail, detail)
                .fix("stop it if it runs, then amem, which replaces the token"),
        );
        return;
    }
    let running = match &loaded.state.url {
        Some(url) => open::running(url, &loaded.state.token, root).await,
        None => None,
    };
    checks.push(match (running, loaded.why) {
        (Some(r), _) => check(
            "web",
            "launcher",
            Status::Pass,
            format!("running at {}", r.url),
        ),
        (None, Some(why)) => check("web", "launcher", Status::Warn, why).fix("amem"),
        (None, None) => check("web", "launcher", Status::Absent, "not running").fix("amem"),
    });
}

/// Whether `h` can run under `auth` on what was found.
fn ready(h: Harness, auth: AuthMode, f: &Found) -> Ready {
    let no = |why: &str| Ready {
        ok: false,
        missing: Some(why.to_string()),
    };
    if !f.runnable.contains(&h) {
        return no(&format!("{} not installed", h.binary()));
    }
    let need = match (h, auth) {
        (Harness::Copilot, AuthMode::Subscription) => {
            let token = f.github || f.set.iter().any(|n| n == "COPILOT_GITHUB_TOKEN");
            (!token).then(|| "COPILOT_GITHUB_TOKEN, GH_TOKEN or gh login".to_string())
        }
        (Harness::Copilot, _) | (Harness::Qwen | Harness::Pi, AuthMode::Subscription) => {
            return no("n/a")
        }
        (_, AuthMode::Subscription) => (!f.logged_in.contains(&h)).then(|| "login".to_string()),
        (_, AuthMode::Key) => {
            let var = credentials::key_var(h.provider()).unwrap_or("a key");
            (!f.set.iter().any(|n| n == var)).then(|| var.to_string())
        }
        (_, AuthMode::Gateway) => {
            if !f.set.iter().any(|n| n == "LLMAO_KEY") {
                Some("LLMAO_KEY".to_string())
            } else {
                match f.gateway {
                    Some(true) => None,
                    Some(false) => Some("gateway not answering".to_string()),
                    None => Some("gateway not asked".to_string()),
                }
            }
        }
    };
    match need {
        Some(why) => no(&why),
        None => Ready {
            ok: true,
            missing: None,
        },
    }
}

const MODES: [(&str, AuthMode); 3] = [
    ("subscription", AuthMode::Subscription),
    ("key", AuthMode::Key),
    ("gateway", AuthMode::Gateway),
];

/// Everything, checked.
pub async fn report(root: &Path, at_start: &[String], offline: bool) -> Report {
    let mut checks = Vec::new();
    let mut found = Found::default();
    let have_record = record(root, &mut checks);
    credential_names(root, at_start, &mut checks, &mut found);
    tap_credentials(root, at_start, &mut checks);
    logins(&mut checks, &mut found);
    harnesses(root, &mut checks, &mut found).await;
    copilot_token(&mut checks, &found);
    gateway(root, offline, have_record, &mut checks, &mut found).await;
    web(root, &mut checks).await;
    let ready = Harness::ALL
        .into_iter()
        .filter(|h| *h != Harness::Command)
        .map(|h| {
            let modes = MODES
                .iter()
                .map(|(name, mode)| ((*name).to_string(), ready(h, *mode, &found)))
                .collect();
            (h.binary().to_string(), modes)
        })
        .collect();
    Report {
        root: root.display().to_string(),
        checks,
        ready,
    }
}

fn print(r: &Report) {
    println!("amem doctor · {}", r.root);
    let mut area = "";
    for c in &r.checks {
        if c.area != area {
            area = c.area;
            println!("\n{area}");
        }
        println!("  {:<5} {:<20} {}", c.status.word(), c.subject, c.detail);
        if let Some(f) = &c.fix {
            println!("  {:<5} {:<20} {f}", "", "");
        }
    }
    println!("\nready to launch");
    println!("  {:<12} {:<24} {:<24} gateway", "", "subscription", "key");
    for (h, modes) in &r.ready {
        let cell = |m: &str| match modes.get(m) {
            Some(Ready { ok: true, .. }) => "yes".to_string(),
            Some(Ready {
                missing: Some(w), ..
            }) if w == "n/a" => w.clone(),
            Some(Ready {
                missing: Some(w), ..
            }) => format!("no: {w}"),
            _ => String::new(),
        };
        println!(
            "  {h:<12} {:<24} {:<24} {}",
            cell("subscription"),
            cell("key"),
            cell("gateway")
        );
    }
}

/// `amem doctor`.
pub async fn run(root: &Path, at_start: &[String], json: bool, offline: bool) -> Fallible {
    let r = report(root, at_start, offline).await;
    if json {
        println!("{}", serde_json::to_string_pretty(&r)?);
    } else {
        print(&r);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readiness_follows_what_each_mode_needs() {
        let f = Found {
            set: vec!["ANTHROPIC_API_KEY".into(), "LLMAO_KEY".into()],
            runnable: vec![
                Harness::Claude,
                Harness::Codex,
                Harness::Copilot,
                Harness::Qwen,
            ],
            logged_in: vec![Harness::Codex],
            github: false,
            gateway: Some(true),
        };
        let r = |h, a| ready(h, a, &f);
        assert!(r(Harness::Claude, AuthMode::Key).ok);
        assert_eq!(
            r(Harness::Claude, AuthMode::Subscription)
                .missing
                .as_deref(),
            Some("login")
        );
        assert!(r(Harness::Codex, AuthMode::Subscription).ok);
        assert_eq!(
            r(Harness::Codex, AuthMode::Key).missing.as_deref(),
            Some("OPENAI_API_KEY")
        );
        assert!(r(Harness::Codex, AuthMode::Gateway).ok);
        assert_eq!(
            r(Harness::Copilot, AuthMode::Subscription)
                .missing
                .as_deref(),
            Some("COPILOT_GITHUB_TOKEN, GH_TOKEN or gh login")
        );
        // a token for Copilot alone is enough
        let copilot_only = Found {
            set: vec!["COPILOT_GITHUB_TOKEN".into()],
            runnable: vec![Harness::Copilot],
            logged_in: Vec::new(),
            github: false,
            gateway: None,
        };
        assert!(ready(Harness::Copilot, AuthMode::Subscription, &copilot_only).ok);
        assert_eq!(
            r(Harness::Copilot, AuthMode::Key).missing.as_deref(),
            Some("n/a")
        );
        assert_eq!(
            r(Harness::Qwen, AuthMode::Subscription).missing.as_deref(),
            Some("n/a")
        );
        assert_eq!(
            r(Harness::Gemini, AuthMode::Key).missing.as_deref(),
            Some("gemini not installed")
        );
        let down = Found {
            gateway: Some(false),
            ..f
        };
        assert_eq!(
            ready(Harness::Claude, AuthMode::Gateway, &down)
                .missing
                .as_deref(),
            Some("gateway not answering")
        );
        // offline, the gateway was not asked: not a yes
        let unasked = Found {
            gateway: None,
            ..down
        };
        assert_eq!(
            ready(Harness::Claude, AuthMode::Gateway, &unasked)
                .missing
                .as_deref(),
            Some("gateway not asked")
        );
    }
}
