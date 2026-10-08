//! The core crate in a browser: the browser reader parses entries and manifests with the
//! same code the server runs, so an exported bundle reads the same everywhere. Every
//! export takes text in and gives JSON out, which keeps the boundary to one shape and
//! keeps the functions testable natively.

use dm_core::manifest::Manifest;
use dm_core::{Entry, Label};
use serde::Serialize;
use wasm_bindgen::prelude::*;

/// An entry as the page sees it: the frontmatter's fields, the body, and what the index
/// would serve for it.
#[derive(Serialize)]
struct Parsed {
    id: String,
    domain: String,
    #[serde(rename = "type")]
    entry_type: String,
    author: String,
    created: String,
    trust_tier: String,
    classification: String,
    label_rank: u8,
    source: Vec<String>,
    supersedes: Option<String>,
    status: Option<String>,
    title: String,
    summary: String,
    body: String,
    digest: String,
}

fn rank(label: Label) -> u8 {
    match label {
        Label::Public => 0,
        Label::Internal => 1,
        Label::Restricted => 2,
    }
}

/// Parse one entry's text. JSON of the entry, or a JSON object with an `error`.
#[wasm_bindgen]
#[must_use]
pub fn parse_entry(text: &str) -> String {
    match Entry::parse(text) {
        Ok(e) => {
            let p = Parsed {
                id: e.front.id.clone(),
                domain: e.front.domain.clone(),
                entry_type: e.front.entry_type.to_string(),
                author: e.front.author.clone(),
                created: e
                    .front
                    .created
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_default(),
                trust_tier: e.front.trust_tier.to_string(),
                classification: e.front.classification.to_string(),
                label_rank: rank(e.front.classification),
                source: e.front.source.clone(),
                supersedes: e.front.supersedes.clone(),
                status: e.front.status.map(|s| format!("{s:?}").to_lowercase()),
                title: e.title(),
                summary: e.summary(),
                digest: dm_core::digest(&e.render()),
                body: e.body,
            };
            serde_json::to_string(&p).unwrap_or_else(|err| error(&err.to_string()))
        }
        Err(e) => error(&e.to_string()),
    }
}

/// Parse a bundle's `manifest.yaml`. JSON of the manifest, or an object with an `error`:
/// a foreign format, or a manifest that does not agree with itself about the labels
/// inside it, which a reader would otherwise report on the manifest's word.
#[wasm_bindgen]
#[must_use]
pub fn parse_manifest(yaml: &str) -> String {
    match serde_yaml::from_str::<Manifest>(yaml) {
        Ok(m) if m.format == dm_core::manifest::FORMAT => match m.consistent() {
            Ok(()) => serde_json::to_string(&m).unwrap_or_else(|err| error(&err.to_string())),
            Err(e) => error(&e.to_string()),
        },
        Ok(m) => error(&format!(
            "bundle format {:?} is not {}",
            m.format,
            dm_core::manifest::FORMAT
        )),
        Err(e) => error(&e.to_string()),
    }
}

/// The digest of a text, as the manifest records it.
#[wasm_bindgen]
#[must_use]
pub fn digest(text: &str) -> String {
    dm_core::digest(text)
}

/// Where a label sits in the order: 0 public, 1 internal, 2 restricted; 255 for a word
/// that is not a label.
#[wasm_bindgen]
#[must_use]
pub fn label_rank(label: &str) -> u8 {
    label.parse::<Label>().map_or(255, rank)
}

fn error(msg: &str) -> String {
    serde_json::json!({ "error": msg }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENTRY: &str = "---\nid: mem-me-00000001\ntype: fact\ndomain: dom-me\nauthor: human:test:me\ncreated: 2026-10-01T09:00:00Z\ntrust_tier: curated\nclassification: internal\n---\n# Retry policy\n\nOperation R retries with backoff.\n";

    #[test]
    fn an_entry_parses_to_what_the_index_would_serve() {
        let v: serde_json::Value = serde_json::from_str(&parse_entry(ENTRY)).unwrap();
        assert_eq!(v["id"], "mem-me-00000001");
        assert_eq!(v["title"], "Retry policy");
        assert_eq!(v["summary"], "Operation R retries with backoff.");
        assert_eq!(v["classification"], "internal");
        assert_eq!(v["label_rank"], 1);
        assert_eq!(v["trust_tier"], "curated");
        assert!(v["body"].as_str().unwrap().contains("backoff"));
        let d = v["digest"].as_str().unwrap();
        assert_eq!(d.len(), 64);
        assert_eq!(digest(&Entry::parse(ENTRY).unwrap().render()), d);
        let bad: serde_json::Value = serde_json::from_str(&parse_entry("no fence")).unwrap();
        assert!(bad["error"].as_str().unwrap().contains("frontmatter"));
    }

    #[test]
    fn a_manifest_that_understates_what_is_inside_it_is_refused() {
        let mut m = Manifest::new("dom-me", "memory test", Label::Internal);
        m.push(dm_core::manifest::ManifestEntry {
            id: "mem-me-00000001".into(),
            path: "mem-me-00000001.md".into(),
            digest: "00".into(),
            classification: Label::Internal,
        })
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&parse_manifest(&m.to_yaml())).unwrap();
        assert_eq!(v["highest_label"], "internal");
        let understated = m
            .to_yaml()
            .replace("highest_label: internal", "highest_label: public");
        let v: serde_json::Value = serde_json::from_str(&parse_manifest(&understated)).unwrap();
        assert!(
            v["error"]
                .as_str()
                .is_some_and(|e| e.contains("highest_label")),
            "{v}"
        );
    }

    #[test]
    fn a_manifest_parses_and_a_foreign_format_is_refused() {
        let m = Manifest::new("dom-me", "memory test", Label::Internal);
        let yaml = serde_yaml::to_string(&m).unwrap();
        let v: serde_json::Value = serde_json::from_str(&parse_manifest(&yaml)).unwrap();
        assert_eq!(v["domain"], "dom-me");
        assert_eq!(v["max_label"], "internal");
        let other = yaml.replace(dm_core::manifest::FORMAT, "someone-else/bundle/9");
        let v: serde_json::Value = serde_json::from_str(&parse_manifest(&other)).unwrap();
        assert!(v["error"].as_str().unwrap().contains("format"));
        assert_eq!(label_rank("restricted"), 2);
        assert_eq!(label_rank("secret"), 255);
    }
}
