//! What remains after an erasure. Demotion keeps the body behind a tombstone and can be
//! undone; erasure removes the body from every version and cannot. The record keeps
//! three things in its place: the stub, the entry's frontmatter with an empty body and
//! `status: erased`; the erasure record, who asked, why as a reason class, and when; and
//! the receipt, what was covered and what could not be reached. The last two are this
//! file, kept beside the record as `.erasures/<id>.yaml` in the entry's domain.

use crate::{Entry, Status};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// Why an entry was erased, as a class rather than a story, so a receipt can be read
/// without reading the reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Reason {
    /// The person it concerns, or its author, asked.
    Requested,
    /// It should never have been written: wrong, or someone else's.
    Mistaken,
    /// It carries material above what the record may hold.
    Sensitive,
    /// A legal obligation.
    Legal,
}

impl std::str::FromStr for Reason {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "requested" => Ok(Reason::Requested),
            "mistaken" => Ok(Reason::Mistaken),
            "sensitive" => Ok(Reason::Sensitive),
            "legal" => Ok(Reason::Legal),
            other => Err(format!(
                "reason {other:?} is not requested, mistaken, sensitive or legal"
            )),
        }
    }
}

impl std::fmt::Display for Reason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Reason::Requested => "requested",
            Reason::Mistaken => "mistaken",
            Reason::Sensitive => "sensitive",
            Reason::Legal => "legal",
        })
    }
}

/// The stub that stands where the entry was: its frontmatter with no body, no summary,
/// and `status: erased`. When the erasure was asked for by the person it concerns, the
/// stub keeps neither the author nor the sources, since those are about that person too;
/// the title goes with them, and goes too when the material was above what the record
/// may hold, since a title says what the body said.
#[must_use]
pub fn stub(entry: &Entry, reason: Reason) -> Entry {
    let mut front = entry.front.clone();
    front.status = Some(Status::Erased);
    front.summary = None;
    if matches!(reason, Reason::Requested | Reason::Sensitive) {
        front.title = None;
    }
    if reason == Reason::Requested {
        front.author = "erased".into();
        front.source.clear();
    }
    Entry {
        front,
        body: String::new(),
    }
}

/// One version whose body was removed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Removed {
    /// `current`, or the archived version's name.
    pub version: String,
    /// The digest of the text that was removed, so a copy can be recognised later.
    pub digest: String,
    /// The digest of the body alone, trimmed, so a copy whose frontmatter was edited is
    /// recognised too. Absent when the version did not parse as an entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_digest: Option<String>,
}

/// The digest an erasure keeps of a body: the text alone, trimmed, so the same words
/// under other frontmatter or a trailing newline are the same body.
#[must_use]
pub fn body_digest(body: &str) -> String {
    crate::digest(body.trim())
}

/// The erasure record and the receipt, together.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Receipt {
    /// The erased entry.
    pub id: String,
    /// Its domain.
    pub domain: String,
    /// When.
    #[serde(with = "time::serde::rfc3339")]
    pub erased_at: OffsetDateTime,
    /// Who asked, system-qualified.
    pub erased_by: String,
    /// Why, as a class.
    pub reason: Reason,
    /// Every version whose body was removed, the current one first.
    pub removed: Vec<Removed>,
    /// Entries that cite the erased one, flagged for review rather than re-derived here.
    #[serde(default)]
    pub flagged: Vec<String>,
    /// Copies this erasure could not reach, named so nobody believes it did.
    #[serde(default)]
    pub unresolved: Vec<String>,
    /// How many records of the entry's promotions had their why removed from the
    /// ledger. Who promoted which version to which tier, and when, is kept.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub promotion_whys_removed: usize,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// The line a receipt carries while an erasure has not finished: the record step is done
/// and the rest is owed. Running the erasure again finishes it and drops the line.
pub const UNFINISHED: &str =
    "the index, the served bundles and the citing entries: the erasure did not finish; run it again";

impl Receipt {
    /// Whether every step ran; a receipt written between the record step and the rest
    /// says so in `unresolved`.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        !self.unresolved.iter().any(|u| u == UNFINISHED)
    }

    /// The receipt as the record keeps it.
    ///
    /// # Panics
    ///
    /// Only if the receipt stops being serialisable, which the round-trip test guards.
    #[must_use]
    pub fn to_yaml(&self) -> String {
        serde_norway::to_string(self).expect("a receipt serialises")
    }

    /// A receipt read back.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Yaml`] when the text is not a receipt.
    pub fn from_yaml(text: &str) -> Result<Receipt, crate::Error> {
        Ok(serde_norway::from_str(text)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_receipt_knows_whether_it_finished() {
        let mut r = Receipt {
            id: "mem-me-1".into(),
            domain: "dom-me".into(),
            erased_at: OffsetDateTime::UNIX_EPOCH,
            erased_by: "human:test:me".into(),
            reason: Reason::Legal,
            removed: vec![],
            flagged: vec![],
            unresolved: vec![],
            promotion_whys_removed: 0,
        };
        assert!(r.is_finished());
        r.unresolved.push(UNFINISHED.into());
        assert!(!r.is_finished());
        assert!(!Receipt::from_yaml(&r.to_yaml()).unwrap().is_finished());
    }

    #[test]
    fn a_receipt_round_trips_and_reasons_parse() {
        let r = Receipt {
            id: "mem-me-00000001".into(),
            domain: "dom-me".into(),
            erased_at: OffsetDateTime::UNIX_EPOCH,
            erased_by: "human:test:me".into(),
            reason: Reason::Requested,
            removed: vec![Removed {
                version: "current".into(),
                digest: "ab".repeat(32),
                body_digest: Some(body_digest("The body.\n")),
            }],
            flagged: vec!["mem-me-00000002".into()],
            unresolved: vec!["exports issued before the erasure".into()],
            promotion_whys_removed: 0,
        };
        let text = r.to_yaml();
        assert!(text.contains("reason: requested"));
        assert_eq!(Receipt::from_yaml(&text).unwrap(), r);
        assert_eq!(body_digest("The body.\n"), body_digest("\n\nThe body."));
        assert_ne!(body_digest("The body."), body_digest("The body, edited."));
        assert_eq!("legal".parse::<Reason>().unwrap(), Reason::Legal);
        assert!("because".parse::<Reason>().is_err());
        assert_eq!(Reason::Sensitive.to_string(), "sensitive");
        assert!(Receipt::from_yaml("nope: [").is_err());
    }

    #[test]
    fn a_stub_keeps_the_frontmatter_and_nothing_the_body_said() {
        let text = "---\nid: mem-me-1\ntype: fact\ndomain: dom-me\nauthor: human:test:me\ncreated: 2026-10-01T10:00:00Z\nsource:\n- https://example.test/x\nsummary: the secret summary\ntitle: Alice's diagnosis\n---\nThe secret body.\n";
        let e = Entry::parse(text).unwrap();
        let s = stub(&e, Reason::Mistaken);
        assert_eq!(s.front.status, Some(Status::Erased));
        assert_eq!(s.body, "");
        assert_eq!(s.front.summary, None);
        assert_eq!(s.front.author, "human:test:me");
        assert_eq!(s.front.source.len(), 1);
        assert_eq!(s.front.title.as_deref(), Some("Alice's diagnosis"));
        let rendered = s.render();
        assert!(!rendered.contains("secret"), "{rendered}");
        assert_eq!(Entry::parse(&rendered).unwrap(), s, "a stub round-trips");
        let r = stub(&e, Reason::Requested);
        assert_eq!(r.front.author, "erased");
        assert!(r.front.source.is_empty());
        assert_eq!(r.front.title, None, "a title is about the person too");
        assert_eq!(stub(&e, Reason::Sensitive).front.title, None);
        assert!(stub(&e, Reason::Legal).front.title.is_some());
    }
}
