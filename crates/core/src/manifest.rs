//! The bundle manifest: what an export carries besides the entries themselves.
//!
//! A bundle is a directory of entries plus `manifest.yaml`. The manifest names
//! every entry with its digest and classification, and carries the highest
//! classification inside the bundle, so a destination can be checked against it.

use crate::Label;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// One entry as the manifest lists it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ManifestEntry {
    /// The entry's id.
    pub id: String,
    /// Path inside the bundle, relative to the manifest.
    pub path: String,
    /// Digest of the rendered entry, as [`crate::digest`] computes it.
    pub digest: String,
    /// The entry's classification, so a receiver can filter without opening it.
    pub classification: Label,
}

/// The manifest at the root of a bundle.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    /// The bundle format, [`FORMAT`].
    pub format: String,
    /// The domain the bundle was exported from.
    pub domain: String,
    /// When the export was made.
    #[serde(with = "time::serde::rfc3339")]
    pub exported_at: OffsetDateTime,
    /// The exporting server, system-qualified, for provenance on import.
    pub exported_by: String,
    /// The bound the export was asked for.
    pub max_label: Label,
    /// The highest classification actually inside the bundle.
    pub highest_label: Label,
    /// Every entry in the bundle.
    pub entries: Vec<ManifestEntry>,
}

/// The current bundle format identifier. A format change bumps the trailing number.
pub const FORMAT: &str = "durable-memory/bundle/0";

impl Manifest {
    /// An empty manifest for `domain`, bounded at `max_label`.
    #[must_use]
    pub fn new(domain: &str, exported_by: &str, max_label: Label) -> Manifest {
        Manifest {
            format: FORMAT.into(),
            domain: domain.into(),
            exported_at: OffsetDateTime::now_utc(),
            exported_by: exported_by.into(),
            max_label,
            highest_label: Label::Public,
            entries: Vec::new(),
        }
    }

    /// Add an entry; refuses anything above the requested bound.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Invalid`] when the entry's classification exceeds `max_label`.
    pub fn push(&mut self, e: ManifestEntry) -> Result<(), crate::Error> {
        if e.classification > self.max_label {
            return Err(crate::Error::Invalid(format!(
                "{} is {} which exceeds the export bound {}",
                e.id, e.classification, self.max_label
            )));
        }
        self.highest_label = self.highest_label.max(e.classification);
        self.entries.push(e);
        Ok(())
    }

    /// Whether the manifest agrees with itself: `highest_label` is the highest of its
    /// lines, public when there are none, and within `max_label`. A manifest that
    /// understates its highest would pass a bundle holding more than it says through a
    /// destination's check on the manifest's word, so a receiver refuses one before it
    /// opens an entry.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Invalid`] naming the field that disagrees.
    pub fn consistent(&self) -> Result<(), crate::Error> {
        let inside = self
            .entries
            .iter()
            .map(|e| e.classification)
            .max()
            .unwrap_or(Label::Public);
        if self.highest_label != inside {
            return Err(crate::Error::Invalid(format!(
                "highest_label is {} but the entries listed reach {}",
                self.highest_label, inside
            )));
        }
        if self.highest_label > self.max_label {
            return Err(crate::Error::Invalid(format!(
                "highest_label {} exceeds the export bound max_label {}",
                self.highest_label, self.max_label
            )));
        }
        Ok(())
    }

    /// Whether the manifest says what is inside the bundle: each line's file carries
    /// the line's id and, in its frontmatter, the line's classification; no id is listed
    /// twice; and the manifest is [`Manifest::consistent`]. `inside` is one pair per
    /// line, in the manifest's order, each the id and frontmatter classification read
    /// from the file that line names: a line is held to its own file, never to whichever
    /// entry carries the id it claims. A disagreement refuses the bundle whole, so a
    /// receiver filtering on the manifest's word never admits more than the record
    /// would have served.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Invalid`] naming the first line that disagrees, or the field.
    pub fn check(&self, inside: &[(String, Label)]) -> Result<(), crate::Error> {
        if inside.len() != self.entries.len() {
            return Err(crate::Error::Invalid(format!(
                "the manifest lists {} entries and the bundle carries {}",
                self.entries.len(),
                inside.len()
            )));
        }
        let mut seen = std::collections::BTreeSet::new();
        for (line, (id, front)) in self.entries.iter().zip(inside) {
            if !seen.insert(line.id.as_str()) {
                return Err(crate::Error::Invalid(format!(
                    "{} is listed twice in the manifest",
                    line.id
                )));
            }
            if *id != line.id {
                return Err(crate::Error::Invalid(format!(
                    "{} is listed as {} but the file carries the id {}",
                    line.path, line.id, id
                )));
            }
            if *front != line.classification {
                return Err(crate::Error::Invalid(format!(
                    "{} is {} in its frontmatter but the manifest lists it as {}",
                    line.id, front, line.classification
                )));
            }
        }
        self.consistent()
    }

    /// Render as YAML.
    ///
    /// # Panics
    ///
    /// Only if [`Manifest`] stops being serialisable, which a test guards.
    #[must_use]
    pub fn to_yaml(&self) -> String {
        serde_yaml::to_string(self).expect("manifest serialises")
    }

    /// Parse from YAML.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Yaml`] when the text does not fit [`Manifest`].
    pub fn from_yaml(s: &str) -> Result<Manifest, crate::Error> {
        Ok(serde_yaml::from_str(s)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bound_and_highest() {
        let mut m = Manifest::new("dom-a", "memory:local:me", Label::Internal);
        m.push(ManifestEntry {
            id: "mem-a-1".into(),
            path: "mem-a-1.md".into(),
            digest: "00".into(),
            classification: Label::Public,
        })
        .unwrap();
        assert_eq!(m.highest_label, Label::Public);
        m.push(ManifestEntry {
            id: "mem-a-2".into(),
            path: "mem-a-2.md".into(),
            digest: "01".into(),
            classification: Label::Internal,
        })
        .unwrap();
        assert_eq!(m.highest_label, Label::Internal);
        let err = m.push(ManifestEntry {
            id: "mem-a-3".into(),
            path: "mem-a-3.md".into(),
            digest: "02".into(),
            classification: Label::Restricted,
        });
        assert!(err.is_err());
        let back = Manifest::from_yaml(&m.to_yaml()).unwrap();
        assert_eq!(back.entries.len(), 2);
        assert_eq!(back.highest_label, Label::Internal);
    }

    fn two() -> Manifest {
        let mut m = Manifest::new("dom-a", "memory:local:me", Label::Internal);
        for (n, label) in [("1", Label::Public), ("2", Label::Internal)] {
            m.push(ManifestEntry {
                id: format!("mem-a-{n}"),
                path: format!("mem-a-{n}.md"),
                digest: "00".into(),
                classification: label,
            })
            .unwrap();
        }
        m
    }

    #[test]
    fn a_manifest_says_what_is_inside_it() {
        let inside =
            |a: Label, b: Label| vec![("mem-a-1".to_string(), a), ("mem-a-2".to_string(), b)];
        // as exported: every line is its frontmatter, and the highest is the highest
        two()
            .check(&inside(Label::Public, Label::Internal))
            .unwrap();
        assert!(two().consistent().is_ok());
        // the highest understated: a destination checking the manifest's word would
        // admit a bundle holding more than it says
        let mut m = two();
        m.highest_label = Label::Public;
        let e = m.consistent().unwrap_err().to_string();
        assert!(e.contains("highest_label") && e.contains("internal"), "{e}");
        assert!(m.check(&inside(Label::Public, Label::Internal)).is_err());
        // overstated is not what is inside it either
        let mut m = two();
        m.highest_label = Label::Restricted;
        assert!(m.consistent().is_err());
        // above the bound the export was asked for
        let mut m = two();
        m.max_label = Label::Public;
        assert!(m.consistent().is_err());
        // a line that understates its entry: the frontmatter says more than the manifest
        let e = two()
            .check(&inside(Label::Public, Label::Restricted))
            .unwrap_err()
            .to_string();
        assert!(e.contains("mem-a-2") && e.contains("restricted"), "{e}");
        // a line that overstates it is a mismatch too
        assert!(two()
            .check(&inside(Label::Internal, Label::Internal))
            .is_err());
        // an entry the manifest lists but the bundle's text names otherwise
        let e = two()
            .check(&[
                ("mem-a-1".to_string(), Label::Public),
                ("mem-a-9".to_string(), Label::Internal),
            ])
            .unwrap_err()
            .to_string();
        assert!(e.contains("mem-a-2"), "{e}");
        // `inside` is one per line, in the manifest's order: a line that borrows another
        // entry's id and label, while its path names a file carrying more, is held to
        // the file it names, not to whichever entry carries the id it claims
        let mut borrowed = two();
        borrowed.entries[1].id = "mem-a-1".into();
        borrowed.entries[1].classification = Label::Public;
        borrowed.highest_label = Label::Public;
        assert!(borrowed.consistent().is_ok(), "to itself it agrees");
        let e = borrowed
            .check(&[
                ("mem-a-1".to_string(), Label::Public),
                ("mem-a-2".to_string(), Label::Internal),
            ])
            .unwrap_err()
            .to_string();
        assert!(e.contains("mem-a-2") || e.contains("mem-a-1"), "{e}");
        // the same id listed twice is two words for one entry
        let mut twice = two();
        twice.entries[1].id = "mem-a-1".into();
        twice.entries[1].classification = Label::Public;
        twice.highest_label = Label::Public;
        assert!(twice
            .check(&[
                ("mem-a-1".to_string(), Label::Public),
                ("mem-a-1".to_string(), Label::Public),
            ])
            .is_err());
        // more or fewer entries than lines is not the bundle the manifest describes
        assert!(two()
            .check(&[("mem-a-1".to_string(), Label::Public)])
            .is_err());
        // nothing inside: public, and nothing else
        let empty = Manifest::new("dom-a", "memory:local:me", Label::Internal);
        empty.check(&[]).unwrap();
        let mut empty = Manifest::new("dom-a", "memory:local:me", Label::Internal);
        empty.highest_label = Label::Internal;
        assert!(empty.consistent().is_err());
    }
}
