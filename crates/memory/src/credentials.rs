//! The credentials a launch reads, kept under the root in `credentials.env` so that
//! `amem` finds them without a wrapper sourcing a file first. The environment wins over
//! the file: a variable set for one run is a choice made for that run.
//!
//! The file names only the variables the launcher reads; any other line is reported and
//! skipped, so the file cannot become a way to set `PATH` or `LD_PRELOAD` for the
//! harness. On Unix a file that others can read is refused, as ssh refuses a key.

use std::path::{Path, PathBuf};

/// The file, relative to the root.
pub const FILE: &str = "credentials.env";

/// What the launcher reads, and what each opens.
pub const KNOWN: &[(&str, &str)] = &[
    ("ANTHROPIC_API_KEY", "--auth key to anthropic"),
    ("OPENAI_API_KEY", "--auth key to openai"),
    ("GEMINI_API_KEY", "--auth key to gemini"),
    ("XAI_API_KEY", "--auth key to xai"),
    ("DASHSCOPE_API_KEY", "--auth key to dashscope"),
    ("META_API_KEY", "--auth key to meta"),
    ("LLMAO_KEY", "--auth gateway"),
    (
        "LLMAO_URL",
        "the gateway's address: the local LiteLLM when unset, https://llm.apache.org for the Foundation's",
    ),
    (
        "GH_TOKEN",
        "copilot, on your GitHub login, when COPILOT_GITHUB_TOKEN is not set",
    ),
    (
        "COPILOT_GITHUB_TOKEN",
        "copilot, on a token with only the Copilot Requests permission",
    ),
    (
        "GOOGLE_CLOUD_PROJECT",
        "gemini on a Google login with a project of its own",
    ),
    (
        "LITELLM_MASTER_KEY",
        "--auth or --server-auth gateway on the wslc backend, which runs its own gateway",
    ),
    ("POSTGRES_PASSWORD", "that gateway's database"),
];

/// The key variable for a key-mode destination.
#[must_use]
pub fn key_var(destination: &str) -> Option<&'static str> {
    Some(match destination {
        "anthropic" => "ANTHROPIC_API_KEY",
        "openai" => "OPENAI_API_KEY",
        "gemini" => "GEMINI_API_KEY",
        "xai" => "XAI_API_KEY",
        "dashscope" => "DASHSCOPE_API_KEY",
        "meta" => "META_API_KEY",
        _ => return None,
    })
}

/// The file as read: the pairs it sets, and what was wrong with it.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Read {
    /// Known names and their values, in file order.
    pub pairs: Vec<(String, String)>,
    /// One line per problem, for the operator.
    pub problems: Vec<String>,
}

/// Parse `NAME=value` lines; `#` comments, blank lines, an `export ` prefix and one
/// level of matching quotes are allowed.
#[must_use]
pub fn parse(text: &str) -> Read {
    let mut r = Read::default();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((name, value)) = line.split_once('=') else {
            r.problems.push(format!("line {}: not NAME=value", n + 1));
            continue;
        };
        let name = name.trim();
        if !KNOWN.iter().any(|(k, _)| *k == name) {
            r.problems.push(format!(
                "line {}: {name} is not a credential amem reads; skipped",
                n + 1
            ));
            continue;
        }
        match value_of(value.trim()) {
            Ok(v) => r.pairs.push((name.to_string(), v.to_string())),
            Err(why) => r.problems.push(format!("line {}: {name}: {why}", n + 1)),
        }
    }
    r
}

/// A value as written: a quoted one runs to its closing quote, and only a comment may
/// follow it; an unquoted one ends at a ` #` comment.
fn value_of(value: &str) -> Result<&str, &'static str> {
    let Some(q) = value.chars().next().filter(|c| *c == '"' || *c == '\'') else {
        return Ok(value.split(" #").next().unwrap_or(value).trim_end());
    };
    let rest = &value[1..];
    let end = rest.find(q).ok_or("no closing quote")?;
    let after = rest[end + 1..].trim_start();
    if after.is_empty() || after.starts_with('#') {
        Ok(&rest[..end])
    } else {
        Err("something after the closing quote")
    }
}

/// Whether this platform's check can say who may read the file.
pub const CHECKS_PERMISSIONS: bool = cfg!(unix);

/// Whether others than the owner can read the file.
#[cfg(unix)]
fn too_open(path: &Path) -> std::io::Result<bool> {
    use std::os::unix::fs::PermissionsExt;
    Ok(std::fs::metadata(path)?.permissions().mode() & 0o077 != 0)
}

#[cfg(not(unix))]
fn too_open(_: &Path) -> std::io::Result<bool> {
    Ok(false)
}

/// The file under `root`: absent is not an error, readable by others is.
///
/// # Errors
///
/// When the file is readable by others, or cannot be read.
pub fn read(root: &Path) -> Result<Option<Read>, String> {
    let path = root.join(FILE);
    if !path.is_file() {
        return Ok(None);
    }
    if too_open(&path).map_err(|e| format!("{}: {e}", path.display()))? {
        return Err(format!(
            "{} is readable by others; run `chmod 600 {}` (on a Windows drive under WSL, which \
             keeps no mode bits, put the root on a Linux filesystem)",
            path.display(),
            path.display()
        ));
    }
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(Some(parse(&text)))
}

/// Set what the file names and the environment does not, at startup, before any other
/// thread exists. Problems are reported and do not stop the command.
pub fn load(root: &Path) {
    match read(root) {
        Ok(None) => {}
        Ok(Some(r)) => {
            for p in &r.problems {
                eprintln!("amem: {}: {p}", root.join(FILE).display());
            }
            for (k, v) in r.pairs {
                if present(std::env::var(&k).ok()).is_none() {
                    std::env::set_var(k, v);
                }
            }
        }
        Err(e) => eprintln!("amem: not loaded: {e}"),
    }
}

/// Where a variable's value comes from, for `doctor`: the environment, the file, or a
/// helper; never the value itself.
#[must_use]
pub fn source(name: &str, file: Option<&Read>, at_start: &[String]) -> Option<&'static str> {
    if at_start.iter().any(|n| n == name) {
        return Some("environment");
    }
    if file.is_some_and(|r| {
        r.pairs
            .iter()
            .any(|(k, v)| k == name && present(Some(v.clone())).is_some())
    }) {
        return Some(FILE);
    }
    None
}

/// The GitHub token: `GH_TOKEN`, else what `gh auth token` prints. The CLI keeps the
/// token in the system keyring, so asking it is how a login on this host is found.
#[must_use]
pub fn gh_token() -> Option<String> {
    if let Some(t) = std::env::var("GH_TOKEN")
        .ok()
        .filter(|t| !t.trim().is_empty())
    {
        return Some(t);
    }
    let out = std::process::Command::new("gh")
        .args(["auth", "token"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let t = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !t.is_empty()).then_some(t)
}

/// The token Copilot CLI runs on, and the variable it goes in: `COPILOT_GITHUB_TOKEN`,
/// a token that can make Copilot requests and nothing else, when it is set; else the
/// GitHub login, as [`gh_token`] finds it, which reaches every repository the login can.
#[must_use]
pub fn copilot_token() -> Option<(&'static str, String)> {
    copilot_choice(std::env::var("COPILOT_GITHUB_TOKEN").ok(), gh_token)
}

/// The choice [`copilot_token`] makes, from what it found: a Copilot token wins, and the
/// login is asked for only without one.
fn copilot_choice(
    copilot: Option<String>,
    login: impl FnOnce() -> Option<String>,
) -> Option<(&'static str, String)> {
    match present(copilot) {
        Some(t) => Some(("COPILOT_GITHUB_TOKEN", t)),
        None => login().map(|t| ("GH_TOKEN", t)),
    }
}

/// A value that is there: set, and not blank. The one test of "set" for every name.
#[must_use]
pub fn present(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.trim().is_empty())
}

/// The default root when neither `--root` nor `MEMORY_ROOT` names one: `~/memory`.
#[must_use]
pub fn default_root() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(|h| PathBuf::from(h).join("memory"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copilot_takes_its_own_token_over_the_login_and_asks_for_the_login_only_without_it() {
        let asked = std::cell::Cell::new(false);
        let login = || {
            asked.set(true);
            Some("gho_login".to_string())
        };
        assert_eq!(
            copilot_choice(Some("github_pat_copilot".into()), login),
            Some(("COPILOT_GITHUB_TOKEN", "github_pat_copilot".to_string()))
        );
        assert!(!asked.get(), "the login is not asked for");
        // a blank one is not one
        assert_eq!(
            copilot_choice(Some("  ".into()), || Some("gho_login".to_string())),
            Some(("GH_TOKEN", "gho_login".to_string()))
        );
        assert_eq!(copilot_choice(None, || None), None);
    }

    #[test]
    fn known_names_are_read_and_others_are_skipped_with_a_reason() {
        let r = parse(
            "# keys\n\nexport ANTHROPIC_API_KEY=\"sk-a # kept\"\nLLMAO_KEY='pat'\nGH_TOKEN=gho # mine\n\
             LD_PRELOAD=/tmp/x.so\nPATH=/evil\njunk\n",
        );
        assert_eq!(
            r.pairs,
            vec![
                ("ANTHROPIC_API_KEY".into(), "sk-a # kept".into()),
                ("LLMAO_KEY".into(), "pat".into()),
                ("GH_TOKEN".into(), "gho".into()),
            ]
        );
        assert_eq!(r.problems.len(), 3, "{:?}", r.problems);
        assert!(r.problems[0].contains("LD_PRELOAD"), "{:?}", r.problems);
        assert!(r.problems[2].contains("line 8"), "{:?}", r.problems);
    }

    #[test]
    fn quotes_and_comments_are_read_as_a_shell_would() {
        assert_eq!(value_of("sk-x"), Ok("sk-x"));
        assert_eq!(value_of("sk-x # mine"), Ok("sk-x"));
        assert_eq!(value_of("'sk-x' # mine"), Ok("sk-x"));
        assert_eq!(value_of("\"sk # x\""), Ok("sk # x"));
        assert_eq!(value_of("'sk-x"), Err("no closing quote"));
        assert_eq!(value_of("'sk'x"), Err("something after the closing quote"));
        let r = parse("OPENAI_API_KEY='sk-x' # mine\nXAI_API_KEY='open\n");
        assert_eq!(r.pairs, vec![("OPENAI_API_KEY".into(), "sk-x".into())]);
        assert!(
            r.problems[0].contains("no closing quote"),
            "{:?}",
            r.problems
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_file_others_can_read_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join(FILE);
        std::fs::write(&f, "OPENAI_API_KEY=k\n").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        let e = read(dir.path()).unwrap_err();
        assert!(e.contains("chmod 600"), "{e}");
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600)).unwrap();
        let r = read(dir.path()).unwrap().unwrap();
        assert_eq!(r.pairs, vec![("OPENAI_API_KEY".into(), "k".into())]);
        assert!(read(&dir.path().join("none")).unwrap().is_none());
    }

    #[test]
    fn every_key_destination_names_a_known_variable() {
        for d in ["anthropic", "openai", "gemini", "xai", "dashscope", "meta"] {
            let v = key_var(d).unwrap();
            assert!(KNOWN.iter().any(|(k, _)| *k == v), "{d}");
        }
        assert_eq!(key_var("llmao"), None);
    }

    #[test]
    fn the_environment_wins_over_the_file() {
        // POSTGRES_PASSWORD is read by no other test, so this one may set it
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join(FILE);
        std::fs::write(&f, "POSTGRES_PASSWORD=from-file\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        std::env::set_var("POSTGRES_PASSWORD", "from-environment");
        load(dir.path());
        assert_eq!(
            std::env::var("POSTGRES_PASSWORD").unwrap(),
            "from-environment"
        );
        std::env::remove_var("POSTGRES_PASSWORD");
        load(dir.path());
        assert_eq!(std::env::var("POSTGRES_PASSWORD").unwrap(), "from-file");
        std::env::remove_var("POSTGRES_PASSWORD");
    }
}
