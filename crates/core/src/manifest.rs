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
}
