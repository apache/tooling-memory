//! The macOS backend: Seatbelt, through `sandbox-exec`, with a profile made from the plan.
//!
//! Dry-coded. [`profile`] is a pure function from a [`Plan`] to the profile's text and is
//! tested on every platform; running a plan, [`run`] and [`run_captured`], is macOS only
//! and has not yet run on a Mac. What to iterate on there is the fixed part of the
//! profile: the system paths, devices and services a harness needs that this does not
//! grant, which a denial in the system log names.
//!
//! macOS has no mount namespace, so a Seatbelt plan is made with
//! [`Layout::seatbelt`](crate::Layout::seatbelt): everything at its host path, and each of
//! the plan's mounts a grant at that path. The profile denies by default, then allows:
//!
//! - processes: fork and exec, signals and process information within the sandbox,
//!   sysctl reads, pseudo-terminals, and a few named system services;
//! - a handful of devices, and the system read-only, [`SYSTEM`];
//! - the metadata, and only that, of each directory above a grant, so a path can be
//!   looked up through them; nothing in them can be listed or read;
//! - the harness's install, the memory binary and the projection, read-only;
//! - the private home and the working tree, read and write;
//! - the network: the egress socket, and the relay's port on loopback, and nothing else.
//!
//! The host's home is never granted, nor anything above it: a plan that would is refused.

use crate::{Backend, Error, Plan};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Where `sandbox-exec` is on every macOS.
pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// The system, read-only: what a binary needs to load and run. `/System` holds the dyld
/// shared cache on macOS 11 and later; `/private/var/db/dyld` held it before.
pub const SYSTEM: &[&str] = &[
    "/usr",
    "/bin",
    "/sbin",
    "/System",
    "/Library",
    "/private/etc",
    "/private/var/db/dyld",
    // zoneinfo, which /etc/localtime points into
    "/private/var/db/timezone",
];

/// The links at the root through which a path into the system is often named: `/etc`,
/// `/var` and `/tmp` point into `/private`. Their metadata is granted so they resolve.
const ROOT_LINKS: &[&str] = &["/etc", "/var", "/tmp"];

/// The system services a harness looks up by name: user and group lookups, the system
/// log, notifications, and certificate evaluation for a harness that speaks TLS through
/// the egress's tunnel.
const SERVICES: &[&str] = &[
    "com.apple.system.opendirectoryd.libinfo",
    "com.apple.system.logger",
    "com.apple.system.notification_center",
    "com.apple.trustd.agent",
];

/// What every profile starts with: deny by default, then processes and devices.
const HEADER: &str = r#"(version 1)
(deny default)

; processes: the relay, the harness, and whatever the harness runs
(allow process-fork)
(allow process-exec)
(allow signal (target same-sandbox))
(allow process-info* (target same-sandbox))
(allow sysctl-read)
(allow pseudo-tty)
"#;

/// The root directory's own listing: a process reads it as it starts on macOS 26, and is
/// aborted when it is refused, `/usr/bin/true` included. The names at the top of the disk
/// and nothing under them; the metadata rule grants `/` its attributes, not this.
const ROOT: &str = r#"
; the root's listing, which every process reads as it starts
(allow file-read-data (literal "/"))
"#;

/// Devices: the ones every process writes, the terminal the harness was given, and the
/// random ones read.
const DEVICES: &str = r#"
; devices
(allow file-read* file-write* file-ioctl
    (literal "/dev/null")
    (literal "/dev/zero")
    (literal "/dev/tty")
    (literal "/dev/ptmx")
    (subpath "/dev/fd")
    (regex #"^/dev/ttys[0-9]+$"))
(allow file-read*
    (literal "/dev/random")
    (literal "/dev/urandom"))
"#;

/// How a grant may be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Access {
    /// Read: the harness's install, the memory binary, the projection.
    Read,
    /// Read and write: the private home and the working tree.
    ReadWrite,
    /// Connect to: the egress socket.
    Connect,
}

/// One path the profile grants, and how.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Grant {
    path: PathBuf,
    access: Access,
    /// A file, granted as a `literal`; a directory is granted as a `subpath`.
    file: bool,
}

fn refused(why: impl std::fmt::Display) -> Error {
    Error::Unsupported(format!("seatbelt: {why}"))
}

/// The profile for a plan made with [`Layout::seatbelt`](crate::Layout::seatbelt), as the
/// text `sandbox-exec -p` takes. Pure: paths are taken as the plan gives them, and the
/// runner resolves macOS's links (`/tmp` to `/private/tmp`) before it calls this.
///
/// # Errors
///
/// A plan for another backend's layout, a mount whose inside differs from its host path,
/// a path that is not absolute, is not UTF-8, holds a control character or a `.` or `..`
/// component, a grant of the host's home or of anything above it, or a relay address
/// that is not a port on loopback.
pub fn profile(plan: &Plan) -> Result<String, Error> {
    render(
        &grants(plan)?,
        &plan.layout.relay,
        plan.host_home.as_deref(),
    )
}

/// The plan's mounts as grants: the egress socket to connect to, the memory binary a
/// file to read, and each other mount a directory, read-only or read-write as mounted.
fn grants(plan: &Plan) -> Result<Vec<Grant>, Error> {
    if plan.layout.backend != Backend::Seatbelt {
        return Err(refused(
            "the plan was made for the namespaces backend's layout",
        ));
    }
    plan.mounts
        .iter()
        .map(|m| {
            if m.source != m.target {
                return Err(refused(format!(
                    "{} would appear at {}; under Seatbelt a path is where it is on the host",
                    m.source.display(),
                    m.target.display()
                )));
            }
            let socket = m.target == plan.layout.socket;
            let access = match (socket, m.read_only) {
                (true, _) => Access::Connect,
                (false, true) => Access::Read,
                (false, false) => Access::ReadWrite,
            };
            Ok(Grant {
                path: m.target.clone(),
                access,
                file: socket || m.target == plan.layout.memory,
            })
        })
        .collect()
}

/// The profile's text for these grants.
fn render(grants: &[Grant], relay: &str, host_home: Option<&Path>) -> Result<String, Error> {
    let port = relay_port(relay)?;
    for g in grants {
        refuse_the_home(&g.path, g.file, host_home)?;
    }
    for s in SYSTEM {
        refuse_the_home(Path::new(s), false, host_home)?;
    }
    let mut out = String::from(HEADER);
    let services: Vec<String> = SERVICES
        .iter()
        .map(|s| format!("(global-name \"{s}\")"))
        .collect();
    rule(&mut out, None, "mach-lookup", &services);
    out.push_str(ROOT);
    out.push_str(DEVICES);
    let system = SYSTEM
        .iter()
        .map(|s| filter(Path::new(s), false))
        .collect::<Result<Vec<_>, _>>()?;
    rule(
        &mut out,
        Some("the system, read-only"),
        "file-read*",
        &system,
    );
    rule(
        &mut out,
        Some("the way to each grant, so a path can be looked up; metadata only"),
        "file-read-metadata",
        &metadata(grants)?,
    );
    let of = |access: Access| -> Result<Vec<String>, Error> {
        grants
            .iter()
            .filter(|g| g.access == access)
            .map(|g| filter(&g.path, g.file))
            .collect()
    };
    rule(
        &mut out,
        Some("the harness's install, the memory binary and the projection, read-only"),
        "file-read*",
        &of(Access::Read)?,
    );
    rule(
        &mut out,
        Some("the private home and the working tree"),
        "file-read* file-write*",
        &of(Access::ReadWrite)?,
    );
    let sockets = grants
        .iter()
        .filter(|g| g.access == Access::Connect)
        .map(|g| {
            Ok(format!(
                "(remote unix-socket (path-literal {}))",
                quoted(&g.path)?
            ))
        })
        .collect::<Result<Vec<_>, Error>>()?;
    let loopback = format!("\"localhost:{port}\"");
    rule(
        &mut out,
        Some("the one way out: the egress socket, and the relay's port on loopback"),
        "network-outbound",
        &sockets,
    );
    rule(
        &mut out,
        None,
        "network-bind network-inbound",
        &[format!("(local ip {loopback})")],
    );
    rule(
        &mut out,
        None,
        "network-outbound",
        &[format!("(remote ip {loopback})")],
    );
    Ok(out)
}

/// One `(allow ...)` rule with its filters, one to a line; nothing when there are none,
/// since a rule without a filter would allow the operation everywhere.
fn rule(out: &mut String, comment: Option<&str>, ops: &str, filters: &[String]) {
    if filters.is_empty() {
        return;
    }
    if let Some(c) = comment {
        out.push_str("\n; ");
        out.push_str(c);
        out.push('\n');
    }
    out.push_str("(allow ");
    out.push_str(ops);
    for f in filters {
        out.push_str("\n    ");
        out.push_str(f);
    }
    out.push_str(")\n");
}

/// A path as a filter: a `literal` for a file, a `subpath` for a directory.
fn filter(path: &Path, file: bool) -> Result<String, Error> {
    let kind = if file { "literal" } else { "subpath" };
    Ok(format!("({kind} {})", quoted(path)?))
}

/// A path as an SBPL string literal, with backslash and double quote escaped. A path
/// that is not absolute, not UTF-8, holds a control character, or walks with `.` or `..`
/// is refused: a grant names one place, as written.
fn quoted(path: &Path) -> Result<String, Error> {
    let text = path
        .to_str()
        .ok_or_else(|| refused(format!("{} is not UTF-8", path.display())))?;
    if !text.starts_with('/') {
        return Err(refused(format!("{text} is not an absolute path")));
    }
    if text.chars().any(char::is_control) {
        return Err(refused(format!("{text:?} holds a control character")));
    }
    if text.split('/').any(|c| c == "." || c == "..") {
        return Err(refused(format!("{text} walks with . or ..")));
    }
    Ok(format!(
        "\"{}\"",
        text.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

/// Refuse a grant that would hand over the host's home: the home itself, or a directory
/// above it, whose subpath would take it in.
fn refuse_the_home(path: &Path, file: bool, host_home: Option<&Path>) -> Result<(), Error> {
    let Some(home) = host_home else {
        return Ok(());
    };
    let takes_it = if file {
        path == home
    } else {
        home.starts_with(path)
    };
    if takes_it {
        return Err(refused(format!(
            "a grant of {} would hand the harness the host's home, {}",
            path.display(),
            home.display()
        )));
    }
    Ok(())
}

/// The directories above each grant and each system path, and the links at the root,
/// as `literal`s: what a lookup walks through. One a directory grant already covers is
/// left out, so each path appears in one rule.
fn metadata(grants: &[Grant]) -> Result<Vec<String>, Error> {
    let covering: Vec<&Path> = grants
        .iter()
        .filter(|g| !g.file && g.access != Access::Connect)
        .map(|g| g.path.as_path())
        .chain(SYSTEM.iter().map(Path::new))
        .collect();
    let mut above: BTreeSet<PathBuf> = ROOT_LINKS.iter().map(PathBuf::from).collect();
    for p in grants
        .iter()
        .map(|g| g.path.as_path())
        .chain(SYSTEM.iter().map(Path::new))
    {
        above.extend(p.ancestors().skip(1).map(Path::to_path_buf));
    }
    above
        .iter()
        .filter(|a| !covering.iter().any(|c| a.starts_with(c)))
        .map(|a| filter(a, true))
        .collect()
}

/// The relay's port, from its `host:port`, which must be on loopback: the relay listens
/// there and the harness connects there, and nothing else on the network is reachable.
fn relay_port(relay: &str) -> Result<u16, Error> {
    let not = || {
        refused(format!(
            "the relay's address {relay} is not a port on loopback"
        ))
    };
    let (host, port) = relay.rsplit_once(':').ok_or_else(not)?;
    if !matches!(host, "127.0.0.1" | "localhost") {
        return Err(not());
    }
    port.parse::<u16>().ok().filter(|p| *p != 0).ok_or_else(not)
}

/// A path with macOS's links resolved, as Seatbelt matches it: `/tmp/x` is
/// `/private/tmp/x`. A path that does not exist yet keeps its last component under its
/// resolved parent.
#[cfg(target_os = "macos")]
fn resolved(path: &Path) -> PathBuf {
    if let Ok(p) = std::fs::canonicalize(path) {
        return p;
    }
    match (path.parent(), path.file_name()) {
        (Some(dir), Some(name)) => resolved(dir).join(name),
        _ => path.to_path_buf(),
    }
}

/// `sandbox-exec -p <profile>` and the plan's command, with the plan's environment and
/// nothing inherited, in the plan's working directory.
#[cfg(target_os = "macos")]
fn command(plan: &Plan) -> Result<std::process::Command, Error> {
    let grants: Vec<Grant> = grants(plan)?
        .into_iter()
        .map(|g| Grant {
            path: resolved(&g.path),
            ..g
        })
        .collect();
    let home = plan.host_home.as_deref().map(resolved);
    let profile = render(&grants, &plan.layout.relay, home.as_deref())?;
    let mut cmd = std::process::Command::new(SANDBOX_EXEC);
    // the command's first word is an absolute path, so nothing after the profile is
    // taken for an option of sandbox-exec's
    cmd.arg("-p").arg(profile).args(&plan.argv);
    cmd.env_clear();
    cmd.envs(&plan.env);
    cmd.current_dir(&plan.cwd);
    Ok(cmd)
}

/// Run the plan under Seatbelt. Returns the exit code.
///
/// # Errors
///
/// When the profile cannot be made or `sandbox-exec` cannot be started.
#[cfg(target_os = "macos")]
pub fn run(plan: &Plan) -> Result<(i32, crate::Resources), Error> {
    let (code, _, r) = crate::measure::run(&mut command(plan)?, false)
        .map_err(|e| Error::Confine(format!("spawn {SANDBOX_EXEC}: {e}")))?;
    Ok((code, r))
}

/// As [`run`], with the harness's standard output captured; standard error is inherited.
///
/// # Errors
///
/// When the profile cannot be made or `sandbox-exec` cannot be started.
#[cfg(target_os = "macos")]
pub fn run_captured(plan: &Plan) -> Result<(i32, Vec<u8>, crate::Resources), Error> {
    crate::measure::run(&mut command(plan)?, true)
        .map_err(|e| Error::Confine(format!("spawn {SANDBOX_EXEC}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Layout, Mount};
    use dm_core::Label;
    use std::collections::BTreeMap;

    const HOME: &str = "/Users/me";
    const PRIVATE: &str = "/Users/me/memory/state/sandbox/claude";
    const WORK: &str = "/Users/me/src/project";
    const SOCKET: &str = "/Users/me/memory/state/run/egress-1.sock";
    const MEMORY: &str = "/Users/me/.cargo/bin/amem";
    const HARNESS: &str = "/opt/homebrew";
    const PROJECTION: &str = "/Users/me/memory/state/projection/claude/s1";

    fn mount(path: &str, read_only: bool) -> Mount {
        Mount {
            source: PathBuf::from(path),
            target: PathBuf::from(path),
            read_only,
        }
    }

    /// A Seatbelt plan as `prepare_in` makes one, built by hand so the test runs on
    /// every platform without a harness installed.
    fn plan_with(mounts: Vec<Mount>) -> Plan {
        Plan {
            mounts,
            env: BTreeMap::new(),
            argv: vec![MEMORY.into(), "relay".into()],
            cwd: PathBuf::from(WORK),
            hostname: "memory-sandbox".into(),
            handling_cap: Label::Public,
            links: Vec::new(),
            layout: Layout {
                backend: Backend::Seatbelt,
                home: PathBuf::from(PRIVATE),
                socket: PathBuf::from(SOCKET),
                memory: PathBuf::from(MEMORY),
                relay: "127.0.0.1:51234".into(),
                projection: Some(PathBuf::from(PROJECTION)),
                tmp: PathBuf::from(PRIVATE).join(".tmp"),
            },
            host_home: Some(PathBuf::from(HOME)),
        }
    }

    fn plan() -> Plan {
        plan_with(vec![
            mount(MEMORY, true),
            mount(HARNESS, true),
            mount(PRIVATE, false),
            mount(WORK, false),
            mount(PROJECTION, true),
            mount(SOCKET, false),
        ])
    }

    /// The profile's rules, as the operations each allows and the rule's text: a rule
    /// is a line that opens `(allow`, and the indented lines of filters after it.
    fn rules(profile: &str) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = Vec::new();
        for line in profile.lines() {
            if let Some(rest) = line.strip_prefix("(allow ") {
                let ops = rest.split('(').next().unwrap_or(rest);
                let ops = ops.trim().trim_end_matches(')').to_string();
                out.push((ops, line.to_string()));
            } else if line.starts_with("    ") {
                if let Some(last) = out.last_mut() {
                    last.1.push('\n');
                    last.1.push_str(line);
                }
            }
        }
        out
    }

    /// The rules that name `path`, as a literal or a subpath.
    fn naming(profile: &str, path: &str) -> Vec<(String, String)> {
        let lit = format!("(literal \"{path}\")");
        let sub = format!("(subpath \"{path}\")");
        let sock = format!("(path-literal \"{path}\")");
        rules(profile)
            .into_iter()
            .filter(|(_, body)| body.contains(&lit) || body.contains(&sub) || body.contains(&sock))
            .collect()
    }

    #[test]
    fn the_profile_denies_by_default() {
        let p = profile(&plan()).unwrap();
        assert!(p.starts_with("(version 1)\n(deny default)\n"), "{p}");
        assert_eq!(p.matches("(deny default)").count(), 1);
        // an allow with no filter would allow its operation everywhere; only the fixed
        // process rules, which name no path, stand alone
        for (ops, body) in rules(&p) {
            let filters = &body["(allow ".len() + ops.len()..];
            if !filters.contains('(') {
                assert!(
                    ["process-fork", "process-exec", "sysctl-read", "pseudo-tty"]
                        .contains(&ops.as_str()),
                    "{ops} is allowed everywhere"
                );
            }
        }
    }

    #[test]
    fn each_grant_appears_once_with_its_operation() {
        let p = profile(&plan()).unwrap();
        let cases = [
            (MEMORY, "file-read*", "literal"),
            (HARNESS, "file-read*", "subpath"),
            (PROJECTION, "file-read*", "subpath"),
            (PRIVATE, "file-read* file-write*", "subpath"),
            (WORK, "file-read* file-write*", "subpath"),
            (SOCKET, "network-outbound", "path-literal"),
        ];
        for (path, ops, kind) in cases {
            let found = naming(&p, path);
            assert_eq!(found.len(), 1, "{path} in {} rules:\n{p}", found.len());
            assert_eq!(found[0].0, ops, "{path}:\n{p}");
            assert_eq!(
                p.matches(&format!("\"{path}\"")).count(),
                1,
                "{path} named more than once:\n{p}"
            );
            assert!(
                found[0].1.contains(&format!("({kind} \"{path}\")")),
                "{path} as {kind}:\n{p}"
            );
        }
        // the system is read-only, and the read-write rule holds the two grants alone
        for s in SYSTEM {
            let found = naming(&p, s);
            assert_eq!(found.len(), 1, "{s}:\n{p}");
            assert_eq!(found[0].0, "file-read*", "{s}");
        }
        let rw: Vec<_> = rules(&p)
            .into_iter()
            .filter(|(ops, _)| ops.contains("file-write*"))
            .collect();
        assert_eq!(rw.len(), 2, "devices and the grants:\n{p}");
        assert!(rw[1].1.contains(PRIVATE) && rw[1].1.contains(WORK));
        assert_eq!(rw[1].1.matches("(subpath").count(), 2, "{}", rw[1].1);
    }

    #[test]
    fn a_path_is_escaped_for_a_string_literal() {
        let odd = r#"/Users/me/src/we"ird\dir"#;
        let p = profile(&plan_with(vec![mount(odd, false)])).unwrap();
        assert!(
            p.contains(r#"(subpath "/Users/me/src/we\"ird\\dir")"#),
            "{p}"
        );
        assert!(!p.contains(odd), "unescaped:\n{p}");
    }

    #[test]
    fn a_path_that_is_not_absolute_is_refused() {
        for bad in [
            "src/project",
            "./src",
            "/Users/me/src/../..",
            "/Users/me/./x",
        ] {
            let e = profile(&plan_with(vec![mount(bad, false)])).unwrap_err();
            assert!(e.to_string().contains("seatbelt"), "{bad}: {e}");
        }
        let e = profile(&plan_with(vec![mount("/Users/me/a\nb", false)])).unwrap_err();
        assert!(e.to_string().contains("control"), "{e}");
    }

    #[test]
    fn nothing_but_the_socket_and_the_relay_port_reaches_the_network() {
        let p = profile(&plan()).unwrap();
        let net: Vec<_> = rules(&p)
            .into_iter()
            .filter(|(ops, _)| ops.contains("network"))
            .collect();
        assert_eq!(net.len(), 3, "{p}");
        let mut filters = Vec::new();
        for (_, body) in &net {
            filters.extend(body.lines().skip(1).map(str::trim));
        }
        assert_eq!(
            filters,
            [
                format!("(remote unix-socket (path-literal \"{SOCKET}\")))"),
                "(local ip \"localhost:51234\"))".to_string(),
                "(remote ip \"localhost:51234\"))".to_string(),
            ]
        );
        assert_eq!(p.matches("network").count(), 4, "{p}");
        // a relay anywhere but loopback is refused
        for relay in [
            "0.0.0.0:4100",
            "example.com:443",
            "127.0.0.1",
            "127.0.0.1:0",
        ] {
            let mut bad = plan();
            bad.layout.relay = relay.into();
            assert!(profile(&bad).is_err(), "{relay}");
        }
    }

    #[test]
    fn the_host_home_is_never_granted() {
        let p = profile(&plan()).unwrap();
        // only its metadata, as a directory on the way to the working tree
        for (ops, body) in rules(&p) {
            for named in [
                format!("(subpath \"{HOME}\")"),
                "(subpath \"/Users\")".to_string(),
                "(subpath \"/\")".to_string(),
            ] {
                assert!(!body.contains(&named), "{ops} grants {named}");
            }
            if ops != "file-read-metadata" {
                assert!(!body.contains(&format!("\"{HOME}\"")), "{ops}:\n{body}");
            }
        }
        // a grant of the home, or of anything above it, is refused
        for (path, file) in [(HOME, false), ("/Users", false), ("/", false), (HOME, true)] {
            let mut m = mount(path, false);
            let mut bad = plan_with(Vec::new());
            if file {
                // the home as the memory binary's literal
                bad.layout.memory = PathBuf::from(path);
                m.read_only = true;
            }
            bad.mounts.push(m);
            let e = profile(&bad).unwrap_err();
            assert!(e.to_string().contains("host's home"), "{path}: {e}");
        }
        // and so is a system path, when the home is under it
        let mut bad = plan();
        bad.host_home = Some(PathBuf::from("/usr/home/me"));
        assert!(profile(&bad).unwrap_err().to_string().contains("/usr"));
    }

    #[test]
    fn the_root_is_listed_and_nothing_under_it_is_read() {
        let p = profile(&plan()).unwrap();
        let root = naming(&p, "/");
        assert_eq!(
            root.iter()
                .filter(|(ops, _)| ops != "file-read-metadata")
                .map(|(ops, body)| (ops.as_str(), body.as_str()))
                .collect::<Vec<_>>(),
            [("file-read-data", "(allow file-read-data (literal \"/\"))")],
            "{p}"
        );
        assert!(!p.contains("(subpath \"/\")"), "{p}");
    }

    #[test]
    fn a_plan_for_the_namespaces_layout_is_refused() {
        let mut p = plan();
        p.layout.backend = Backend::Namespaces;
        assert!(profile(&p).is_err());
        let mut p = plan();
        p.mounts.push(Mount {
            source: PathBuf::from(WORK),
            target: PathBuf::from("/work"),
            read_only: false,
        });
        assert!(profile(&p).unwrap_err().to_string().contains("/work"));
    }

    #[test]
    fn the_way_to_a_grant_is_metadata_only() {
        let p = profile(&plan()).unwrap();
        let meta: Vec<_> = rules(&p)
            .into_iter()
            .filter(|(ops, _)| ops == "file-read-metadata")
            .collect();
        assert_eq!(meta.len(), 1, "{p}");
        let body = &meta[0].1;
        for above in [
            "/",
            "/Users",
            HOME,
            "/Users/me/src",
            "/private",
            "/etc",
            "/tmp",
        ] {
            assert!(
                body.contains(&format!("(literal \"{above}\")")),
                "{above}:\n{body}"
            );
        }
        // never a subpath, and nothing a grant already covers
        assert!(!body.contains("(subpath"), "{body}");
        assert!(!body.contains(&format!("\"{WORK}\"")), "{body}");
        assert!(!body.contains("\"/usr\""), "{body}");
        assert!(body.contains("(literal \"/private/var/db\")"), "{body}");
    }
}

#[cfg(all(test, target_os = "macos"))]
mod on_a_mac {
    use super::*;
    use crate::{Layout, Mount};
    use dm_core::Label;
    use std::collections::BTreeMap;

    /// A plan that runs `argv` with one directory granted read-write.
    fn plan(granted: &Path, argv: &[&str]) -> Plan {
        Plan {
            mounts: vec![Mount {
                source: granted.to_path_buf(),
                target: granted.to_path_buf(),
                read_only: false,
            }],
            env: BTreeMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]),
            argv: argv.iter().map(ToString::to_string).collect(),
            cwd: granted.to_path_buf(),
            hostname: "memory-sandbox".into(),
            handling_cap: Label::Public,
            links: Vec::new(),
            layout: Layout {
                backend: Backend::Seatbelt,
                home: granted.to_path_buf(),
                socket: granted.join("egress.sock"),
                memory: granted.join("memory"),
                relay: "127.0.0.1:4100".into(),
                projection: None,
                tmp: granted.join(".tmp"),
            },
            host_home: None,
        }
    }

    #[test]
    fn a_confined_process_starts_and_its_own_exit_code_comes_back() {
        let inside = tempfile::tempdir().unwrap();
        let (code, out, _) = run_captured(&plan(inside.path(), &["/usr/bin/true"])).unwrap();
        assert_eq!(code, 0, "{}", String::from_utf8_lossy(&out));
        // a code of its own, which a death by signal, reported as 1, cannot pass for
        let (code, out, _) = run_captured(&plan(
            inside.path(),
            &["/bin/sh", "-c", "echo started; exit 7"],
        ))
        .unwrap();
        assert_eq!((code, out.as_slice()), (7, b"started\n".as_slice()));
    }

    #[test]
    fn a_confined_cat_reads_inside_the_grant_and_not_outside() {
        let inside = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(inside.path().join("in.txt"), "granted").unwrap();
        std::fs::write(outside.path().join("out.txt"), "secret").unwrap();
        let read = |file: &Path| {
            let f = file.display().to_string();
            let (code, out, _) = run_captured(&plan(inside.path(), &["/bin/cat", &f])).unwrap();
            (code, out)
        };
        let (code, out) = read(&inside.path().join("in.txt"));
        assert_eq!((code, out.as_slice()), (0, b"granted".as_slice()));
        let (code, out) = read(&outside.path().join("out.txt"));
        assert_ne!(code, 0);
        assert!(out.is_empty());
    }

    #[test]
    fn a_confined_write_lands_inside_the_grant_and_not_outside() {
        let inside = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let touch = |file: &Path| {
            let f = file.display().to_string();
            run_captured(&plan(inside.path(), &["/usr/bin/touch", &f]))
                .unwrap()
                .0
        };
        assert_eq!(touch(&inside.path().join("new")), 0);
        assert!(inside.path().join("new").exists());
        assert_ne!(touch(&outside.path().join("new")), 0);
        assert!(!outside.path().join("new").exists());
    }
}
