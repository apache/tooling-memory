//! Profiles: a name for the flags a launch is given, kept in `profiles.toml` under the
//! root, so `amem claude` or `amem work nightly` is the whole command.
//!
//! ```toml
//! [nightly]
//! harness = "codex"
//! auth = "gateway"
//! model = "qwen3.8-27b"
//! subagents = "board"
//! workdir = "/home/me/src/project"
//! ```
//!
//! A profile becomes the flags it names, put in front of the command line before it is
//! parsed, and only for the flags the command line does not give itself: what is typed
//! wins over what is written down. Every harness is a profile of its own name with
//! nothing else set, so `amem claude` works with no file at all.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;

/// The file, relative to the root.
pub const FILE: &str = "profiles.toml";

/// One profile: each field is the launch flag of the same name.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// `--harness`.
    pub harness: String,
    /// `--auth`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<String>,
    /// `--provider`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// `--model`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// `--server-auth`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_auth: Option<String>,
    /// `--server-provider`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_provider: Option<String>,
    /// `--subagents`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagents: Option<String>,
    /// `--backend`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    /// `--workdir`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workdir: Option<String>,
}

impl Profile {
    /// The flags, in order.
    fn flags(&self) -> Vec<(&'static str, &str)> {
        let mut v = vec![("--harness", self.harness.as_str())];
        for (flag, value) in [
            ("--auth", &self.auth),
            ("--provider", &self.provider),
            ("--model", &self.model),
            ("--server-auth", &self.server_auth),
            ("--server-provider", &self.server_provider),
            ("--subagents", &self.subagents),
            ("--backend", &self.backend),
            ("--workdir", &self.workdir),
        ] {
            if let Some(v2) = value {
                v.push((flag, v2.as_str()));
            }
        }
        v
    }
}

/// The harnesses, each a profile of its own name.
pub const HARNESSES: &[&str] = &[
    "claude",
    "codex",
    "gemini",
    "antigravity",
    "qwen",
    "grok",
    "muse",
    "pi",
    "copilot",
    "opencode",
];

/// Whether `name` may name a profile: what a person types, and nothing a path or a flag
/// could be made of.
#[must_use]
pub fn valid_name(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// Every harness as a profile of its own name.
#[must_use]
pub fn builtin() -> BTreeMap<String, Profile> {
    HARNESSES
        .iter()
        .map(|h| {
            (
                (*h).to_string(),
                Profile {
                    harness: (*h).to_string(),
                    ..Profile::default()
                },
            )
        })
        .collect()
}

/// The profiles under `root`: the file's, and the harnesses' own where the file does not
/// name them.
///
/// # Errors
///
/// When the file is there and does not parse, or names a profile badly.
pub fn load(root: &Path) -> Result<BTreeMap<String, Profile>, String> {
    let mut all = builtin();
    let path = root.join(FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(all),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let named: BTreeMap<String, Profile> =
        toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    for (name, p) in named {
        if !valid_name(&name) {
            return Err(format!(
                "{}: `{name}` is not a profile name: lowercase letters, digits, - and _, \
                 starting with a letter",
                path.display()
            ));
        }
        all.insert(name, p);
    }
    Ok(all)
}

/// `amem profiles`: a line each, or the whole as JSON.
///
/// # Errors
///
/// When the JSON cannot be written.
pub fn show(
    all: &BTreeMap<String, Profile>,
    as_json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if as_json {
        println!("{}", serde_json::to_string_pretty(all)?);
        return Ok(());
    }
    for (name, p) in all {
        let rest: Vec<String> = p
            .flags()
            .into_iter()
            .skip(1)
            .map(|(flag, value)| format!("{flag} {value}"))
            .collect();
        println!("{name:<16} {:<12} {}", p.harness, rest.join(" "));
    }
    Ok(())
}

/// Whether a flag of a command takes the next word as its value; `--flag=value` and a
/// flag that takes none consume no word.
pub type TakesValue<'a> = &'a dyn Fn(&str) -> bool;

/// Whether `args` gives `flag`, as `--flag value` or `--flag=value`, among the command's
/// own flags: up to `--`, or the first word that is not a flag, after which everything
/// is the harness's.
fn gives(args: &[OsString], flag: &str, takes_value: TakesValue<'_>) -> bool {
    let eq = format!("{flag}=");
    let mut i = 0;
    while let Some(a) = args.get(i).and_then(|a| a.to_str()) {
        if a == "--" || !a.starts_with('-') {
            return false;
        }
        if a == flag || a.starts_with(&eq) {
            return true;
        }
        i += if !a.contains('=') && takes_value(a) {
            2
        } else {
            1
        };
    }
    false
}

/// Put `profile`'s flags in front of `rest`, skipping those `rest` gives itself.
fn with_profile(
    profile: &Profile,
    rest: &[OsString],
    takes_value: TakesValue<'_>,
) -> Vec<OsString> {
    let mut out = Vec::new();
    for (flag, value) in profile.flags() {
        if !gives(rest, flag, takes_value) {
            out.push(OsString::from(flag));
            out.push(OsString::from(value));
        }
    }
    out.extend(rest.iter().cloned());
    out
}

/// Take `--profile <name>` (or `--profile=<name>`) out of `args`, before any `--`.
fn take_profile_flag(args: &mut Vec<OsString>) -> Option<String> {
    let end = args.iter().position(|a| a == "--").unwrap_or(args.len());
    for i in 0..end {
        if args[i] == "--profile" && i + 1 < end {
            let name = args.remove(i + 1).to_string_lossy().into_owned();
            args.remove(i);
            return Some(name);
        }
        if let Some(name) = args[i].to_str().and_then(|s| s.strip_prefix("--profile=")) {
            let name = name.to_string();
            args.remove(i);
            return Some(name);
        }
    }
    None
}

fn find<'a>(profiles: &'a BTreeMap<String, Profile>, name: &str) -> Result<&'a Profile, String> {
    profiles.get(name).ok_or_else(|| {
        format!(
            "no command or profile named `{name}`; profiles: {}",
            profiles.keys().cloned().collect::<Vec<_>>().join(", ")
        )
    })
}

/// Where the command word is: after the program and any global `--root`.
fn command_at(argv: &[OsString]) -> usize {
    let mut i = 1;
    while i < argv.len() {
        if argv[i] == "--root" {
            i += 2;
        } else if argv[i].to_str().is_some_and(|s| s.starts_with("--root=")) {
            i += 1;
        } else {
            break;
        }
    }
    i
}

/// The command line with any profile it names spelled out as flags: `amem <name> ...`
/// is `amem launch <flags> ...`, and `launch` and `work` take `--profile <name>`, `work`
/// its name as a word too. `commands` are the words that are commands, not profiles.
///
/// # Errors
///
/// When a profile is named that is not one, or the file does not parse.
pub fn expand(
    argv: Vec<OsString>,
    commands: &[String],
    takes_value: &dyn Fn(&str, &str) -> bool,
    profiles: impl FnOnce() -> Result<BTreeMap<String, Profile>, String>,
) -> Result<Vec<OsString>, String> {
    let at = command_at(&argv);
    let Some(word) = argv.get(at).and_then(|w| w.to_str()).map(str::to_string) else {
        return Ok(argv);
    };
    if word.starts_with('-') {
        return Ok(argv);
    }
    let (head, rest) = argv.split_at(at + 1);
    let mut rest = rest.to_vec();
    if !commands.contains(&word) {
        // a word that is no command is a profile, launched
        let all = profiles()?;
        let p = find(&all, &word)?;
        let mut out = head[..at].to_vec();
        out.push("launch".into());
        out.extend(with_profile(p, &rest, &|f| takes_value("launch", f)));
        return Ok(out);
    }
    if word != "launch" && word != "work" {
        return Ok(argv);
    }
    let named = take_profile_flag(&mut rest).or_else(|| {
        // `work nightly`: work takes no word of its own, so a first word is a profile
        (word == "work"
            && rest
                .first()
                .is_some_and(|a| !a.to_string_lossy().starts_with('-')))
        .then(|| rest.remove(0).to_string_lossy().into_owned())
    });
    let Some(name) = named else {
        return Ok(argv);
    };
    let all = profiles()?;
    let p = find(&all, &name)?;
    let mut out = head.to_vec();
    out.extend(with_profile(p, &rest, &|f| takes_value(&word, f)));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    fn strs(v: &[OsString]) -> Vec<String> {
        v.iter().map(|a| a.to_string_lossy().into_owned()).collect()
    }

    /// As clap has it for launch and work: these take no value.
    fn takes(_: &str, flag: &str) -> bool {
        !matches!(
            flag,
            "--unconfined" | "--loop" | "--no-handoff" | "--mine" | "-h" | "--help"
        )
    }

    fn commands() -> Vec<String> {
        ["launch", "work", "web", "doctor", "init", "profiles"]
            .iter()
            .map(|s| (*s).to_string())
            .collect()
    }

    fn sample() -> BTreeMap<String, Profile> {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(FILE),
            "[nightly]\nharness = \"codex\"\nauth = \"gateway\"\nmodel = \"m1\"\nsubagents = \"board\"\n",
        )
        .unwrap();
        load(dir.path()).unwrap()
    }

    #[test]
    fn a_harness_is_a_profile_and_the_file_adds_more() {
        let all = sample();
        assert_eq!(all["claude"].harness, "claude");
        assert_eq!(all["claude"].auth, None);
        assert_eq!(all["nightly"].model.as_deref(), Some("m1"));
        assert!(all.contains_key("pi"));
    }

    #[test]
    fn a_profile_name_is_launched_and_what_is_typed_wins() {
        let out = expand(
            os(&["amem", "nightly", "--model", "m2", "--", "--resume"]),
            &commands(),
            &takes,
            || Ok(sample()),
        )
        .unwrap();
        assert_eq!(
            strs(&out),
            [
                "amem",
                "launch",
                "--harness",
                "codex",
                "--auth",
                "gateway",
                "--subagents",
                "board",
                "--model",
                "m2",
                "--",
                "--resume"
            ]
        );
        let out = expand(
            os(&["amem", "--root", "/r", "claude"]),
            &commands(),
            &takes,
            || Ok(sample()),
        )
        .unwrap();
        assert_eq!(
            strs(&out),
            ["amem", "--root", "/r", "launch", "--harness", "claude"]
        );
    }

    #[test]
    fn the_typed_flag_is_found_as_written_and_the_harness_words_are_not_flags() {
        let run =
            |argv: &[&str]| strs(&expand(os(argv), &commands(), &takes, || Ok(sample())).unwrap());
        // `--model=m2` is the flag too
        assert!(!run(&["amem", "nightly", "--model=m2"]).contains(&"m1".to_string()));
        // after `--`, or the first word that is not a flag, the words are the harness's
        for argv in [
            &["amem", "nightly", "--", "--model", "x"][..],
            &["amem", "nightly", "--unconfined", "fix", "--model", "x"],
        ] {
            let out = run(argv);
            assert!(out.windows(2).any(|w| w == ["--model", "m1"]), "{out:?}");
        }
        // a flag that takes a value consumes it: `--prompt --model` is a prompt
        let out = run(&["amem", "nightly", "--prompt", "--model"]);
        assert!(out.windows(2).any(|w| w == ["--model", "m1"]), "{out:?}");
    }

    #[test]
    fn launch_and_work_take_a_profile_and_work_takes_it_as_a_word() {
        let out = expand(
            os(&["amem", "work", "nightly", "--loop"]),
            &commands(),
            &takes,
            || Ok(sample()),
        )
        .unwrap();
        assert_eq!(
            strs(&out),
            [
                "amem",
                "work",
                "--harness",
                "codex",
                "--auth",
                "gateway",
                "--model",
                "m1",
                "--subagents",
                "board",
                "--loop"
            ]
        );
        let out = expand(
            os(&["amem", "launch", "--profile=nightly", "--auth", "key"]),
            &commands(),
            &takes,
            || Ok(sample()),
        )
        .unwrap();
        assert_eq!(
            strs(&out),
            [
                "amem",
                "launch",
                "--harness",
                "codex",
                "--model",
                "m1",
                "--subagents",
                "board",
                "--auth",
                "key"
            ]
        );
    }

    #[test]
    fn commands_and_flags_pass_through_untouched_and_the_file_is_not_read() {
        for argv in [
            &["amem", "doctor"][..],
            &["amem"],
            &["amem", "--no-open"],
            &["amem", "launch", "--harness", "claude"],
        ] {
            let out = expand(os(argv), &commands(), &takes, || Err("read".into())).unwrap();
            assert_eq!(strs(&out), argv);
        }
    }

    #[test]
    fn an_unknown_name_says_what_there_is() {
        let e = expand(os(&["amem", "nope"]), &commands(), &takes, || Ok(sample())).unwrap_err();
        assert!(
            e.contains("no command or profile named `nope`") && e.contains("nightly"),
            "{e}"
        );
    }

    #[test]
    fn a_bad_file_or_name_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE), "[Bad]\nharness = \"codex\"\n").unwrap();
        assert!(load(dir.path()).unwrap_err().contains("not a profile name"));
        std::fs::write(
            dir.path().join(FILE),
            "[ok]\nharness = \"codex\"\ncolour = \"red\"\n",
        )
        .unwrap();
        assert!(load(dir.path()).unwrap_err().contains("colour"));
        assert!(
            valid_name("night-1") && !valid_name("../x") && !valid_name("1x") && !valid_name("")
        );
    }
}
