//! The address of what a fetch serves: an entry, one section of it, or one version of
//! it. The paper's Appendix A names the three: `id`, `id#anchor` and `id@version`.
//!
//! An anchor is the slug of a Markdown heading in the body. A version is the digest of
//! the stored text of the entry, the form that an approval and a declassified copy
//! cite. A citation in one of the three forms names the entry by its bare id
//! ([`entry_of`]), so lineage reaches an entry that was cited by version.

use std::str::FromStr;

/// The most characters an entry id holds.
const MAX_ID: usize = 128;
/// The most characters an anchor holds.
pub const MAX_ANCHOR: usize = 64;
/// The characters of a version: a SHA-256 digest in lower-case hex.
pub const VERSION_LEN: usize = 64;
/// The three forms, for a refusal.
pub const FORMS: &str = "an address is `<id>`, `<id>#<anchor>` or `<id>@<version>`: an anchor is 1 to 64 characters of a-z, 0-9 and `-`, and a version is 64 lower-case hex characters";

/// Which part of an entry an address names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Part {
    /// The current version, whole.
    Whole,
    /// One section of the current version, by the anchor of its heading.
    Anchor(String),
    /// One version, by the digest of its stored text.
    Version(String),
}

/// An entry, and the part of it to serve.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Address {
    /// The entry.
    pub id: String,
    /// The part.
    pub part: Part,
}

/// Whether `id` is a name that the record takes for an entry: what the store takes. An
/// id that this code makes starts with `mem-`. An entry that a person wrote by hand, or
/// that came from another root, can have another name, and a fetch serves it too.
fn is_id(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('.')
        && id.len() <= MAX_ID
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn is_anchor(anchor: &str) -> bool {
    (1..=MAX_ANCHOR).contains(&anchor.len())
        && anchor
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn is_version(version: &str) -> bool {
    version.len() == VERSION_LEN
        && version
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

/// The id of an address, a slice of `s`, and its part, when all of `s` is well formed. An
/// address with both `#` and `@` is refused: an anchor holds no `@`, and a version no `#`.
fn parts(s: &str) -> Result<(&str, Part), String> {
    let refused = || format!("`{s}` is no address: {FORMS}");
    let (id, part) = match (s.split_once('#'), s.split_once('@')) {
        (Some((id, anchor)), None) if is_anchor(anchor) => (id, Part::Anchor(anchor.to_string())),
        (None, Some((id, version))) if is_version(version) => {
            (id, Part::Version(version.to_string()))
        }
        (None, None) => (s, Part::Whole),
        _ => return Err(refused()),
    };
    if is_id(id) {
        Ok((id, part))
    } else {
        Err(refused())
    }
}

impl FromStr for Address {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (id, part) = parts(s)?;
        Ok(Address {
            id: id.to_string(),
            part,
        })
    }
}

/// The bare id of a citation that names an entry in one of the three forms. A link, a
/// path, a transcript or a malformed address names no entry. Nor does a name without
/// the shape of an entry id: a citation is free text, and a word in it is no entry.
#[must_use]
pub fn entry_of(cite: &str) -> Option<&str> {
    let (id, _) = parts(cite).ok()?;
    dm_core::id::is_entry_id(id).then_some(id)
}

/// The entries that a list of citations names, each one time, by the bare id, in the
/// order of first mention.
#[must_use]
pub fn entries_of(cites: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for id in cites.iter().filter_map(|c| entry_of(c)) {
        if !out.iter().any(|o| o == id) {
            out.push(id.to_string());
        }
    }
    out
}

/// One heading of a body.
struct Heading {
    /// The line it is on, counted from zero.
    line: usize,
    /// One for `#`, to six for `######`.
    level: usize,
    /// Its anchor; empty when the text of the heading holds no letter and no digit.
    anchor: String,
}

/// The level and the text of an ATX heading: one to six `#`, then a space.
fn atx(line: &str) -> Option<(usize, &str)> {
    let level = line.chars().take_while(|c| *c == '#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    line[level..].strip_prefix(' ').map(|text| (level, text))
}

/// The character of a code fence that this line opens or closes.
fn fence(line: &str) -> Option<char> {
    let t = line.trim_start();
    ['`', '~']
        .into_iter()
        .find(|c| t.chars().take_while(|x| x == c).count() >= 3)
}

/// The anchor of a heading: its text in lower case, each run of other characters as one
/// `-`, none at either end, and no longer than an address may name.
fn slug(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.truncate(MAX_ANCHOR);
    out.trim_end_matches('-').to_string()
}

/// The headings of a body outside its fenced code blocks, in order.
fn headings(body: &str) -> Vec<Heading> {
    let mut open: Option<char> = None;
    let mut out = Vec::new();
    for (line, text) in body.lines().enumerate() {
        if let Some(mark) = fence(text) {
            match open {
                None => open = Some(mark),
                Some(o) if o == mark => open = None,
                Some(_) => {}
            }
        } else if open.is_none() {
            if let Some((level, title)) = atx(text) {
                out.push(Heading {
                    line,
                    level,
                    anchor: slug(title),
                });
            }
        }
    }
    out
}

/// The anchors of a body, in the order of its headings. When two headings give the same
/// anchor, the first one has it.
#[must_use]
pub fn anchors(body: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for h in headings(body) {
        if !h.anchor.is_empty() && !out.contains(&h.anchor) {
            out.push(h.anchor);
        }
    }
    out
}

/// The section of a body that `anchor` names: the line of its heading and every line up
/// to the next heading of the same or a higher level. A heading of a lower level stays
/// inside the section. `None` when no heading gives the anchor.
#[must_use]
pub fn section(body: &str, anchor: &str) -> Option<String> {
    let all = headings(body);
    let at = all.iter().position(|h| h.anchor == anchor)?;
    let start = &all[at];
    let end = all[at + 1..]
        .iter()
        .find(|h| h.level <= start.level)
        .map(|h| h.line);
    Some(
        body.split_inclusive('\n')
            .enumerate()
            .filter(|(n, _)| *n >= start.line && end.is_none_or(|e| *n < e))
            .map(|(_, l)| l)
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const V: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn an_address_names_an_entry_a_section_or_a_version() {
        let whole: Address = "mem-me-abc12345".parse().unwrap();
        assert_eq!(whole.id, "mem-me-abc12345");
        assert_eq!(whole.part, Part::Whole);
        let anchor: Address = "mem-me-abc12345#how-to-2".parse().unwrap();
        assert_eq!(anchor.id, "mem-me-abc12345");
        assert_eq!(anchor.part, Part::Anchor("how-to-2".into()));
        let version: Address = format!("mem-me-abc12345@{V}").parse().unwrap();
        assert_eq!(version.id, "mem-me-abc12345");
        assert_eq!(version.part, Part::Version(V.into()));
    }

    #[test]
    fn a_malformed_address_is_refused_and_the_refusal_names_the_three_forms() {
        let long = format!("mem-me-abc12345#{}", "a".repeat(MAX_ANCHOR + 1));
        let short = format!("mem-me-abc12345@{}", &V[1..]);
        let upper = format!("mem-me-abc12345@{}", V.to_uppercase());
        let both = format!("mem-me-abc12345#steps@{V}");
        let both_reversed = format!("mem-me-abc12345@{V}#steps");
        let long_id = format!("mem-{}", "a".repeat(MAX_ID));
        for bad in [
            ".mem-me-abc12345",
            "mem-me-abc12345/steps",
            "mem-me-abc12345:steps",
            "mem-me abc12345",
            "mem-me-abc12345#",
            "mem-me-abc12345#Steps",
            "mem-me-abc12345#a_b",
            long.as_str(),
            "mem-me-abc12345@",
            "mem-me-abc12345@abc",
            short.as_str(),
            upper.as_str(),
            both.as_str(),
            both_reversed.as_str(),
            long_id.as_str(),
            "#steps",
            "",
        ] {
            let refused = bad.parse::<Address>();
            assert!(
                matches!(&refused, Err(why) if why.contains("<id>#<anchor>") && why.contains("<id>@<version>")),
                "{bad}: {refused:?}"
            );
            assert_eq!(entry_of(bad), None, "{bad}");
        }
    }

    #[test]
    fn an_address_takes_each_name_that_the_record_takes_and_a_citation_only_an_entry_id() {
        for id in ["note-1", "task-me-abc12345", "v1.2_final", "A"] {
            let whole: Address = id.parse().unwrap();
            assert_eq!((whole.id.as_str(), &whole.part), (id, &Part::Whole));
            let version: Address = format!("{id}@{V}").parse().unwrap();
            assert_eq!(version.id, id);
            let anchor: Address = format!("{id}#steps").parse().unwrap();
            assert_eq!(anchor.part, Part::Anchor("steps".into()));
            assert_eq!(entry_of(id), None, "{id}");
            assert_eq!(entry_of(&format!("{id}@{V}")), None, "{id}");
        }
    }

    #[test]
    fn a_citation_in_each_form_names_the_bare_entry_and_a_link_names_none() {
        assert_eq!(entry_of("mem-me-abc12345"), Some("mem-me-abc12345"));
        assert_eq!(entry_of("mem-me-abc12345#steps"), Some("mem-me-abc12345"));
        let by_version = format!("mem-me-abc12345@{V}");
        assert_eq!(entry_of(&by_version), Some("mem-me-abc12345"));
        for other in [
            "https://example.org/mem-me-abc12345",
            "docs/mem-me-abc12345.md",
            "transcript:sess-1#4",
            "mem-me-abc12345@latest",
        ] {
            assert_eq!(entry_of(other), None, "{other}");
        }
    }

    #[test]
    fn the_entries_of_a_list_of_citations_are_bare_ids_each_one_time() {
        let cites = [
            format!("mem-me-abc12345@{V}"),
            "https://example.org/fact".to_string(),
            "mem-me-other123#steps".to_string(),
            "mem-me-abc12345".to_string(),
        ];
        assert_eq!(entries_of(&cites), ["mem-me-abc12345", "mem-me-other123"]);
    }

    const BODY: &str = "# Import: the Steps!\n\nIntro.\n\n## First step\n\nOne.\n\n```sh\n# Not a heading\n## Nor this\n```\n\n### A detail\n\nFine print.\n\n~~~\n```\n# Still code\n~~~\n\n## Second step\n\nTwo.\n\n## First step\n\nAgain.\n\n# Appendix\n\nLast.\n";

    #[test]
    fn a_heading_inside_a_fenced_code_block_gives_no_anchor() {
        assert_eq!(
            anchors(BODY),
            [
                "import-the-steps",
                "first-step",
                "a-detail",
                "second-step",
                "appendix"
            ]
        );
        assert_eq!(section(BODY, "not-a-heading"), None);
        assert_eq!(section(BODY, "nor-this"), None);
        assert_eq!(section(BODY, "still-code"), None);
        // no space after the marks, or more than six marks: no heading
        assert!(anchors("#Tag\n\n####### Seven\n").is_empty());
    }

    #[test]
    fn a_section_ends_at_the_next_heading_of_the_same_or_a_higher_level() {
        // the lower-level heading and the code block stay inside; the first of two
        // headings with one anchor has it
        assert_eq!(
            section(BODY, "first-step").unwrap(),
            "## First step\n\nOne.\n\n```sh\n# Not a heading\n## Nor this\n```\n\n### A detail\n\nFine print.\n\n~~~\n```\n# Still code\n~~~\n\n"
        );
        assert_eq!(
            section(BODY, "a-detail").unwrap(),
            "### A detail\n\nFine print.\n\n~~~\n```\n# Still code\n~~~\n\n"
        );
        // a higher level ends it
        assert_eq!(
            section(BODY, "second-step").unwrap(),
            "## Second step\n\nTwo.\n\n"
        );
        // the last section runs to the end of the body
        assert_eq!(section(BODY, "appendix").unwrap(), "# Appendix\n\nLast.\n");
        // the first heading holds every section below it, up to its own level
        let top = section(BODY, "import-the-steps").unwrap();
        assert!(top.starts_with("# Import: the Steps!\n"), "{top}");
        assert!(top.ends_with("Again.\n\n"), "{top}");
        assert_eq!(section(BODY, "nope"), None);
    }

    #[test]
    fn every_anchor_of_a_body_is_one_that_an_address_may_name() {
        let long = format!("# {}\n\n## Größe & Maß\n\n## !!!\n", "word ".repeat(40));
        let found = anchors(&long);
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(found[1], "gr-e-ma");
        for a in &found {
            assert!(is_anchor(a), "{a}");
            assert!(section(&long, a).is_some(), "{a}");
        }
    }
}
