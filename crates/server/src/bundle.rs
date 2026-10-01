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

use dm_core::manifest::{Manifest, ManifestEntry};
use dm_core::{digest, Entry, Label, ReadFilter, TrustTier};
use dm_index::Index;
use dm_store::Store;
use std::path::Path;

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
}

/// Export every entry in `domain` that `filter` admits at or below `max_label`, including
/// tombstones, into `dest`. Fails before writing if the filter does not cover the domain.
///
/// # Errors
///
/// A store, index, or I/O error, or a domain the filter does not admit.
pub async fn export(
    store: &Store,
    index: &Index,
    filter: &ReadFilter,
    domain: &str,
    max_label: Label,
    exported_by: &str,
    dest: &Path,
) -> Result<Exported, Error> {
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

    std::fs::create_dir_all(dest).map_err(io(dest))?;
    let mut manifest = Manifest::new(domain, exported_by, max_label);
    for hit in index.list(&scoped, None, usize::MAX / 2)? {
        let Some(text) = store.get_text(domain, &hit.id).await? else {
            continue;
        };
        let file = format!("{}.md", hit.id);
        let path = dest.join(&file);
        std::fs::write(&path, &text).map_err(io(&path))?;
        manifest.push(ManifestEntry {
            id: hit.id.clone(),
            path: file,
            digest: digest(&text),
            classification: hit.classification,
        })?;
    }
    let mpath = dest.join("manifest.yaml");
    std::fs::write(&mpath, manifest.to_yaml()).map_err(io(&mpath))?;
    Ok(Exported { manifest })
}

/// Import a bundle from `src` into `into_domain`. `owner` is this record's owner; a bundle
/// the same owner exported keeps its trust tiers, every other bundle lands at raw.
///
/// # Errors
///
/// A bad or missing manifest, a path that is not a plain file name, a digest that does
/// not match, an entry that does not parse, or a store or index error.
pub async fn import(
    store: &Store,
    index: &mut Index,
    owner: &str,
    src: &Path,
    into_domain: &str,
    default_classification: Label,
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
    // entry is refused whole, so a partial import never happens.
    let mut checked: Vec<Entry> = Vec::with_capacity(manifest.entries.len());
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
        checked.push(Entry::parse(&text)?);
    }

    // Pass two: land what is new.
    let mut out = Imported::default();
    for mut entry in checked {
        let origin = entry.front.origin.clone().unwrap_or_else(|| {
            format!(
                "bundle:{}/{}/{}",
                manifest.exported_by, manifest.domain, entry.front.id
            )
        });
        if index.by_origin(&origin)?.is_some() {
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
