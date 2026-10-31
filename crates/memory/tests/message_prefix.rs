//! The binary's own messages start with `amem:`, the command's name, and none starts
//! with `memory:`, the name it had before. The demo selects the lines of `amem work` by
//! that prefix, so a message under the old name is a line the demo drops without a word.
//! This reads the sources, since no run reaches every message. It checks that the old
//! name is gone, not that every line carries the new one: `relay:`, `error:` and the
//! like are prefixes of their own.

use std::path::Path;

fn sources(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("crates/ reads") {
        let path = entry.expect("an entry").path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n != "target") {
                sources(&path, out);
            }
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Where a message under the old name starts, each as it is spelled in a source line.
fn old_spellings(name: &str) -> Vec<String> {
    // after a quote, a backtick, a byte string, or a newline escape; and what follows
    // the colon is a space, a tab, a placeholder, a line continuation, or the string's end
    let starts = ["\"", "'", "`", "b\"", "\\n"];
    let follows = [" ", "\\t", "{", "\\", "\""];
    let mut old = Vec::new();
    for s in starts {
        for f in follows {
            old.push(format!("{s}{name}:{f}"));
        }
        old.push(format!("{s}{name} hook:"));
    }
    old
}

/// The lines of `text` that start a message under the old name. A comment is not a
/// message, and a line that says it holds the old name on purpose is let through.
fn under_the_old_name(text: &str) -> Vec<(usize, String)> {
    // assembled, so this file does not hold what it looks for
    let name = ["mem", "ory"].concat();
    let allowed = ["old name", " allowed"].concat();
    let old = old_spellings(&name);
    let mut found = Vec::new();
    let mut continues = false;
    for (n, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        let comment = trimmed.starts_with("//");
        let held = old.iter().any(|o| line.contains(o.as_str()))
            || (continues && trimmed.starts_with(&format!("{name}:")));
        if held && !comment && !line.contains(&allowed) {
            found.push((n + 1, trimmed.to_string()));
        }
        continues = !comment && line.ends_with('\\');
    }
    found
}

#[test]
fn every_spelling_of_the_old_name_is_found_and_only_those() {
    let name = ["mem", "ory"].concat();
    let caught = [
        format!("eprintln!(\"{name}: x\");"),
        format!("eprintln!(\"{name}:{{e}}\");"),
        format!("eprintln!(\"{name}:\\tx\");"),
        format!("eprintln!(\"\\n{name}: after a blank line\");"),
        format!("let js = r#\"console.warn('{name}: ' + e);\"#;"),
        format!("let b = b\"{name}: sandbox: \";"),
        format!("eprintln!(\"{name} hook: x\");"),
    ];
    for line in &caught {
        assert_eq!(under_the_old_name(line).len(), 1, "missed: {line}");
    }
    let continued = format!("eprintln!(\"first line\\n\\\n     {name}: second line\");");
    assert_eq!(
        under_the_old_name(&continued),
        vec![(2, format!("{name}: second line\");"))]
    );
    let passed = [
        format!("/// the `{name}: ` key"),
        format!("// \"{name}: x\""),
        format!("let limits = \"{name}: 1Gi\"; // old name allowed: a pod limit"),
        format!("{name}: Arc<Memory>,"),
        "eprintln!(\"amem: x\");".to_string(),
    ];
    for line in &passed {
        assert!(under_the_old_name(line).is_empty(), "flagged: {line}");
    }
}

#[test]
fn no_message_starts_with_the_old_name() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut files = Vec::new();
    sources(&crates, &mut files);
    assert!(
        files.len() > 50,
        "found only {} sources under {}",
        files.len(),
        crates.display()
    );
    let mut found = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file).expect("a source reads");
        for (n, line) in under_the_old_name(&text) {
            found.push(format!("{}:{n}: {line}", file.display()));
        }
    }
    assert!(
        found.is_empty(),
        "messages under the old name:\n{}",
        found.join("\n")
    );
}
