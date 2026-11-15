//! What remains after an erasure. Demotion keeps the body behind a tombstone and can be
//! undone; erasure removes the body from every version and cannot. The record keeps
//! three things in its place: the stub, an id, `status: erased` and the access label,
//! with no body; the erasure record, who asked, why as a reason class, and when; and
//! the receipt, what was covered, what was withheld downstream, the sends that left, and
//! what could not be reached. The last two are this file, kept beside the record as
//! `.erasures/<id>.yaml` in the entry's domain.

use crate::{Entry, Status, TrustTier};
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

/// The stub that stands where the entry was: an id, the erased marker and the access
/// label, with the audit metadata that repeats nothing of the entry. It keeps `id`,
/// `type`, `domain`, `created`, `classification`, `supersedes` and `session`, and
/// `status: erased`. The author becomes `erased` and the tier raw. The title, the
/// summary, the citations, the origin, the confidence, the review date, the policy
/// context and the body go, for every reason class: a title says what the body said, a
/// citation or an origin can be a link that carries a credential, and an author is
/// about a person. The reason stays on the receipt.
#[must_use]
pub fn stub(entry: &Entry, _reason: Reason) -> Entry {
    let mut front = entry.front.clone();
    front.status = Some(Status::Erased);
    front.author = "erased".into();
    front.trust_tier = TrustTier::Raw;
    front.cites.clear();
    front.title = None;
    front.summary = None;
    front.origin = None;
    front.confidence = None;
    front.review_by = None;
    front.policy_context.clear();
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

/// The sends of one session to one destination, as the log of one domain holds them:
/// what lineage gives for content that left the perimeter and cannot be called back.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sent {
    /// The domain whose log holds the send events.
    pub domain: String,
    /// Where they went.
    pub to: String,
    /// The session that sent.
    pub session: String,
    /// How many send events.
    pub count: u64,
    /// The sequence number of the first of them in that log.
    pub first: u64,
    /// The sequence number of the last.
    pub last: u64,
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
    /// What a binary from before entries were withheld wrote: the entries that cite the
    /// erased one, which it flagged for review. Read from a receipt on disk, and written
    /// by no erasure now.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flagged: Vec<String>,
    /// The entries this erasure withheld from retrieval: each names the erased one, or
    /// was written by a session after that session read it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub withheld: Vec<String>,
    /// The entries further downstream, two steps of lineage or more from the erased
    /// one: listed for whoever follows up, and not withheld.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub further: Vec<String>,
    /// The sends in the lineage forward of the erased entry, at any depth: copies that
    /// left the perimeter and cannot be called back, with where each went.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sends: Vec<Sent>,
    /// Copies this erasure could not reach, named so nobody believes it did.
    #[serde(default)]
    pub unresolved: Vec<String>,
    /// How many records of the entry's promotions had their why removed from the
    /// ledger. Who promoted which version to which tier, and when, is kept.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub promotion_whys_removed: usize,
    /// How many approval tasks of the entry's promotions now name it by its id alone:
    /// their title and body were rewritten, and the open ones closed.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub promotion_tasks_cleared: usize,
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
            withheld: vec![],
            further: vec![],
            sends: vec![],
            unresolved: vec![],
            promotion_whys_removed: 0,
            promotion_tasks_cleared: 0,
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
            flagged: vec![],
            withheld: vec!["mem-me-00000002".into()],
            further: vec![],
            sends: vec![],
            unresolved: vec!["exports issued before the erasure".into()],
            promotion_whys_removed: 0,
            promotion_tasks_cleared: 0,
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

    /// An entry with every field set: what a stub must not repeat is each findable.
    const FULL: &str = "---\nid: mem-me-1\ntype: decision\ndomain: dom-me\nauthor: human:test:alice\ncreated: 2026-10-01T10:00:00Z\ncites:\n- https://example.test/x?token=s3cret\n- mem-me-0\nsession: sess-abc@7\nconfidence: high\ntrust_tier: verified\nreview_by: 2027-04-01\nsupersedes: mem-me-0\npolicy_context:\n- pol-L2-031\nclassification: restricted\norigin: github:example/repo#12\nsummary: the secret summary\ntitle: Alice's diagnosis\n---\nThe secret body.\n";

    /// What a stub holds of `e`: the id, the marker and the label, and the audit
    /// metadata, which repeats nothing of the entry.
    fn holds_the_id_the_marker_the_label_and_the_audit_fields(s: &Entry, e: &Entry) {
        assert_eq!(s.front.id, "mem-me-1");
        assert_eq!(s.front.status, Some(Status::Erased));
        assert_eq!(s.front.classification, crate::Label::Restricted);
        assert_eq!(s.front.entry_type, e.front.entry_type);
        assert_eq!(s.front.domain, "dom-me");
        assert_eq!(s.front.created, e.front.created);
        assert_eq!(s.front.supersedes.as_deref(), Some("mem-me-0"));
        assert_eq!(s.front.session.as_deref(), Some("sess-abc@7"));
    }

    /// What a stub does not hold, whatever the reason.
    fn holds_nothing_else_of_the_entry(s: &Entry, reason: Reason) {
        assert_eq!(s.body, "", "{reason}");
        assert_eq!(s.front.title, None, "{reason}: a title");
        assert_eq!(s.front.summary, None, "{reason}: a summary");
        assert!(s.front.cites.is_empty(), "{reason}: a citation");
        assert_eq!(s.front.origin, None, "{reason}: an origin");
        assert_eq!(s.front.author, "erased", "{reason}: an author");
        assert_eq!(s.front.trust_tier, TrustTier::Raw, "{reason}: a tier");
        assert_eq!(s.front.confidence, None, "{reason}");
        assert_eq!(s.front.review_by, None, "{reason}");
        assert!(s.front.policy_context.is_empty(), "{reason}");
    }

    #[test]
    fn a_stub_holds_the_id_the_marker_and_the_label_and_nothing_else_of_the_entry() {
        let e = Entry::parse(FULL).unwrap();
        for reason in [
            Reason::Requested,
            Reason::Mistaken,
            Reason::Sensitive,
            Reason::Legal,
        ] {
            let s = stub(&e, reason);
            holds_the_id_the_marker_the_label_and_the_audit_fields(&s, &e);
            holds_nothing_else_of_the_entry(&s, reason);
            let rendered = s.render();
            for gone in [
                "secret", "Alice", "alice", "example", "token", "github", "pol-L2", "2027",
            ] {
                assert!(!rendered.contains(gone), "{reason}: {gone} in {rendered}");
            }
            assert_eq!(Entry::parse(&rendered).unwrap(), s, "a stub round-trips");
        }
    }

    #[test]
    fn a_stub_written_before_the_stub_was_made_smaller_still_reads() {
        // as the stub of an entry erased for the reason `mistaken` was written: with its
        // title, its author and its citations
        let text = "---\nid: mem-me-1\ntype: fact\ndomain: dom-me\nauthor: human:test:me\ncreated: 2026-10-01T10:00:00Z\ncites:\n- https://example.test/x\ntrust_tier: curated\nclassification: internal\nstatus: erased\ntitle: A title\n---\n";
        let old = Entry::parse(text).unwrap();
        assert_eq!(old.front.status, Some(Status::Erased));
        assert_eq!(old.front.title.as_deref(), Some("A title"));
        assert_eq!(old.front.cites.len(), 1);
        assert_eq!(old.render(), text, "and is written again as it was");
    }

    #[test]
    fn a_receipt_written_before_entries_were_withheld_still_reads() {
        // as a binary from before wrote it: `flagged`, and none of the new fields
        let text = "id: mem-me-00000001\ndomain: dom-me\nerased_at: 1970-01-01T00:00:00Z\nerased_by: human:test:me\nreason: requested\nremoved:\n- version: current\n  digest: abab\nflagged:\n- mem-me-00000002\nunresolved:\n- exports issued before the erasure\n";
        let r = Receipt::from_yaml(text).unwrap();
        assert_eq!(r.flagged, ["mem-me-00000002"]);
        assert!(r.withheld.is_empty() && r.further.is_empty() && r.sends.is_empty());
        assert!(r.is_finished());
        assert_eq!(r.to_yaml(), text, "and is written again as it was");
    }

    #[test]
    fn a_receipt_names_what_it_withheld_what_is_further_and_each_send_and_no_empty_list() {
        let mut r = Receipt::from_yaml(
            "id: mem-me-1\ndomain: dom-me\nerased_at: 1970-01-01T00:00:00Z\nerased_by: human:test:me\nreason: legal\nremoved: []\n",
        )
        .unwrap();
        let bare = r.to_yaml();
        for absent in ["flagged", "withheld", "further", "sends"] {
            assert!(!bare.contains(absent), "{absent} in {bare}");
        }
        r.withheld = vec!["mem-me-2".into()];
        r.further = vec!["mem-me-3".into()];
        r.sends = vec![Sent {
            domain: "dom-me".into(),
            to: "export:internal".into(),
            session: "sess-a".into(),
            count: 2,
            first: 7,
            last: 9,
        }];
        let text = r.to_yaml();
        assert!(text.contains("withheld:\n- mem-me-2\n"), "{text}");
        assert!(text.contains("further:\n- mem-me-3\n"), "{text}");
        assert!(text.contains("to: export:internal"), "{text}");
        assert!(!text.contains("flagged"), "{text}");
        assert_eq!(Receipt::from_yaml(&text).unwrap(), r);
    }
}
