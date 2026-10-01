//! The bundle manifest: what an export carries besides the entries themselves.
//!
//! A bundle is a directory of entries plus `manifest.yaml`. The manifest names
//! every entry with its digest and classification, and carries the highest
//! classification inside the bundle, so a destination can be checked against it.

use crate::Label;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub id: String,
    /// Path inside the bundle, relative to the manifest.
    pub path: String,
    pub digest: String,
    pub classification: Label,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub format: String,
    pub domain: String,
    #[serde(with = "time::serde::rfc3339")]
    pub exported_at: OffsetDateTime,
    /// The exporting server, system-qualified, for provenance on import.
    pub exported_by: String,
    /// The bound the export was asked for.
    pub max_label: Label,
    /// The highest classification actually inside the bundle.
    pub highest_label: Label,
    pub entries: Vec<ManifestEntry>,
}

pub const FORMAT: &str = "durable-memory/bundle/0";

impl Manifest {
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

    pub fn to_yaml(&self) -> String {
        serde_yaml::to_string(self).expect("manifest serialises")
    }

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
