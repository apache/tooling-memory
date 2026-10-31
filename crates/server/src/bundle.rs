//! Leaving and arriving: export a domain to a bundle, import a bundle into a domain.
//!
//! Export is a read under the caller's filter, bounded by a label. The bundle is a
//! directory of entries plus `manifest.yaml`, and the manifest carries the highest
//! classification inside it. Nothing leaves below its classification.
//!
//! Import is the mirror. Every entry lands at trust raw unless the same owner exported it,
//! keeps its provenance and classification, and is keyed on origin so a second import
//! creates nothing twice. Why its entries were promoted travels with them: the records
//! about entries exported, at or below the bound, in a file the manifest holds the digest
//! of; an import keeps them where it keeps the tiers, from the same owner, and leaves them
//! behind where the entries land raw. A manifest path that is not a plain file name is refused before
//! anything is read, and an entry whose digest does not match its manifest line is
//! refused before anything is written.

use dm_core::erasure::body_digest;
use dm_core::manifest::{Manifest, ManifestEntry, ManifestPromotion, ManifestPromotions};
use dm_core::{digest, Entry, Label, ReadFilter, TrustTier};
use dm_index::{Hit, Index};
use dm_ledger::PromotionRecord;
use dm_store::Store;
use std::collections::{BTreeMap, HashSet};
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

/// Where an import's writes are logged: the root, and who writes.
#[derive(Clone, Copy, Debug)]
pub struct LogTo<'a> {
    /// The root, whose `state/log` holds the domain's log.
    pub root: &'a Path,
    /// The author the log names.
    pub by: &'a str,
}

/// Whether this code reads the bundle's format.
fn format_read(manifest: &Manifest) -> Result<(), Error> {
    if manifest.format == dm_core::manifest::FORMAT {
        return Ok(());
    }
    Err(Error::Invalid(format!(
        "format {:?}; this code reads {:?}",
        manifest.format,
        dm_core::manifest::FORMAT
    )))
}

impl LogTo<'_> {
    fn put(self, domain: &str, entry: &Entry) -> Result<(), Error> {
        crate::log::append(
            self.root,
            domain,
            self.by,
            crate::log::Change::Put {
                id: entry.front.id.clone(),
                digest: digest(&entry.render()),
                active: entry.is_active(),
            },
        )
        .map(|_| ())
        .map_err(|e| Error::Invalid(format!("the log: {e}")))
    }
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
    /// For each entry written, the id it landed under, keyed by the bundle's: the same
    /// unless the bundle's id was already in use here.
    pub landed: BTreeMap<String, String>,
    /// Transcript turns restored as excerpts of the entries written.
    pub excerpts: usize,
    /// Where the bundle came from: `bundle:<exporter>/<domain>`.
    pub from: String,
    /// The promotion records to restore, each moved to its entry as it stands here: the
    /// entries written, and those skipped because they are here already, whose records
    /// an earlier import may not have reached. From the same owner's bundle only. Another
    /// owner's entries land raw, and the reasons for the tier they lost are left behind.
    pub promotions: Vec<PromotionRecord>,
}

/// Where a bundle carries the promotion records of its entries.
pub const PROMOTIONS_FILE: &str = "promotions.json";

/// Refuse a manifest path that is not a plain file name in the bundle's directory.
fn plain_name(path: &str) -> Result<(), Error> {
    if path.contains(['/', '\\']) || path.starts_with('.') || path.is_empty() {
        return Err(Error::Invalid(format!(
            "manifest path {path:?} is not a plain file name"
        )));
    }
    Ok(())
}

/// Write the records about entries the manifest lists, at or below its bound, and list
/// them in it. Nothing is written when there are none.
async fn carry_promotions(
    manifest: &mut Manifest,
    records: Vec<PromotionRecord>,
    dest: &Path,
) -> Result<(), Error> {
    let carried: Vec<PromotionRecord> = records
        .into_iter()
        .filter(|r| {
            r.spec.classification <= manifest.max_label
                && manifest.entries.iter().any(|e| e.id == r.spec.entry)
        })
        .collect();
    if carried.is_empty() {
        return Ok(());
    }
    let text = serde_json::to_string_pretty(&carried)
        .map_err(|e| Error::Invalid(format!("promotion records: {e}")))?;
    let path = dest.join(PROMOTIONS_FILE);
    tokio::fs::write(&path, &text).await.map_err(io(&path))?;
    manifest.set_promotions(ManifestPromotions {
        path: PROMOTIONS_FILE.into(),
        digest: digest(&text),
        records: carried
            .iter()
            .map(|r| ManifestPromotion {
                entry: r.spec.entry.clone(),
                classification: r.spec.classification,
            })
            .collect(),
    })?;
    Ok(())
}

/// The promotion records a bundle carries, checked before anything is written: a plain
/// file name, its digest, and one record per manifest line, about the entry and at the
/// label the line says. A bundle with one bad record is refused whole.
fn carried_promotions(src: &Path, manifest: &Manifest) -> Result<Vec<PromotionRecord>, Error> {
    let Some(listed) = &manifest.promotions else {
        return Ok(Vec::new());
    };
    plain_name(&listed.path)?;
    let path = src.join(&listed.path);
    let text = std::fs::read_to_string(&path).map_err(io(&path))?;
    if digest(&text) != listed.digest {
        return Err(Error::Invalid(format!(
            "{} does not match its manifest digest",
            listed.path
        )));
    }
    let records: Vec<PromotionRecord> =
        serde_json::from_str(&text).map_err(|e| Error::Invalid(format!("{}: {e}", listed.path)))?;
    let inside: Vec<(String, Label)> = records
        .iter()
        .map(|r| (r.spec.entry.clone(), r.spec.classification))
        .collect();
    manifest.check_promotions(&inside)?;
    Ok(records)
}

/// An entry of the bundle as it stands here after the import: written now, or here
/// already from before. Its records are moved to it.
struct Placed {
    /// The id the bundle names it by.
    bundle_id: String,
    /// The id it has here.
    here: String,
    /// The digest of its text in the bundle.
    bundle_digest: String,
    /// The digest of its text here: not the bundle's, since landing sets the origin and
    /// may change the id, the domain and the classification.
    digest: String,
    /// Its classification here, which an import may have raised to the domain's default.
    classification: Label,
}

/// The records to restore: those about an entry that is here, moved to it as it stands.
/// A record of the version the bundle carries names the version as it landed, so it
/// reads as landed; a record is at least as classified as its entry, as the ledger keeps
/// it. A record about an entry that is not here, refused or never written, is left out.
fn landed_promotions(
    carried: Vec<PromotionRecord>,
    placed: &[Placed],
    into_domain: &str,
) -> Vec<PromotionRecord> {
    carried
        .into_iter()
        .filter_map(|mut r| {
            let p = placed.iter().find(|p| p.bundle_id == r.spec.entry)?;
            r.spec.entry.clone_from(&p.here);
            r.spec.domain = into_domain.to_string();
            if r.spec.new_digest == p.bundle_digest {
                r.spec.new_digest.clone_from(&p.digest);
            }
            r.spec.classification = r.spec.classification.max(p.classification);
            Some(r)
        })
        .collect()
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

/// Export `hits`, the entries [`export_scope`] listed for `domain`, into `dest`, with
/// the `promotions` of those exported at or below the bound. The
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
    promotions: Vec<PromotionRecord>,
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
    carry_promotions(&mut manifest, promotions, dest).await?;
    let mpath = dest.join("manifest.yaml");
    tokio::fs::write(&mpath, manifest.to_yaml())
        .await
        .map_err(io(&mpath))?;
    Ok(Exported { manifest })
}

/// Whether an origin that names this record and `domain` names an entry that is here:
/// `id` is what followed the record's own prefix, written by whoever made the bundle, so
/// one that is not a name is not ours rather than a refusal of the whole bundle.
async fn is_own(store: &Store, domain: &str, id: Option<&str>) -> Result<bool, Error> {
    let Some(id) = id else {
        return Ok(false);
    };
    match store.get(domain, id).await {
        Ok(found) => Ok(found.is_some()),
        Err(dm_store::Error::BadName(_)) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// The id a bundle entry with `origin` has here, when it is here: this domain's own,
/// come back in a bundle it left in (`home` is the prefix of such an origin), or one a
/// previous import landed, known by its origin.
async fn present_here(
    store: &Store,
    index: &Index,
    into_domain: &str,
    origin: &str,
    home: &str,
) -> Result<Option<String>, Error> {
    let own = origin.strip_prefix(home);
    if is_own(store, into_domain, own).await? {
        return Ok(own.map(str::to_string));
    }
    Ok(index.by_origin(into_domain, origin)?)
}

/// Where a bundle entry that is here already stands, for its records to be moved to:
/// `None` for an erased one, whose records keep nothing of it.
async fn placed_here(
    store: &Store,
    into_domain: &str,
    bundle_id: &str,
    here: String,
    bundle_digest: String,
) -> Result<Option<Placed>, Error> {
    let Some(e) = store.get(into_domain, &here).await? else {
        return Ok(None);
    };
    if e.front.status == Some(dm_core::Status::Erased) {
        return Ok(None);
    }
    Ok(Some(Placed {
        bundle_id: bundle_id.to_string(),
        here,
        bundle_digest,
        digest: digest(&e.render()),
        classification: e.front.classification,
    }))
}

/// Pass one of an import: every path and digest, before anything is written, so a bundle
/// with one bad entry is refused whole and a partial import never happens. Text this
/// domain erased is known by its digest and is not an error, the bundle being older than
/// the erasure: it is named on `out` and left out. The manifest's word on the labels must
/// be the entries' own, erased ones included: a line or a highest that understates is
/// refused whole. Returns each entry to land with the digest of its text in the bundle,
/// which its records name.
fn check_entries(
    src: &Path,
    manifest: &Manifest,
    erased: &HashSet<String>,
    out: &mut Imported,
) -> Result<Vec<(Entry, String)>, Error> {
    let mut checked: Vec<(Entry, String)> = Vec::with_capacity(manifest.entries.len());
    let mut inside: Vec<(String, Label)> = Vec::with_capacity(manifest.entries.len());
    for m in &manifest.entries {
        plain_name(&m.path)?;
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
        checked.push((entry, m.digest.clone()));
    }
    manifest.check(&inside)?;
    Ok(checked)
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
#[allow(clippy::too_many_arguments)] // the stores, the log, and what the import is told
pub async fn import(
    store: &Store,
    index: &mut Index,
    log: LogTo<'_>,
    owner: &str,
    src: &Path,
    into_domain: &str,
    default_classification: Label,
    erased: &HashSet<String>,
) -> Result<Imported, Error> {
    let mpath = src.join("manifest.yaml");
    let mtext = std::fs::read_to_string(&mpath).map_err(io(&mpath))?;
    let manifest = Manifest::from_yaml(&mtext)?;
    format_read(&manifest)?;
    let same_owner = manifest.exported_by == owner;

    let mut out = Imported {
        from: format!("bundle:{}/{}", manifest.exported_by, manifest.domain),
        ..Imported::default()
    };
    let checked = check_entries(src, &manifest, erased, &mut out)?;
    let carried = carried_promotions(src, &manifest)?;

    // Pass two: land what is new. An entry is already here when a previous import landed
    // it, known by its origin, or when it is this domain's own, come back in a bundle it
    // left in: its origin names this record and this domain, and the id it names is here.
    // Presence is all that is compared: a bundle carrying another version of an entry
    // this domain holds is skipped, not landed as a version, since the record's versions
    // are written here and a bundle is a copy.
    let home = format!("bundle:{owner}/{into_domain}/");
    let mut placed: Vec<Placed> = Vec::with_capacity(checked.len());
    for (mut entry, bundle_digest) in checked {
        let origin = entry.front.origin.clone().unwrap_or_else(|| {
            format!(
                "bundle:{}/{}/{}",
                manifest.exported_by, manifest.domain, entry.front.id
            )
        });
        if let Some(here) = present_here(store, index, into_domain, &origin, &home).await? {
            // its records may still be owed, when the import that landed it stopped
            // before they were restored; what is here says what they are about
            if same_owner {
                placed.extend(
                    placed_here(store, into_domain, &entry.front.id, here, bundle_digest).await?,
                );
            }
            out.skipped.push(entry.front.id.clone());
            continue;
        }
        entry.front.origin = Some(origin);
        if entry.front.domain != into_domain {
            entry.front.domain = into_domain.to_string();
        }
        // an id names one entry across the record, whichever domain holds it: a bundle's
        // id that is in use here, in this domain or another, lands under a new one. The
        // record is asked, since the index can lag it and is what a reindex rebuilds.
        let bundle_id = entry.front.id.clone();
        if store.holds(&entry.front.id).await? {
            entry.front.id = dm_core::id::new_entry_id(into_domain);
        }
        if !same_owner {
            entry.front.trust_tier = TrustTier::Raw;
        }
        entry.front.classification = entry.front.classification.max(default_classification);
        // through the domain's log, as every write of an entry goes
        log.put(into_domain, &entry)?;
        store.put(&entry).await?;
        index.upsert(&entry)?;
        // the entry it replaces, which the store just marked superseded
        if let Some(older) = &entry.front.supersedes {
            if let Some(old) = store.get(&entry.front.domain, older).await? {
                index.upsert(&old)?;
            }
        }
        out.landed.insert(bundle_id.clone(), entry.front.id.clone());
        out.written.push(entry.front.id.clone());
        placed.push(Placed {
            bundle_id,
            here: entry.front.id.clone(),
            bundle_digest,
            digest: digest(&entry.render()),
            classification: entry.front.classification,
        });
    }
    if same_owner {
        out.promotions = landed_promotions(carried, &placed, into_domain);
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
            review_due: false,
        };
        let dest = tempfile::tempdir().unwrap();
        let ex = export(
            &store,
            vec![stale],
            "dom-a",
            Label::Restricted,
            "memory:test:me",
            dest.path(),
            Vec::new(),
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

    /// A record about an entry that the export lists but does not write, since the
    /// record no longer holds its text, stays home even within the bound: a bundle
    /// carries records of its entries alone.
    #[tokio::test]
    async fn a_record_of_an_entry_left_home_is_not_carried_even_within_the_bound() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_dir(dir.path()).unwrap();
        let entry = Entry::parse(HELD).unwrap();
        store.put(&entry).await.unwrap();
        let hit = |id: &str| Hit {
            id: id.into(),
            domain: "dom-a".into(),
            entry_type: entry.front.entry_type,
            trust_tier: entry.front.trust_tier,
            classification: Label::Public,
            title: "Held".into(),
            summary: "Held.".into(),
            score: 0.0,
            review_due: false,
        };
        let record = |id: &str| PromotionRecord {
            spec: dm_ledger::PromotionSpec {
                domain: "dom-a".into(),
                entry: id.into(),
                tier: TrustTier::Curated,
                by: "agent:librarian".into(),
                author: "human:test:me".into(),
                why: format!("why {id} was promoted"),
                classification: Label::Public,
                digest: "d0".into(),
                new_digest: "d1".into(),
            },
            why_erased: false,
            at: time::OffsetDateTime::UNIX_EPOCH,
        };
        let dest = tempfile::tempdir().unwrap();
        let ex = export(
            &store,
            vec![hit(&entry.front.id), hit("mem-a-gone0001")],
            "dom-a",
            Label::Restricted,
            "memory:test:me",
            dest.path(),
            vec![record(&entry.front.id), record("mem-a-gone0001")],
        )
        .await
        .unwrap();
        let carried = ex
            .manifest
            .promotions
            .as_ref()
            .expect("the held entry's record is carried");
        assert_eq!(carried.records.len(), 1, "{carried:?}");
        assert_eq!(carried.records[0].entry, entry.front.id);
        let text = std::fs::read_to_string(dest.path().join(PROMOTIONS_FILE)).unwrap();
        assert!(!text.contains("mem-a-gone0001"), "{text}");
        let records = carried_promotions(dest.path(), &ex.manifest).unwrap();
        assert_eq!(records.len(), 1);
    }
}
