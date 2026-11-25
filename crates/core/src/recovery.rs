//! The manifest of a recovery bundle: `recovery.yaml`.
//!
//! A recovery bundle is a copy of a root that a restore can serve from again: the record,
//! the log of each domain with its cursors, the events of the ledger, the writes held in
//! quarantine, and the configuration. The manifest says where each of the two stores
//! stood when the copy was made, and gives a digest for each file, so a restore can show
//! that the bundle is whole before it writes anything.
//!
//! A recovery bundle is not the export bundle of [`crate::manifest`]. An export carries
//! the entries of one domain up to a classification, for another owner to import. A
//! recovery bundle carries everything, for the same owner to restore.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

/// The version of the recovery bundle layout. A restore reads no other.
pub const FORMAT: u32 = 1;

/// The name of the manifest file in a recovery bundle.
pub const FILE: &str = "recovery.yaml";

/// Where the log of one domain and its two cursors stood when the bundle was made.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainCheckpoint {
    /// The domain.
    pub domain: String,
    /// The sequence number of the last event in the log of the domain.
    pub log_seq: u64,
    /// The sequence number that the index was fed up to. The index is not in the bundle;
    /// events of the log above this number name versions that the index did not hold.
    pub index_fed: u64,
    /// The number of the last event of the ledger that the log holds a copy of.
    pub ledger_copied: u64,
}

/// Where the ledger stood when the bundle was made.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerCheckpoint {
    /// How many events the bundle holds.
    pub events: u64,
    /// The highest sequence number among them, 0 when there are none.
    pub last_seq: u64,
    /// The claim epoch in force. A restore begins a later one.
    pub epoch: u64,
}

/// One file of the bundle.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundledFile {
    /// The path below the bundle directory, with `/` between its parts.
    pub path: String,
    /// The SHA-256 of the bytes of the file, in hex.
    pub digest: String,
}

/// The manifest of a recovery bundle.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecoveryManifest {
    /// The layout version: [`FORMAT`].
    pub format: u32,
    /// When the bundle was made.
    #[serde(with = "time::serde::rfc3339")]
    pub made_at: OffsetDateTime,
    /// Who made it.
    pub made_by: String,
    /// Each domain of the bundle.
    pub domains: Vec<DomainCheckpoint>,
    /// The ledger.
    pub ledger: LedgerCheckpoint,
    /// Every file of the bundle except the manifest.
    pub files: Vec<BundledFile>,
}

/// The SHA-256 of `bytes`, in hex: the digest that a [`BundledFile`] carries.
#[must_use]
pub fn digest_bytes(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Whether `path` is a path below the bundle directory and nothing else: relative, with
/// `/` between its parts, and no part that is empty, `.` or `..`.
fn stays_inside(path: &str) -> bool {
    !path.is_empty()
        && !path.contains('\\')
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

impl RecoveryManifest {
    /// Serialise to YAML.
    ///
    /// # Panics
    ///
    /// Only if [`RecoveryManifest`] stops being serialisable, which a test guards.
    #[must_use]
    pub fn to_yaml(&self) -> String {
        serde_norway::to_string(self).expect("a recovery manifest serialises")
    }

    /// Parse from YAML.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Yaml`] when the text does not fit [`RecoveryManifest`].
    pub fn from_yaml(s: &str) -> Result<RecoveryManifest, crate::Error> {
        Ok(serde_norway::from_str(s)?)
    }

    /// Whether a restore can use this manifest.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Invalid`] for a format other than [`FORMAT`]; for a path that is
    /// absolute, holds `..`, a backslash or an empty part, or is listed twice; for a
    /// domain whose index was fed past the end of its log; and for a domain whose log
    /// holds a copy of a ledger event above the last one of the ledger.
    pub fn usable(&self) -> Result<(), crate::Error> {
        let refuse = |why: String| Err(crate::Error::Invalid(format!("recovery bundle: {why}")));
        if self.format != FORMAT {
            return refuse(format!("format {}; this code reads {FORMAT}", self.format));
        }
        let mut seen = std::collections::BTreeSet::new();
        for file in &self.files {
            if !stays_inside(&file.path) {
                return refuse(format!(
                    "the path {:?} does not stay in the bundle",
                    file.path
                ));
            }
            if !seen.insert(file.path.as_str()) {
                return refuse(format!("the path {:?} is listed twice", file.path));
            }
        }
        for d in &self.domains {
            if d.index_fed > d.log_seq {
                return refuse(format!(
                    "the index of {} was fed up to {}, and its log ends at {}",
                    d.domain, d.index_fed, d.log_seq
                ));
            }
            if d.ledger_copied > self.ledger.last_seq {
                return refuse(format!(
                    "the log of {} copied the ledger up to {}, and the ledger ends at {}",
                    d.domain, d.ledger_copied, self.ledger.last_seq
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn manifest() -> RecoveryManifest {
        RecoveryManifest {
            format: FORMAT,
            made_at: datetime!(2026-11-24 10:00 UTC),
            made_by: "human:test:me".into(),
            domains: vec![DomainCheckpoint {
                domain: "dom-a".into(),
                log_seq: 9,
                index_fed: 7,
                ledger_copied: 4,
            }],
            ledger: LedgerCheckpoint {
                events: 5,
                last_seq: 5,
                epoch: 2,
            },
            files: vec![
                BundledFile {
                    path: "record/dom-a/mem-a-1.md".into(),
                    digest: digest_bytes(b"one"),
                },
                BundledFile {
                    path: "ledger.jsonl".into(),
                    digest: digest_bytes(b"two"),
                },
            ],
        }
    }

    #[test]
    fn a_manifest_round_trips_and_is_usable() {
        let m = manifest();
        assert_eq!(RecoveryManifest::from_yaml(&m.to_yaml()).unwrap(), m);
        m.usable().unwrap();
    }

    #[test]
    fn the_digest_of_a_file_is_the_digest_of_its_text() {
        assert_eq!(digest_bytes("text".as_bytes()), crate::digest("text"));
    }

    fn refused(m: &RecoveryManifest, word: &str) {
        let said = m.usable().unwrap_err().to_string();
        assert!(said.contains(word), "{said}");
    }

    #[test]
    fn another_format_is_refused() {
        let mut m = manifest();
        m.format = FORMAT + 1;
        refused(&m, "format");
    }

    #[test]
    fn a_path_that_leaves_the_bundle_is_refused() {
        for path in [
            "/etc/passwd",
            "../x",
            "record/../../x",
            "record\\x",
            "record//x",
            "record/./x",
            "record/",
            "",
        ] {
            let mut m = manifest();
            m.files[0].path = path.into();
            refused(&m, "does not stay in the bundle");
        }
    }

    #[test]
    fn a_path_listed_twice_is_refused() {
        let mut m = manifest();
        let first = m.files[0].path.clone();
        m.files[1].path = first;
        refused(&m, "listed twice");
    }

    #[test]
    fn an_index_fed_past_the_end_of_its_log_is_refused() {
        let mut m = manifest();
        m.domains[0].index_fed = 9;
        m.usable().unwrap();
        m.domains[0].index_fed = 10;
        refused(&m, "was fed up to 10");
    }

    #[test]
    fn a_log_that_copied_a_ledger_event_the_ledger_does_not_hold_is_refused() {
        let mut m = manifest();
        m.domains[0].ledger_copied = 5;
        m.usable().unwrap();
        m.domains[0].ledger_copied = 6;
        refused(&m, "copied the ledger up to 6");
    }
}
