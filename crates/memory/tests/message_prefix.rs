//! Every line the binary writes for a person starts with `amem:`, the command's name.
//! The demo selects the lines of `amem work` by that prefix, so a message under the old
//! name is a line the demo drops without a word. This reads the sources, since no run
//! reaches every message.

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
    // the old spellings, assembled so this file does not hold them
    let old: Vec<String> = ["\"", "`", "b\""]
        .iter()
        .flat_map(|q| [format!("{q}memory: "), format!("{q}memory hook:")])
        .collect();
    let mut found = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file).expect("a source reads");
        for (n, line) in text.lines().enumerate() {
            if old.iter().any(|o| line.contains(o.as_str())) {
                found.push(format!("{}:{}: {}", file.display(), n + 1, line.trim()));
            }
        }
    }
    assert!(
        found.is_empty(),
        "messages under the old name:\n{}",
        found.join("\n")
    );
}
