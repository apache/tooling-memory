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

/// `text` with every comment blanked and every line break kept, so a line number still
/// points at its source line. A literal is kept whole: `//` or `/*` inside a string or
/// a character opens no comment.
fn without_comments(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let rest = &chars[i..];
        if let Some(n) = comment_len(rest) {
            out.extend(rest[..n].iter().map(|&c| if c == '\n' { c } else { ' ' }));
            i += n;
        } else {
            let n = literal_len(rest).unwrap_or(1);
            out.extend(&rest[..n]);
            i += n;
        }
    }
    out
}

/// The length of the comment that `rest` starts with, if it starts with one. Block
/// comments nest.
fn comment_len(rest: &[char]) -> Option<usize> {
    if rest.starts_with(&['/', '/']) {
        return Some(rest.iter().position(|&c| c == '\n').unwrap_or(rest.len()));
    }
    if !rest.starts_with(&['/', '*']) {
        return None;
    }
    let mut depth = 0;
    let mut i = 0;
    while i + 1 < rest.len() {
        match (rest[i], rest[i + 1]) {
            ('/', '*') => depth += 1,
            ('*', '/') => depth -= 1,
            _ => {
                i += 1;
                continue;
            }
        }
        i += 2;
        if depth == 0 {
            return Some(i);
        }
    }
    Some(rest.len())
}

/// The length of the string or character literal that `rest` starts with, if it starts
/// with one. A quote that no character closes is a lifetime.
fn literal_len(rest: &[char]) -> Option<usize> {
    let up_to = |close: &[char], from: usize| {
        rest[from..]
            .windows(close.len())
            .position(|w| w == close)
            .map_or(rest.len(), |p| from + p + close.len())
    };
    match rest {
        ['"', ..] => {
            let mut i = 1;
            while i < rest.len() && rest[i] != '"' {
                i += if rest[i] == '\\' { 2 } else { 1 };
            }
            Some((i + 1).min(rest.len()))
        }
        ['r', tail @ ..] => {
            let hashes = tail.iter().take_while(|&&c| c == '#').count();
            let close: Vec<char> = std::iter::once('"')
                .chain(std::iter::repeat_n('#', hashes))
                .collect();
            (tail.get(hashes) == Some(&'"')).then(|| up_to(&close, hashes + 2))
        }
        ['\'', '\\', ..] => Some(up_to(&['\''], 3)),
        ['\'', _, '\'', ..] => Some(3),
        _ => None,
    }
}

/// The lines of `text` that start a message under the old name. A comment is not a
/// message, and a line that says it holds the old name on purpose is let through.
fn under_the_old_name(text: &str) -> Vec<(usize, String)> {
    // assembled, so this file does not hold what it looks for
    let name = ["mem", "ory"].concat();
    let allowed = ["old name", " allowed"].concat();
    let old = old_spellings(&name);
    let code = without_comments(text);
    let mut found = Vec::new();
    let mut continues = false;
    for (n, (line, source)) in code.lines().zip(text.lines()).enumerate() {
        let trimmed = line.trim_start();
        // a continuation skips blank lines as it skips indentation
        if trimmed.is_empty() {
            continue;
        }
        let held = old.iter().any(|o| line.contains(o.as_str()))
            || (continues && trimmed.starts_with(&format!("{name}:")));
        if held && !source.contains(&allowed) {
            found.push((n + 1, source.trim_start().to_string()));
        }
        continues = line.ends_with('\\');
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
        // what opens a comment inside a literal opens none
        format!("let s = \"//\"; eprintln!(\"{name}: x\");"),
        format!("let s = \"/*\"; eprintln!(\"{name}: x\"); // */"),
        format!("let s = \"a \\\" // b\"; eprintln!(\"{name}: x\");"),
        format!("let s = r#\"a \" // \"#; eprintln!(\"{name}: x\");"),
        format!("let q = '\"'; let s = \"//\"; eprintln!(\"{name}: x\");"),
        format!("let q = '\\''; let s = \"//\"; eprintln!(\"{name}: x\");"),
        format!("fn f<'a>(s: &'a str) {{ eprintln!(\"{name}: x\"); }}"),
        format!("/* a note */ eprintln!(\"{name}: x\");"),
        // a comment ends at its line, and an escaped quote in a char opens no string
        format!("// a note\neprintln!(\"{name}: x\");"),
        format!("let c = '\\\"'; let s = \"//\"; eprintln!(\"{name}: x\");"),
    ];
    for line in &caught {
        assert_eq!(under_the_old_name(line).len(), 1, "missed: {line}");
    }
    let continued = format!("eprintln!(\"first line\\n\\\n     {name}: second line\");");
    assert_eq!(
        under_the_old_name(&continued),
        vec![(2, format!("{name}: second line\");"))]
    );
    // a continuation skips every blank line and all indentation that follows it
    for blank in ["", "    "] {
        let continued =
            format!("eprintln!(\"first line\\n\\\n{blank}\n     {name}: second line\");");
        assert_eq!(
            under_the_old_name(&continued),
            vec![(3, format!("{name}: second line\");"))],
            "after a line of {blank:?}"
        );
    }
    let after_a_block = format!("/* a note\n   on two lines */ eprintln!(\"{name}: x\");");
    assert_eq!(under_the_old_name(&after_a_block).len(), 1);
    let passed = [
        format!("/// the `{name}: ` key"),
        format!("// \"{name}: x\""),
        format!("let x = 1; // formerly \"{name}: x\""),
        format!("/* formerly \"{name}: x\" */"),
        format!("let x = 1; /* \"{name}: x\" */ let y = 2;"),
        format!("/* /* nested */ \"{name}: x\" */"),
        format!("/*\n * formerly \"{name}: x\"\n */"),
        format!("/**\n * eprintln!(\"first\\n\\\n {name}: x\");\n */"),
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
    let mut read = 0;
    for file in &files {
        let text = std::fs::read_to_string(file).expect("a source reads");
        read += without_comments(&text).matches("\"amem: ").count();
        for (n, line) in under_the_old_name(&text) {
            found.push(format!("{}:{n}: {line}", file.display()));
        }
    }
    // The binary has about 80 messages; far fewer seen means comments hid code.
    assert!(read > 70, "only {read} messages seen outside comments");
    assert!(
        found.is_empty(),
        "messages under the old name:\n{}",
        found.join("\n")
    );
}
