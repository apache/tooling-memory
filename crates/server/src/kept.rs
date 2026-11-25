//! An export that the memory server keeps under the root. A session that a harness
//! reaches gives no path: the place is made from the root, the session id, the domain and
//! the label, and from nothing else. The answer holds counts, labels and a digest, and
//! no id or text of an entry. The count and the digest are facts about the entries of
//! the bundle, so the label of the session rises to the highest label among them.

use crate::bundle::{self, Kept, EXPORTS_DIR};
use crate::{io, Error, Memory};
use dm_core::Label;
use std::path::Path;

/// Refuse `path` when it is there and is not a directory of its own: a link there would
/// carry the bundle out of the root.
fn not_a_link(path: &Path) -> Result<(), Error> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => Err(Error::Denied(format!(
            "{} is not a directory of the exports",
            path.display()
        ))),
        _ => Ok(()),
    }
}

impl Memory {
    /// Export a domain at or below a label into a bundle that stays under the root, at
    /// `state/exports/<session id>/<domain>-<label>`. The bundle is the one that
    /// [`Memory::export`] writes, under the same checks: the read filter of the session,
    /// the bound, the label of the session, which rises to the highest label in the
    /// bundle, and the two events in the log of the domain. A bundle that an earlier
    /// call of this session left at that place is removed first. A call that fails
    /// leaves no bundle there.
    ///
    /// # Errors
    ///
    /// As [`Memory::export`]; a name that cannot be a directory; a link where a directory
    /// of the exports must be.
    pub async fn export_kept(&self, domain: Option<&str>, max_label: Label) -> Result<Kept, Error> {
        let domain = self.resolve_domain(domain)?;
        let kept_at = bundle::kept_at(&self.session_id, &domain, max_label)?;
        let exports = self.root.join(EXPORTS_DIR);
        not_a_link(&exports)?;
        not_a_link(&exports.join(&self.session_id))?;
        let dir = self.root.join(&kept_at);
        if std::fs::symlink_metadata(&dir).is_ok() {
            std::fs::remove_dir_all(&dir).map_err(io(&dir))?;
        }
        let exported = match self.export(Some(&domain), max_label, &dir).await {
            Ok(exported) => exported,
            Err(e) => {
                // a part of a bundle is no bundle
                let _ = std::fs::remove_dir_all(&dir);
                return Err(e);
            }
        };
        let manifest = exported.manifest;
        Ok(Kept {
            domain,
            max_label,
            highest_label: manifest.highest_label,
            entries: manifest.entries.len(),
            manifest_digest: dm_core::digest(&manifest.to_yaml()),
            kept_at,
        })
    }

    /// Remove the bundles of a domain that the sessions of this root keep, at every
    /// label: a kept bundle holds bodies as they were. An erasure calls this for the
    /// domain of the erased entry and for each domain that holds an entry it withholds.
    pub(crate) fn drop_kept(&self, domain: &str) -> Result<(), Error> {
        let exports = self.root.join(EXPORTS_DIR);
        let Ok(sessions) = std::fs::read_dir(&exports) else {
            return Ok(());
        };
        for session in sessions.flatten() {
            for label in [Label::Public, Label::Internal, Label::Restricted] {
                let dir = session.path().join(format!("{domain}-{label}"));
                if std::fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir()) {
                    std::fs::remove_dir_all(&dir).map_err(io(&dir))?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::{self, Change};
    use crate::testing::{as_author, held_out_root};
    use crate::WriteRequest;
    use dm_core::manifest::Manifest;
    use dm_core::{EntryType, TrustTier};
    use dm_ledger::TaskSpec;

    const TITLE: &str = "Harbour crane";
    const WORD: &str = "gantry";

    async fn wrote(m: &Memory, kind: EntryType, label: Label, domain: Option<&str>) -> String {
        m.write(WriteRequest {
            title: Some(TITLE.into()),
            body: format!("The {WORD} lifts at {label}.\n"),
            entry_type: kind,
            classification: Some(label),
            domain: domain.map(str::to_string),
            ..WriteRequest::default()
        })
        .await
        .unwrap()
        .id
    }

    /// The entries of [`shelf`], by what each one is.
    struct Shelf {
        owner: Memory,
        /// A public fact of the owner in `dom-me`.
        open: String,
        /// An internal fact of the owner in `dom-me`.
        inner: String,
        /// A restricted fact of the owner in `dom-me`.
        secret: String,
        /// A public convention in the held-out domain, which no agent reads.
        held: String,
        /// A public handoff of `agent:a`, raw, with tap reach: it names a task of a tap.
        note: String,
    }

    /// A root with entries inside and outside what an agent with no task reads.
    async fn shelf(dir: &Path) -> Shelf {
        let owner = held_out_root(dir);
        let me = Some("dom-me");
        let open = wrote(&owner, EntryType::Fact, Label::Public, me).await;
        let inner = wrote(&owner, EntryType::Fact, Label::Internal, me).await;
        let secret = wrote(&owner, EntryType::Fact, Label::Restricted, me).await;
        let held = wrote(
            &owner,
            EntryType::Convention,
            Label::Public,
            Some("dom-held"),
        )
        .await;
        let spec = TaskSpec {
            title: "From the tracker".into(),
            body: "Do it.\n".into(),
            domain: "dom-me".into(),
            ..TaskSpec::default()
        };
        let filed = as_author(dir, "tap:issues").create_task(spec).await;
        let task = owner.admit(&filed.unwrap().task.id).await.unwrap();
        let a = as_author(dir, "agent:a");
        a.hold_task(&task.id).await.unwrap();
        let note = wrote(&a, EntryType::Handoff, Label::Public, me).await;
        let front = owner.get(&note).await.unwrap().front;
        assert_eq!(
            (front.trust_tier, front.reach),
            (TrustTier::Raw, dm_core::Reach::Tap)
        );
        let owner = Memory::open(dir).unwrap();
        Shelf {
            owner,
            open,
            inner,
            secret,
            held,
            note,
        }
    }

    fn manifest_at(root: &Path, kept: &Kept) -> (String, Manifest) {
        let text = std::fs::read_to_string(root.join(&kept.kept_at).join("manifest.yaml")).unwrap();
        let manifest = Manifest::from_yaml(&text).unwrap();
        (text, manifest)
    }

    fn ids(manifest: &Manifest) -> Vec<&str> {
        let mut ids: Vec<&str> = manifest.entries.iter().map(|e| e.id.as_str()).collect();
        ids.sort_unstable();
        ids
    }

    /// The `read` event of an export that carries `ids`: a survey, and no use of them.
    fn survey_of(ids: Vec<String>) -> Change {
        Change::Read { ids, survey: true }
    }

    fn sorted(mut ids: Vec<&str>) -> Vec<&str> {
        ids.sort_unstable();
        ids
    }

    #[test]
    fn a_name_is_checked_before_it_is_a_directory_of_an_export() {
        assert_eq!(
            bundle::kept_at("sess-1a", "dom-me", Label::Internal).unwrap(),
            "state/exports/sess-1a/dom-me-internal"
        );
        for bad in [
            "", "..", ".", ".hidden", "a/b", "a\\b", "../x", "a b", "dom:me", "é",
        ] {
            assert!(!bundle::is_kept_name(bad), "{bad:?}");
            let as_session = bundle::kept_at(bad, "dom-me", Label::Public);
            assert!(as_session.is_err(), "session {bad:?}: {as_session:?}");
            if !bad.is_empty() {
                let as_domain = bundle::kept_at("sess-1a", bad, Label::Public);
                assert!(as_domain.is_err(), "domain {bad:?}: {as_domain:?}");
            }
        }
        for good in ["sess-1a", "dom_me", "v1.2", "A-9"] {
            assert!(bundle::is_kept_name(good), "{good}");
        }
    }

    #[tokio::test]
    async fn an_export_above_the_handling_of_the_session_is_refused_and_makes_no_directory() {
        let dir = tempfile::tempdir().unwrap();
        let s = shelf(dir.path()).await;
        drop(s);
        let capped = as_author(dir.path(), "agent:b").with_handling_cap(Label::Internal);
        let refused = capped.export_kept(Some("dom-me"), Label::Restricted).await;
        assert!(
            matches!(&refused, Err(Error::Denied(why)) if why.contains("exceeds handling")),
            "{refused:?}"
        );
        // a domain that the session does not read, and no domain where several are granted
        assert!(capped
            .export_kept(Some("dom-held"), Label::Public)
            .await
            .is_err());
        assert!(capped
            .export_kept(Some("dom-none"), Label::Public)
            .await
            .is_err());
        assert!(capped.export_kept(None, Label::Public).await.is_err());
        assert!(
            !dir.path().join(EXPORTS_DIR).exists(),
            "a refused export makes no directory"
        );
    }

    #[tokio::test]
    async fn the_bundle_of_an_agent_holds_only_what_its_read_filter_admits_under_the_bound() {
        let dir = tempfile::tempdir().unwrap();
        let s = shelf(dir.path()).await;
        let b = as_author(dir.path(), "agent:b");
        let at = log::read(dir.path(), "dom-me", 0).unwrap().len() as u64;
        let kept = b
            .export_kept(Some("dom-me"), Label::Internal)
            .await
            .unwrap();
        let (text, manifest) = manifest_at(dir.path(), &kept);
        assert_eq!(
            ids(&manifest),
            sorted(vec![&s.open, &s.inner]),
            "not the restricted entry {}, not the entry {} of the held-out domain, and not \
             the raw note {} of a task chain that the session is not in",
            s.secret,
            s.held,
            s.note
        );
        assert_eq!(
            kept.kept_at,
            format!("state/exports/{}/dom-me-internal", b.session_id())
        );
        assert_eq!(
            (kept.domain.as_str(), kept.max_label),
            ("dom-me", Label::Internal)
        );
        assert_eq!(kept.entries, 2);
        assert_eq!(kept.highest_label, Label::Internal);
        assert_eq!(kept.highest_label, manifest.highest_label);
        assert_eq!(kept.manifest_digest, dm_core::digest(&text));

        // the answer names no entry
        let answer = serde_json::to_string(&kept).unwrap();
        for hidden in [&*s.open, &*s.inner, TITLE, WORD] {
            assert!(!answer.contains(hidden), "{hidden} in {answer}");
        }
        let keys: Vec<String> = serde_json::to_value(&kept)
            .unwrap()
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        assert_eq!(
            keys,
            [
                "domain",
                "entries",
                "highest_label",
                "kept_at",
                "manifest_digest",
                "max_label"
            ]
        );

        // the log of the domain: the read of what the bundle carries, then the send
        let events = log::read(dir.path(), "dom-me", at + 1).unwrap();
        let own: Vec<_> = events
            .iter()
            .filter(|e| e.session.as_deref() == Some(b.session_id()))
            .collect();
        assert_eq!(own.len(), 2, "{own:?}");
        let carried: Vec<String> = manifest.entries.iter().map(|e| e.id.clone()).collect();
        assert_eq!(own[0].change, survey_of(carried));
        assert_eq!(own[0].label, Some(Label::Internal));
        let sent = Change::Send {
            to: "export:internal".into(),
            digest: kept.manifest_digest.clone(),
        };
        assert_eq!(own[1].change, sent);

        // a lower bound: the public entry alone, and the highest label is the bundle's
        let public = b.export_kept(Some("dom-me"), Label::Public).await.unwrap();
        let (_, manifest) = manifest_at(dir.path(), &public);
        assert_eq!(ids(&manifest), [s.open.as_str()]);
        assert_eq!((public.entries, public.highest_label), (1, Label::Public));
        // the owner reads the note and the restricted entry; an agent does not
        let all = s.owner.export_kept(Some("dom-me"), Label::Restricted).await;
        let (_, manifest) = manifest_at(dir.path(), &all.unwrap());
        assert_eq!(
            ids(&manifest),
            sorted(vec![&s.open, &s.inner, &s.secret, &s.note])
        );
    }

    #[tokio::test]
    async fn an_export_raises_the_label_of_the_session_to_the_highest_label_in_the_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let s = shelf(dir.path()).await;
        let b = as_author(dir.path(), "agent:b");
        assert_eq!(b.label(), Label::Public);
        // the bound is restricted, and the highest entry that the session reads there
        // is internal
        let low = b.export_kept(Some("dom-me"), Label::Public).await.unwrap();
        assert_eq!((low.entries, b.label()), (1, Label::Public));
        let kept = b.export_kept(Some("dom-me"), Label::Internal).await;
        let kept = kept.unwrap();
        assert_eq!((kept.entries, kept.highest_label), (2, Label::Internal));
        assert_eq!(
            b.label(),
            Label::Internal,
            "as after a search with those hits"
        );
        // what the session writes next is at its label: the count is not public now
        let said = b
            .write(WriteRequest {
                body: format!("dom-me holds {} entries.\n", kept.entries),
                domain: Some("dom-me".into()),
                ..WriteRequest::default()
            })
            .await
            .unwrap();
        let front = s.owner.archived(&said.id).await.unwrap().front;
        assert_eq!(front.classification, Label::Internal);
        // an export to a directory that the caller names is under the same rule
        let c = as_author(dir.path(), "agent:c");
        let out = tempfile::tempdir().unwrap();
        c.export(Some("dom-me"), Label::Internal, out.path())
            .await
            .unwrap();
        assert_eq!(c.label(), Label::Internal);
        // a bundle with no entry raises nothing
        crate::testing::agents_read(dir.path(), "\"raw_reach\": \"chain\"");
        let d = as_author(dir.path(), "agent:d");
        let none = d.export_kept(Some("dom-me"), Label::Internal).await;
        assert_eq!((none.unwrap().entries, d.label()), (0, Label::Public));
    }

    #[tokio::test]
    async fn the_highest_label_is_of_the_entries_and_not_of_the_bound() {
        let dir = tempfile::tempdir().unwrap();
        let owner = Memory::init(dir.path(), "human:test:me", "dom-me").unwrap();
        wrote(&owner, EntryType::Fact, Label::Public, None).await;
        wrote(&owner, EntryType::Fact, Label::Internal, None).await;
        let kept = owner.export_kept(None, Label::Restricted).await.unwrap();
        assert_eq!(kept.max_label, Label::Restricted);
        assert_eq!(kept.highest_label, Label::Internal);
        let (_, manifest) = manifest_at(dir.path(), &kept);
        let highest = manifest.entries.iter().map(|e| e.classification).max();
        assert_eq!(Some(kept.highest_label), highest);
    }

    #[tokio::test]
    async fn a_second_call_replaces_the_bundle_of_the_session_and_no_other() {
        let dir = tempfile::tempdir().unwrap();
        let s = shelf(dir.path()).await;
        let b = as_author(dir.path(), "agent:b");
        let other = as_author(dir.path(), "agent:c");
        let first = b
            .export_kept(Some("dom-me"), Label::Internal)
            .await
            .unwrap();
        let theirs = other
            .export_kept(Some("dom-me"), Label::Internal)
            .await
            .unwrap();
        assert_ne!(
            first.kept_at, theirs.kept_at,
            "one directory for each session"
        );
        let mine = dir.path().join(&first.kept_at);
        let file = |id: &str| mine.join(format!("{id}.md"));
        assert!(file(&s.open).is_file() && file(&s.inner).is_file());

        // the agents of the root now read no raw entry outside a task chain
        crate::testing::agents_read(dir.path(), "\"raw_reach\": \"chain\"");
        let second = b
            .export_kept(Some("dom-me"), Label::Internal)
            .await
            .unwrap();
        assert_eq!(second.kept_at, first.kept_at);
        assert_eq!(second.entries, 0, "{second:?}");
        assert_ne!(second.manifest_digest, first.manifest_digest);
        assert!(
            !file(&s.open).exists() && !file(&s.inner).exists(),
            "the files of the first bundle are gone"
        );
        let (_, manifest) = manifest_at(dir.path(), &second);
        assert!(manifest.entries.is_empty());
        let (_, manifest) = manifest_at(dir.path(), &theirs);
        assert_eq!(ids(&manifest), sorted(vec![&s.open, &s.inner]));
        assert!(dir
            .path()
            .join(&theirs.kept_at)
            .join(format!("{}.md", s.open))
            .is_file());
    }

    #[tokio::test]
    async fn a_kept_bundle_imports_into_a_second_root() {
        let dir = tempfile::tempdir().unwrap();
        let s = shelf(dir.path()).await;
        let b = as_author(dir.path(), "agent:b");
        let kept = b
            .export_kept(Some("dom-me"), Label::Internal)
            .await
            .unwrap();
        let there = tempfile::tempdir().unwrap();
        let other = Memory::init(there.path(), "human:test:you", "dom-you").unwrap();
        let landed = other
            .import(&dir.path().join(&kept.kept_at), None)
            .await
            .unwrap();
        assert_eq!(landed.written.len(), kept.entries, "{landed:?}");
        let found = other.search(WORD, None, 10).await.unwrap();
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(other
            .search("restricted", None, 10)
            .await
            .unwrap()
            .is_empty());
        drop(s);
    }

    #[tokio::test]
    async fn an_erasure_removes_the_kept_bundles_of_the_domain() {
        let dir = tempfile::tempdir().unwrap();
        let s = shelf(dir.path()).await;
        let b = as_author(dir.path(), "agent:b");
        let kept = b
            .export_kept(Some("dom-me"), Label::Internal)
            .await
            .unwrap();
        let held = s.owner.export_kept(Some("dom-held"), Label::Internal).await;
        let held = held.unwrap();
        let body = dir
            .path()
            .join(&kept.kept_at)
            .join(format!("{}.md", s.open));
        assert!(std::fs::read_to_string(&body).unwrap().contains(WORD));
        s.owner
            .erase(&s.open, dm_core::erasure::Reason::Requested)
            .await
            .unwrap();
        assert!(!body.exists(), "the body left the kept bundle");
        assert!(!dir.path().join(&kept.kept_at).exists());
        assert!(
            dir.path()
                .join(&held.kept_at)
                .join("manifest.yaml")
                .is_file(),
            "the bundle of another domain stays"
        );
    }

    #[tokio::test]
    async fn an_erasure_removes_the_kept_bundles_of_a_domain_that_holds_a_withheld_entry() {
        let dir = tempfile::tempdir().unwrap();
        let s = shelf(dir.path()).await;
        // an entry of the held-out domain that names the public fact of `dom-me`
        let citing = s
            .owner
            .write(WriteRequest {
                body: format!("Drawn from {}.\n", s.open),
                entry_type: EntryType::Convention,
                classification: Some(Label::Public),
                domain: Some("dom-held".into()),
                cites: vec![s.open.clone()],
                ..WriteRequest::default()
            })
            .await
            .unwrap()
            .id;
        let held = s.owner.export_kept(Some("dom-held"), Label::Internal).await;
        let held = dir.path().join(held.unwrap().kept_at);
        let body = held.join(format!("{citing}.md"));
        assert!(body.is_file());
        let served = bundle::bundle_dir(dir.path(), "dom-held", Label::Public);
        std::fs::create_dir_all(&served).unwrap();
        let receipt = s.owner.erase(&s.open, dm_core::erasure::Reason::Requested);
        assert_eq!(receipt.await.unwrap().withheld, [citing.as_str()]);
        let now = s.owner.store.get("dom-held", &citing).await.unwrap();
        assert_eq!(now.unwrap().front.status, Some(dm_core::Status::Withheld));
        assert!(!body.exists(), "the withheld body left the kept bundle");
        assert!(!held.exists());
        assert!(!served.exists(), "and the served bundle of that domain");
    }

    #[tokio::test]
    async fn an_export_keeps_the_lowest_tier_of_the_read_filter_of_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let s = shelf(dir.path()).await;
        s.owner
            .promote(&s.inner, TrustTier::Curated, "Checked.")
            .await
            .unwrap();
        crate::testing::agents_read(dir.path(), "\"min_tier\": \"curated\"");
        let b = as_author(dir.path(), "agent:b");
        assert!(b.get(&s.open).await.is_err(), "a raw entry is not read");
        let kept = b.export_kept(Some("dom-me"), Label::Internal);
        let kept = kept.await.unwrap();
        let (_, manifest) = manifest_at(dir.path(), &kept);
        assert_eq!(
            ids(&manifest),
            [s.inner.as_str()],
            "the curated entry, and not the raw entry {}",
            s.open
        );
        assert_eq!(kept.entries, 1);
        // the owner is no agent, and its bundle holds the raw entries too
        let all = s.owner.export_kept(Some("dom-me"), Label::Internal).await;
        let (_, manifest) = manifest_at(dir.path(), &all.unwrap());
        assert_eq!(ids(&manifest), sorted(vec![&s.open, &s.inner, &s.note]));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_link_where_the_exports_are_kept_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let owner = Memory::init(dir.path(), "human:test:me", "dom-me").unwrap();
        wrote(&owner, EntryType::Fact, Label::Public, None).await;
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join(EXPORTS_DIR)).unwrap();
        let refused = owner.export_kept(None, Label::Public).await;
        assert!(matches!(refused, Err(Error::Denied(_))), "{refused:?}");
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
        std::fs::remove_file(dir.path().join(EXPORTS_DIR)).unwrap();
        std::fs::create_dir_all(dir.path().join(EXPORTS_DIR)).unwrap();
        let session = dir.path().join(EXPORTS_DIR).join(owner.session_id());
        std::os::unix::fs::symlink(outside.path(), &session).unwrap();
        let refused = owner.export_kept(None, Label::Public).await;
        assert!(matches!(refused, Err(Error::Denied(_))), "{refused:?}");
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    }
}
