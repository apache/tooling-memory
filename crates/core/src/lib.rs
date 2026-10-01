//! The entry format and everything every other crate must agree on.
//!
//! No I/O lives here. This crate compiles to native, to wasm for the browser
//! reader, and to WASI for plugins.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;
use time::OffsetDateTime;

pub mod filter;
pub mod id;
pub mod manifest;

pub use filter::ReadFilter;

/// How sensitive an entry is. Ordered: public < internal < restricted.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "lowercase")]
pub enum Label {
    /// Anyone may read it.
    #[default]
    Public,
    /// The domain's members may read it.
    Internal,
    /// Only a session whose handling allows it may read it.
    Restricted,
}

impl fmt::Display for Label {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Label::Public => "public",
            Label::Internal => "internal",
            Label::Restricted => "restricted",
        };
        f.write_str(s)
    }
}

impl std::str::FromStr for Label {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Error> {
        match s {
            "public" => Ok(Label::Public),
            "internal" => Ok(Label::Internal),
            "restricted" => Ok(Label::Restricted),
            other => Err(Error::Invalid(format!("unknown label {other:?}"))),
        }
    }
}

/// How far an entry has been checked. Ordered: raw < curated < verified.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "lowercase")]
pub enum TrustTier {
    /// Written and not yet looked at. Searchable, with the tier visible.
    #[default]
    Raw,
    /// A librarian has summarised, merged, or verified it.
    Curated,
    /// Passed the review gate; the approver was not the proposer.
    Verified,
}

impl fmt::Display for TrustTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            TrustTier::Raw => "raw",
            TrustTier::Curated => "curated",
            TrustTier::Verified => "verified",
        })
    }
}

/// What kind of knowledge an entry holds. The kinds age differently.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum EntryType {
    /// Context, options, choice, rationale. Superseded, never expired.
    Decision,
    /// Decays fast; carries a review date. The default kind.
    #[default]
    Fact,
    /// How things are done here; needs an owner.
    Convention,
    /// A strategy or skill: a convention about how to work, under stricter scrutiny.
    Method,
    /// What an agent wrote when it stopped, so the next one need not read its transcript.
    Handoff,
}

impl fmt::Display for EntryType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            EntryType::Decision => "decision",
            EntryType::Fact => "fact",
            EntryType::Convention => "convention",
            EntryType::Method => "method",
            EntryType::Handoff => "handoff",
        })
    }
}

impl std::str::FromStr for EntryType {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Error> {
        match s {
            "decision" => Ok(EntryType::Decision),
            "fact" => Ok(EntryType::Fact),
            "convention" => Ok(EntryType::Convention),
            "method" => Ok(EntryType::Method),
            "handoff" => Ok(EntryType::Handoff),
            other => Err(Error::Invalid(format!("unknown entry type {other:?}"))),
        }
    }
}

/// The author's own confidence in an entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    /// Checked against a source.
    High,
    /// Believed, not checked.
    Medium,
    /// A guess worth recording.
    Low,
}

/// An entry's place in its lifecycle. Absent means active.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Demoted by curation; the body is still in the archive behind this tombstone.
    Demoted,
    /// Superseded by another entry, which names this one in its `supersedes`.
    Superseded,
    /// Erased: the body is gone everywhere, only this stub remains.
    Erased,
}

/// The frontmatter every entry carries. Field names are the contract.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Frontmatter {
    /// Stable, permanent, meaningless.
    pub id: String,
    /// What kind of knowledge this is.
    #[serde(rename = "type")]
    pub entry_type: EntryType,
    /// A capability or a person, never a team name.
    pub domain: String,
    /// `agent:<name> (model: <id>)` or `human:<system>:<user-id>`.
    pub author: String,
    /// When the entry was written.
    #[serde(with = "time::serde::rfc3339")]
    pub created: OffsetDateTime,
    /// Links or citations the entry was distilled from.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source: Vec<String>,
    /// The author's confidence, if stated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<Confidence>,
    /// How far the entry has been checked.
    #[serde(default)]
    pub trust_tier: TrustTier,
    /// Decay clock, as a calendar date.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_by: Option<time::Date>,
    /// The entry this one replaces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
    /// The policy versions in force when it was written.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub policy_context: Vec<String>,
    /// How sensitive the entry is; at least the label of what the author read.
    #[serde(default)]
    pub classification: Label,
    /// Lifecycle state; absent means active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Status>,
    /// Where an imported or tapped entry came from, for dedup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// One line, what the index serves before anyone opens the body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// A title, when the body's first heading is not the right one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// One entry: frontmatter plus a Markdown body.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    /// The frontmatter.
    pub front: Frontmatter,
    /// The Markdown body, without the frontmatter block.
    pub body: String,
}

/// What can go wrong reading or building an entry.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The text does not start with a `---` fenced frontmatter block.
    #[error("entry has no frontmatter block")]
    NoFrontmatter,
    /// The frontmatter is not valid YAML, or does not fit [`Frontmatter`].
    #[error("frontmatter: {0}")]
    Yaml(#[from] serde_yaml::Error),
    /// A value is well-formed but not allowed.
    #[error("invalid: {0}")]
    Invalid(String),
}

const FENCE: &str = "---";

impl Entry {
    /// Parse `---\nyaml\n---\nbody`.
    ///
    /// # Errors
    ///
    /// [`Error::NoFrontmatter`] when the fences are missing, [`Error::Yaml`] when the
    /// block does not fit [`Frontmatter`], and [`Error::Invalid`] when `id` or `domain`
    /// is empty.
    pub fn parse(text: &str) -> Result<Entry, Error> {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let mut lines = text.split_inclusive('\n');
        let first = lines.next().ok_or(Error::NoFrontmatter)?;
        if first.trim_end() != FENCE {
            return Err(Error::NoFrontmatter);
        }
        let mut yaml = String::new();
        let mut body = String::new();
        let mut in_front = true;
        for line in lines {
            if in_front {
                if line.trim_end() == FENCE {
                    in_front = false;
                } else {
                    yaml.push_str(line);
                }
            } else {
                body.push_str(line);
            }
        }
        if in_front {
            return Err(Error::NoFrontmatter);
        }
        let front: Frontmatter = serde_yaml::from_str(&yaml)?;
        if front.id.is_empty() || front.domain.is_empty() {
            return Err(Error::Invalid("id and domain are required".into()));
        }
        let body = body.strip_prefix('\n').unwrap_or(&body).to_string();
        Ok(Entry { front, body })
    }

    /// Render to the on-disk form. `parse(render(e)) == e`.
    ///
    /// # Panics
    ///
    /// Only if [`Frontmatter`] stops being serialisable, which a test guards.
    pub fn render(&self) -> String {
        let yaml = serde_yaml::to_string(&self.front).expect("frontmatter serialises");
        let mut out = String::with_capacity(yaml.len() + self.body.len() + 16);
        out.push_str(FENCE);
        out.push('\n');
        out.push_str(&yaml);
        if !yaml.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(FENCE);
        out.push('\n');
        if !self.body.is_empty() {
            out.push('\n');
            out.push_str(&self.body);
            if !self.body.ends_with('\n') {
                out.push('\n');
            }
        }
        out
    }

    /// The title the index shows: explicit, else the first heading, else the id.
    #[must_use]
    pub fn title(&self) -> String {
        if let Some(t) = &self.front.title {
            return t.clone();
        }
        self.body
            .lines()
            .find_map(|l| l.strip_prefix("# ").or_else(|| l.strip_prefix("## ")))
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| self.front.id.clone())
    }

    /// The one-line summary the index serves: explicit, else the first paragraph, clipped.
    #[must_use]
    pub fn summary(&self) -> String {
        if let Some(s) = &self.front.summary {
            return s.clone();
        }
        let para: String = self
            .body
            .lines()
            .map(str::trim)
            .skip_while(|l| l.is_empty() || l.starts_with('#'))
            .take_while(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        clip(&para, 200)
    }

    /// Whether the entry is live, as opposed to demoted, superseded, or erased.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.front.status.is_none()
    }
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max - 1).collect();
    format!("{}…", cut.trim_end())
}

/// Content digest, hex SHA-256. What manifests and stubs carry.
#[must_use]
pub fn digest(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

/// A rough token count for budgeting T0: whitespace-split words times 1.3.
#[must_use]
pub fn approx_tokens(text: &str) -> usize {
    let words = text.split_whitespace().count();
    words + words.div_ceil(3)
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::{date, datetime};

    fn sample() -> Entry {
        Entry {
            front: Frontmatter {
                id: "mem-a-0847".into(),
                entry_type: EntryType::Decision,
                domain: "dom-a".into(),
                author: "agent:worker-3 (model: test-1)".into(),
                created: datetime!(2026-10-01 10:00 UTC),
                source: vec!["https://example.org/thread/1".into()],
                confidence: Some(Confidence::High),
                trust_tier: TrustTier::Curated,
                review_by: Some(date!(2027-04-01)),
                supersedes: Some("mem-a-0312".into()),
                policy_context: vec!["pol-L2-031".into()],
                classification: Label::Internal,
                status: None,
                origin: None,
                summary: None,
                title: None,
            },
            body: "# Retry with backoff\n\nOperation R retries with exponential backoff.\nState lives in header H.\n\n## Options\n\nTwo were considered.\n".into(),
        }
    }

    #[test]
    fn round_trip() {
        let e = sample();
        let text = e.render();
        let back = Entry::parse(&text).unwrap();
        assert_eq!(e, back);
        assert_eq!(text, back.render());
    }

    #[test]
    fn minimal_frontmatter_defaults() {
        let text = "---\nid: mem-x-1\ntype: handoff\ndomain: dom-x\nauthor: human:local:me\ncreated: 2026-10-01T10:00:00Z\n---\n\nStopped at step 3.\n";
        let e = Entry::parse(text).unwrap();
        assert_eq!(e.front.trust_tier, TrustTier::Raw);
        assert_eq!(e.front.classification, Label::Public);
        assert!(e.front.source.is_empty());
        assert_eq!(e.body, "Stopped at step 3.\n");
        assert_eq!(e.title(), "mem-x-1");
        assert_eq!(e.summary(), "Stopped at step 3.");
        assert!(e.is_active());
    }

    #[test]
    fn title_and_summary_from_body() {
        let e = sample();
        assert_eq!(e.title(), "Retry with backoff");
        assert_eq!(
            e.summary(),
            "Operation R retries with exponential backoff. State lives in header H."
        );
    }

    #[test]
    fn explicit_title_and_summary_win() {
        let mut e = sample();
        e.front.title = Some("Given".into());
        e.front.summary = Some("Said once.".into());
        assert_eq!(e.title(), "Given");
        assert_eq!(e.summary(), "Said once.");
    }

    #[test]
    fn summary_is_clipped() {
        let mut e = sample();
        e.body = "x ".repeat(300);
        let s = e.summary();
        assert_eq!(s.chars().count(), 200);
        assert!(s.ends_with('…'));
    }

    #[test]
    fn labels_and_tiers_order_and_parse() {
        assert!(Label::Public < Label::Internal);
        assert!(Label::Internal < Label::Restricted);
        assert_eq!("restricted".parse::<Label>().unwrap(), Label::Restricted);
        assert!("secret".parse::<Label>().is_err());
        assert_eq!(Label::Internal.to_string(), "internal");
        assert!(TrustTier::Raw < TrustTier::Verified);
        assert_eq!(TrustTier::Curated.to_string(), "curated");
        assert_eq!("method".parse::<EntryType>().unwrap(), EntryType::Method);
        assert!("rumour".parse::<EntryType>().is_err());
        assert_eq!(EntryType::Handoff.to_string(), "handoff");
    }

    #[test]
    fn rejects_without_frontmatter() {
        assert!(matches!(
            Entry::parse("just text"),
            Err(Error::NoFrontmatter)
        ));
        assert!(matches!(
            Entry::parse("---\nid: x\n"),
            Err(Error::NoFrontmatter)
        ));
        assert!(matches!(Entry::parse(""), Err(Error::NoFrontmatter)));
    }

    #[test]
    fn rejects_empty_id_or_domain() {
        let text = "---\nid: \"\"\ntype: fact\ndomain: dom-x\nauthor: a\ncreated: 2026-10-01T10:00:00Z\n---\n";
        assert!(matches!(Entry::parse(text), Err(Error::Invalid(_))));
        let bad_yaml = "---\nid: [\ntype: fact\n---\n";
        assert!(matches!(Entry::parse(bad_yaml), Err(Error::Yaml(_))));
    }

    #[test]
    fn status_round_trips_and_marks_inactive() {
        let mut e = sample();
        e.front.status = Some(Status::Demoted);
        let back = Entry::parse(&e.render()).unwrap();
        assert_eq!(back.front.status, Some(Status::Demoted));
        assert!(!back.is_active());
    }

    #[test]
    fn digest_is_stable() {
        assert_eq!(digest("a"), digest("a"));
        assert_ne!(digest("a"), digest("b"));
        assert_eq!(digest("").len(), 64);
    }

    #[test]
    fn approx_tokens_scales_words() {
        assert_eq!(approx_tokens(""), 0);
        assert_eq!(approx_tokens("one two three"), 4);
        assert_eq!(approx_tokens(&"w ".repeat(300)), 400);
    }
}
