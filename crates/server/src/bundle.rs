//! Leaving and arriving: export a domain to a bundle, import a bundle into a domain.
//!
//! Export is a read under the caller's filter, bounded by a label. The bundle is a
//! directory of entries plus `manifest.yaml`, and the manifest carries the highest
//! classification inside it. Nothing leaves below its classification.
//!
//! Import is the mirror. Every entry lands at trust raw unless the same owner exported it,
//! keeps its provenance and classification, and is keyed on origin so a second import
//! creates nothing twice. A manifest path that is not a plain file name is refused before
//! anything is read, and an entry whose digest does not match its manifest line is
//! refused before anything is written.

use dm_core::erasure::body_digest;
use dm_core::manifest::{Manifest, ManifestEntry};
use dm_core::{digest, Entry, Label, ReadFilter, TrustTier};
use dm_index::{Hit, Index};
use dm_store::Store;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// What can go wrong at the bundle boundary.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The record.
    #[error(transparent)]
    Store(#[from] dm_store::Error),
    /// The index.
    #[error(transparent)]
    Index(#[from] dm_index::Error),
    /// An entry or manifest that does not parse.
    #[error(transparent)]
    Core(#[from] dm_core::Error),
    /// The bundle directory.
    #[error("bundle {path}: {source}")]
    Io {
        /// The path.
        path: String,
        /// Why.
        source: std::io::Error,
    },
    /// The bundle is not one this code reads.
    #[error("bundle: {0}")]
    Invalid(String),
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> Error + '_ {
    move |source| Error::Io {
        path: path.display().to_string(),
        source,
    }
}

/// The outcome of an export.
#[derive(Clone, Debug, PartialEq)]
pub struct Exported {
    /// The manifest as written.
    pub manifest: Manifest,
}

/// The outcome of an import.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Imported {
    /// Ids written, in bundle order.
    pub written: Vec<String>,
    /// Ids skipped because their origin was already in the index.
    pub skipped: Vec<String>,
    /// Ids refused because their text was erased from this domain: the receipt kept the
    /// digest, and a bundle from before the erasure still carries the body.
    pub erased: Vec<String>,
}

/// Where the binary keeps the bundles it serves, under the root's state.
pub const BUNDLES_DIR: &str = "state/bundles";

/// Where a domain's bundle at a label is exported for serving: one directory per
/// domain and label, replaced whole on each export.
#[must_use]
pub fn bundle_dir(root: &Path, domain: &str, label: Label) -> PathBuf {
    root.join(BUNDLES_DIR).join(format!("{domain}-{label}"))
}

/// What an export reads: `filter` narrowed to `domain`, bounded at `max_label`, with
/// tombstones and every tier, since a bundle carries the domain whole. Fails if the
/// filter does not cover the domain.
///
/// # Errors
///
/// [`Error::Invalid`] for a domain the filter does not admit.
pub fn export_scope(
    filter: &ReadFilter,
    domain: &str,
    max_label: Label,
) -> Result<ReadFilter, Error> {
    if !filter.domains.is_empty() && !filter.domains.iter().any(|d| d == domain) {
        return Err(Error::Invalid(format!(
            "domain {domain} is not readable here"
        )));
    }
    let mut scoped = filter.clone();
    scoped.domains = vec![domain.to_string()];
    scoped.handling = scoped.handling.min(max_label);
    scoped.include_inactive = true;
    scoped.min_tier = TrustTier::Raw;
    Ok(scoped)
}

/// Export `hits`, the entries [`export_scope`] listed for `domain`, into `dest`. The
/// index is consulted before this is called and never held across the record reads,
/// so the export can run on any executor thread. The manifest's word on each entry's
/// label is the record's, read from the text written, not the index's, which can lag
/// it: every receiver checks the manifest against the text, so an exporter must say
/// what the text says.
///
/// # Errors
///
/// A store or I/O error.
pub async fn export(
    store: &Store,
    hits: Vec<Hit>,
    domain: &str,
    max_label: Label,
    exported_by: &str,
    dest: &Path,
) -> Result<Exported, Error> {
    tokio::fs::create_dir_all(dest).await.map_err(io(dest))?;
    let mut manifest = Manifest::new(domain, exported_by, max_label);
    for hit in hits {
        let Some(text) = store.get_text(domain, &hit.id).await? else {
            continue;
        };
        let classification = Entry::parse(&text)?.front.classification;
        let file = format!("{}.md", hit.id);
        let path = dest.join(&file);
        tokio::fs::write(&path, &text).await.map_err(io(&path))?;
        manifest.push(ManifestEntry {
            id: hit.id.clone(),
            path: file,
            digest: digest(&text),
            classification,
        })?;
    }
    let mpath = dest.join("manifest.yaml");
    tokio::fs::write(&mpath, manifest.to_yaml())
        .await
        .map_err(io(&mpath))?;
    Ok(Exported { manifest })
}

/// Import a bundle from `src` into `into_domain`. `owner` is this record's owner; a bundle
/// the same owner exported keeps its trust tiers, every other bundle lands at raw. An
/// entry whose text's digest, or whose trimmed body's, is in `erased`, what this
/// domain's receipts list as removed, is refused and named rather than written back.
/// Recognition is by digest: a copy that was reworded is a new text, and the receipt
/// says so.
///
/// # Errors
///
/// A bad or missing manifest, a path that is not a plain file name, a digest that does
/// not match, a manifest whose labels are not the entries' own, an entry that does not
/// parse, or a store or index error.
pub async fn import(
    store: &Store,
    index: &mut Index,
    owner: &str,
    src: &Path,
    into_domain: &str,
    default_classification: Label,
    erased: &HashSet<String>,
) -> Result<Imported, Error> {
    let mpath = src.join("manifest.yaml");
    let mtext = std::fs::read_to_string(&mpath).map_err(io(&mpath))?;
    let manifest = Manifest::from_yaml(&mtext)?;
    if manifest.format != dm_core::manifest::FORMAT {
        return Err(Error::Invalid(format!(
            "format {:?}; this code reads {:?}",
            manifest.format,
            dm_core::manifest::FORMAT
        )));
    }
    let same_owner = manifest.exported_by == owner;

    // Pass one: every path and digest, before anything is written. A bundle with one bad
    // entry is refused whole, so a partial import never happens. Text this domain erased
    // is known by its digest and is not an error: the bundle is older than the erasure.
    let mut out = Imported::default();
    let mut checked: Vec<Entry> = Vec::with_capacity(manifest.entries.len());
    let mut inside: Vec<(String, Label)> = Vec::with_capacity(manifest.entries.len());
    for m in &manifest.entries {
        if m.path.contains(['/', '\\']) || m.path.starts_with('.') || m.path.is_empty() {
            return Err(Error::Invalid(format!(
                "manifest path {:?} is not a plain file name",
                m.path
            )));
        }
        let path = src.join(&m.path);
        let text = std::fs::read_to_string(&path).map_err(io(&path))?;
        if digest(&text) != m.digest {
            return Err(Error::Invalid(format!(
                "{} does not match its manifest digest",
                m.path
            )));
        }
        let entry = Entry::parse(&text)?;
        inside.push((entry.front.id.clone(), entry.front.classification));
        if erased.contains(&m.digest) || erased.contains(&body_digest(&entry.body)) {
            out.erased.push(m.id.clone());
            continue;
        }
        checked.push(entry);
    }
    // the manifest's word on the labels must be the entries' own, erased ones included,
    // before anything lands: a line or a highest that understates is refused whole
    manifest.check(&inside)?;

    // Pass two: land what is new.
    for mut entry in checked {
        let origin = entry.front.origin.clone().unwrap_or_else(|| {
            format!(
                "bundle:{}/{}/{}",
                manifest.exported_by, manifest.domain, entry.front.id
            )
        });
        if index.by_origin(into_domain, &origin)?.is_some() {
            out.skipped.push(entry.front.id.clone());
            continue;
        }
        entry.front.origin = Some(origin);
        if entry.front.domain != into_domain {
            entry.front.domain = into_domain.to_string();
        }
        if store.get(into_domain, &entry.front.id).await?.is_some() {
            entry.front.id = dm_core::id::new_entry_id(into_domain);
        }
        if !same_owner {
            entry.front.trust_tier = TrustTier::Raw;
        }
        entry.front.classification = entry.front.classification.max(default_classification);
        store.put(&entry).await?;
        index.upsert(&entry)?;
        out.written.push(entry.front.id.clone());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HELD: &str = "---\nid: mem-a-held0001\ntype: fact\ndomain: dom-a\nauthor: human:test:me\ncreated: 2026-10-01T09:00:00Z\ntrust_tier: curated\nclassification: restricted\n---\n# Held\n\nHeld.\n";

    #[tokio::test]
    async fn an_export_s_manifest_speaks_for_the_record_not_the_index() {
        // the index can lag the record, which is what reindex exists for; the manifest
        // every receiver now checks against the text must be written from the text, or
        // an exporter hands out a bundle every receiver refuses
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_dir(dir.path()).unwrap();
        let entry = Entry::parse(HELD).unwrap();
        store.put(&entry).await.unwrap();
        let stale = Hit {
            id: entry.front.id.clone(),
            domain: "dom-a".into(),
            entry_type: entry.front.entry_type,
            trust_tier: entry.front.trust_tier,
            classification: Label::Public,
            title: "Held".into(),
            summary: "Held.".into(),
            score: 0.0,
        };
        let dest = tempfile::tempdir().unwrap();
        let ex = export(
            &store,
            vec![stale],
            "dom-a",
            Label::Restricted,
            "memory:test:me",
            dest.path(),
        )
        .await
        .unwrap();
        assert_eq!(ex.manifest.entries[0].classification, Label::Restricted);
        assert_eq!(ex.manifest.highest_label, Label::Restricted);
        let written = Manifest::from_yaml(
            &std::fs::read_to_string(dest.path().join("manifest.yaml")).unwrap(),
        )
        .unwrap();
        written
            .check(&[(entry.front.id.clone(), Label::Restricted)])
            .unwrap();
    }
}
