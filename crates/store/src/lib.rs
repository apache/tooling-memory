//! The record: entries as versioned objects behind Apache OpenDAL.
//!
//! Four things hold here, because the rest of the system depends on them:
//!
//! - every write is a new version; the previous text is kept, byte for byte;
//! - nothing is edited in place: the current version is replaced, never patched;
//! - demotion leaves a tombstone, which is the same entry with a status;
//! - the whole record can be read back, so an index can be rebuilt from it alone.
//!
//! The store is rooted. Every path it touches is relative to the operator's root, names
//! are validated before they become paths, and nothing can name a file outside the root.
//! On a laptop the root is a directory; in an institution it is a bucket. The code does
//! not know which.
//!
//! Layout, per domain:
//!
//! ```text
//! record/<domain>/<id>.md                             the current version
//! record/<domain>/.history/<id>/<stamp>-<digest>.md   every previous version
//! record/<domain>/.erasures/<id>.yaml                 the receipt of an erasure
//! record/<domain>/.transcripts/<session>.json         a transcript: a source, not an entry
//! record/<domain>/.excerpts/<id>/<name>.json          what an entry kept of a transcript
//! ```
//!
//! Transcripts and excerpts are objects beside the entries, never entries: nothing that
//! lists, reads back or rebuilds entries sees them, because they live in directories a
//! listing of entries skips. What they hold is the server's business; the store only
//! keeps the bytes under a validated name.
//!
//! Erasure is the one exception to the first two rules: the body leaves every version,
//! current and archived, and the stub is written over each of them in place, with the
//! receipt beside the record saying what was removed.

use dm_core::erasure::{body_digest, Receipt, Removed};
use dm_core::{digest, Entry, Status};
use opendal::Operator;
use std::path::Path;
use time::{format_description::FormatItem, macros::format_description, OffsetDateTime};

const RECORD: &str = "record";
const HISTORY: &str = ".history";
const ERASURES: &str = ".erasures";
const TRANSCRIPTS: &str = ".transcripts";
const EXCERPTS: &str = ".excerpts";
const STAMP: &[FormatItem<'_>] =
    format_description!("[year][month][day]T[hour][minute][second].[subsecond digits:6]Z");

/// What can go wrong at the record.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The backend refused or failed.
    #[error("record backend: {0}")]
    Backend(#[from] opendal::Error),
    /// Text in the record is not a valid entry.
    #[error("entry {path}: {source}")]
    Entry {
        /// The path that failed to parse.
        path: String,
        /// Why.
        source: dm_core::Error,
    },
    /// A domain or id that cannot be a path segment.
    #[error("bad name {0:?}: letters, digits, '.', '_' and '-' only, not starting with '.'")]
    BadName(String),
    /// Bytes in the record that are not UTF-8.
    #[error("entry {0} is not UTF-8")]
    NotText(String),
    /// Asked for an entry that is not there.
    #[error("no entry {id} in {domain}")]
    NotFound {
        /// The domain asked for.
        domain: String,
        /// The id asked for.
        id: String,
    },
}

/// The outcome of a write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Written {
    /// The entry's id.
    pub id: String,
    /// Digest of the text now current.
    pub digest: String,
    /// Where the previous version went, if there was one and it differed.
    pub archived: Option<String>,
    /// False when the text was already current and nothing was written.
    pub changed: bool,
}

/// One previous version of an entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    /// `<stamp>-<digest>`, which sorts by time.
    pub name: String,
    /// The text as it was.
    pub text: String,
}

/// Where beside a domain's entries an object lives that is not an entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Area {
    /// Session transcripts: sources, reached only by citation.
    Transcripts,
    /// Excerpts of transcripts, one directory per entry that keeps them.
    Excerpts,
}

impl Area {
    fn dir(self) -> &'static str {
        match self {
            Area::Transcripts => TRANSCRIPTS,
            Area::Excerpts => EXCERPTS,
        }
    }
}

fn object_path(domain: &str, area: Area, parts: &[&str]) -> Result<String, Error> {
    let mut path = format!("{RECORD}/{}/{}", valid_name(domain)?, area.dir());
    for p in parts {
        path.push('/');
        path.push_str(valid_name(p)?);
    }
    Ok(path)
}

/// The record, behind one operator.
#[derive(Clone)]
pub struct Store {
    op: Operator,
}

fn valid_name(s: &str) -> Result<&str, Error> {
    let ok = !s.is_empty()
        && !s.starts_with('.')
        && s.len() <= 128
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok {
        Ok(s)
    } else {
        Err(Error::BadName(s.to_string()))
    }
}

fn current_path(domain: &str, id: &str) -> Result<String, Error> {
    Ok(format!(
        "{RECORD}/{}/{}.md",
        valid_name(domain)?,
        valid_name(id)?
    ))
}

fn history_dir(domain: &str, id: &str) -> Result<String, Error> {
    Ok(format!(
        "{RECORD}/{}/{HISTORY}/{}/",
        valid_name(domain)?,
        valid_name(id)?
    ))
}

fn receipt_path(domain: &str, id: &str) -> Result<String, Error> {
    Ok(format!(
        "{RECORD}/{}/{ERASURES}/{}.yaml",
        valid_name(domain)?,
        valid_name(id)?
    ))
}

fn stamp() -> String {
    OffsetDateTime::now_utc()
        .format(STAMP)
        .unwrap_or_else(|_| "00000000T000000.000000Z".to_string())
}

impl Store {
    /// A store over any operator. The operator's root is the record's boundary.
    #[must_use]
    pub fn from_operator(op: Operator) -> Store {
        Store { op }
    }

    /// A store in a directory on disk, created if missing.
    ///
    /// # Errors
    ///
    /// When the backend cannot be built, for instance when the path is not a directory.
    pub fn open_dir(root: &Path) -> Result<Store, Error> {
        let root = root.to_string_lossy();
        let op = Operator::new(opendal::services::Fs::default().root(&root))?;
        Ok(Store::from_operator(op))
    }

    /// A store that lives only in memory, for tests and dry runs.
    ///
    /// # Errors
    ///
    /// Never in practice; the memory backend has nothing to fail on.
    pub fn in_memory() -> Result<Store, Error> {
        let op = Operator::new(opendal::services::Memory::default())?;
        Ok(Store::from_operator(op))
    }

    /// Write an entry as the current version of its id in its domain.
    ///
    /// If a different version is current, it is archived first. If the same text is
    /// already current, nothing is written. If the entry supersedes another that is still
    /// active, that one is marked superseded, which is a write of its own.
    ///
    /// # Errors
    ///
    /// [`Error::BadName`] for a domain or id that cannot be a path, or a backend error.
    pub async fn put(&self, entry: &Entry) -> Result<Written, Error> {
        let domain = entry.front.domain.as_str();
        let id = entry.front.id.as_str();
        let path = current_path(domain, id)?;
        let text = entry.render();
        let new_digest = digest(&text);

        let previous = self.read_text(&path).await?;
        if previous.as_deref() == Some(text.as_str()) {
            return Ok(Written {
                id: id.to_string(),
                digest: new_digest,
                archived: None,
                changed: false,
            });
        }

        let mut archived = None;
        if let Some(old) = previous {
            let name = format!("{}-{}", stamp(), &digest(&old)[..8]);
            let hist = format!("{}{name}.md", history_dir(domain, id)?);
            self.op.write(&hist, old.into_bytes()).await?;
            archived = Some(name);
        }
        self.op.write(&path, text.into_bytes()).await?;

        if let Some(older) = &entry.front.supersedes {
            if let Some(mut old) = self.get(domain, older).await? {
                if old.is_active() {
                    old.front.status = Some(Status::Superseded);
                    Box::pin(self.put(&old)).await?;
                }
            }
        }

        Ok(Written {
            id: id.to_string(),
            digest: new_digest,
            archived,
            changed: true,
        })
    }

    /// The current version of an entry, parsed. `None` when there is none.
    ///
    /// # Errors
    ///
    /// A bad name, a backend error, or text in the record that is not an entry.
    pub async fn get(&self, domain: &str, id: &str) -> Result<Option<Entry>, Error> {
        let path = current_path(domain, id)?;
        match self.read_text(&path).await? {
            None => Ok(None),
            Some(text) => Entry::parse(&text)
                .map(Some)
                .map_err(|source| Error::Entry { path, source }),
        }
    }

    /// The current version of an entry as text. `None` when there is none.
    ///
    /// # Errors
    ///
    /// A bad name or a backend error.
    pub async fn get_text(&self, domain: &str, id: &str) -> Result<Option<String>, Error> {
        self.read_text(&current_path(domain, id)?).await
    }

    /// Demote an entry: a new version with `status: demoted`, the body kept.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] when there is no such entry, or what [`Store::put`] returns.
    pub async fn demote(&self, domain: &str, id: &str) -> Result<Written, Error> {
        let mut entry = self.get(domain, id).await?.ok_or_else(|| Error::NotFound {
            domain: domain.to_string(),
            id: id.to_string(),
        })?;
        entry.front.status = Some(Status::Demoted);
        self.put(&entry).await
    }

    /// Every version of an entry with the digest of its text, the current one first, as
    /// an erasure will list them: read before anything is removed, so the receipt can be
    /// written first.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] when there is no such entry, a bad name, or a backend error.
    pub async fn digests(&self, domain: &str, id: &str) -> Result<Vec<Removed>, Error> {
        let current = self
            .read_text(&current_path(domain, id)?)
            .await?
            .ok_or_else(|| Error::NotFound {
                domain: domain.to_string(),
                id: id.to_string(),
            })?;
        let of_body = |text: &str| Entry::parse(text).ok().map(|e| body_digest(&e.body));
        let mut out = vec![Removed {
            version: "current".into(),
            digest: digest(&current),
            body_digest: of_body(&current),
        }];
        for version in self.history(domain, id).await? {
            out.push(Removed {
                version: version.name,
                digest: digest(&version.text),
                body_digest: of_body(&version.text),
            });
        }
        Ok(out)
    }

    /// Erase the entry at `domain` and `id`: `stub` is written over its current version
    /// and over every archived one, so no version keeps the body, and nothing is
    /// archived on the way. Returns what was removed, as [`Store::digests`] lists it.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] when there is no such entry, a bad name, or a backend error.
    pub async fn erase(&self, domain: &str, id: &str, stub: &Entry) -> Result<Vec<Removed>, Error> {
        let removed = self.digests(domain, id).await?;
        let text = stub.render();
        let dir = history_dir(domain, id)?;
        for version in removed.iter().skip(1) {
            self.op
                .write(
                    &format!("{dir}{}.md", version.version),
                    text.clone().into_bytes(),
                )
                .await?;
        }
        self.op
            .write(&current_path(domain, id)?, text.into_bytes())
            .await?;
        Ok(removed)
    }

    /// Keep the receipt of an erasure beside the record.
    ///
    /// # Errors
    ///
    /// A bad name or a backend error.
    pub async fn put_receipt(&self, receipt: &Receipt) -> Result<(), Error> {
        let path = receipt_path(&receipt.domain, &receipt.id)?;
        self.op.write(&path, receipt.to_yaml().into_bytes()).await?;
        Ok(())
    }

    /// The receipt of an erasure, if the entry was erased.
    ///
    /// # Errors
    ///
    /// A bad name, a backend error, or a receipt that does not parse.
    pub async fn receipt(&self, domain: &str, id: &str) -> Result<Option<Receipt>, Error> {
        let path = receipt_path(domain, id)?;
        match self.read_text(&path).await? {
            None => Ok(None),
            Some(text) => Receipt::from_yaml(&text)
                .map(Some)
                .map_err(|source| Error::Entry { path, source }),
        }
    }

    /// Every receipt in a domain, by id.
    ///
    /// # Errors
    ///
    /// A bad name, a backend error, or a receipt that does not parse.
    pub async fn receipts(&self, domain: &str) -> Result<Vec<Receipt>, Error> {
        let dir = format!("{RECORD}/{}/{ERASURES}/", valid_name(domain)?);
        if !self.op.exists(&dir).await? {
            return Ok(Vec::new());
        }
        let mut ids: Vec<String> = self
            .op
            .list(&dir)
            .await?
            .into_iter()
            .filter(|e| e.metadata().is_file())
            .filter_map(|e| e.name().strip_suffix(".yaml").map(str::to_string))
            .collect();
        ids.sort();
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(r) = self.receipt(domain, &id).await? {
                out.push(r);
            }
        }
        Ok(out)
    }

    /// Ids with a current version in a domain, tombstones included, sorted.
    ///
    /// # Errors
    ///
    /// A bad name or a backend error.
    pub async fn list(&self, domain: &str) -> Result<Vec<String>, Error> {
        let dir = format!("{RECORD}/{}/", valid_name(domain)?);
        let mut ids: Vec<String> = self
            .op
            .list(&dir)
            .await?
            .into_iter()
            .filter(|e| e.metadata().is_file())
            .filter_map(|e| e.name().strip_suffix(".md").map(str::to_string))
            .collect();
        ids.sort();
        Ok(ids)
    }

    /// Whether `id` names a current entry in any domain of the record. An id names one
    /// entry across the record, since a reader asks for one by id alone, so a copy that
    /// would land under an id in use is given another; the record answers, not an index
    /// that can lag it.
    ///
    /// # Errors
    ///
    /// [`Error::BadName`] for an id that cannot be a path, or a backend error.
    pub async fn holds(&self, id: &str) -> Result<bool, Error> {
        for domain in self.domains().await? {
            if self.read_text(&current_path(&domain, id)?).await?.is_some() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Every domain with a directory in the record, sorted.
    ///
    /// # Errors
    ///
    /// A backend error.
    pub async fn domains(&self) -> Result<Vec<String>, Error> {
        let mut names: Vec<String> = self
            .op
            .list(&format!("{RECORD}/"))
            .await?
            .into_iter()
            // the listing names the directory itself as well as what is in it
            .filter(|e| e.metadata().is_dir() && e.path() != format!("{RECORD}/"))
            .map(|e| e.name().trim_end_matches('/').to_string())
            .filter(|n| !n.is_empty() && !n.starts_with('.'))
            .collect();
        names.sort();
        Ok(names)
    }

    /// Every previous version of an entry, oldest first.
    ///
    /// # Errors
    ///
    /// A bad name, a backend error, or bytes that are not text.
    pub async fn history(&self, domain: &str, id: &str) -> Result<Vec<Version>, Error> {
        let dir = history_dir(domain, id)?;
        if !self.op.exists(&dir).await? {
            return Ok(Vec::new());
        }
        let mut names: Vec<String> = self
            .op
            .list(&dir)
            .await?
            .into_iter()
            .filter(|e| e.metadata().is_file())
            .filter_map(|e| e.name().strip_suffix(".md").map(str::to_string))
            .collect();
        names.sort();
        let mut out = Vec::with_capacity(names.len());
        for name in names {
            let path = format!("{dir}{name}.md");
            let text = self
                .read_text(&path)
                .await?
                .ok_or_else(|| Error::NotText(path.clone()))?;
            out.push(Version { name, text });
        }
        Ok(out)
    }

    /// Every current entry in every domain. What a rebuild reads.
    ///
    /// # Errors
    ///
    /// A backend error, or text in the record that is not an entry.
    pub async fn all(&self) -> Result<Vec<Entry>, Error> {
        let mut out = Vec::new();
        for domain in self.domains().await? {
            for id in self.list(&domain).await? {
                if let Some(e) = self.get(&domain, &id).await? {
                    out.push(e);
                }
            }
        }
        Ok(out)
    }

    /// Keep an object that is not an entry under a domain: `parts` name it inside the
    /// area, each a valid name. Replaces what was there.
    ///
    /// # Errors
    ///
    /// A bad name or a backend error.
    pub async fn put_object(
        &self,
        domain: &str,
        area: Area,
        parts: &[&str],
        bytes: Vec<u8>,
    ) -> Result<(), Error> {
        self.op
            .write(&object_path(domain, area, parts)?, bytes)
            .await?;
        Ok(())
    }

    /// An object's text, if it is there.
    ///
    /// # Errors
    ///
    /// A bad name, a backend error, or bytes that are not text.
    pub async fn get_object(
        &self,
        domain: &str,
        area: Area,
        parts: &[&str],
    ) -> Result<Option<String>, Error> {
        self.read_text(&object_path(domain, area, parts)?).await
    }

    /// Remove an object; one that is not there is not an error.
    ///
    /// # Errors
    ///
    /// A bad name or a backend error.
    pub async fn remove_object(
        &self,
        domain: &str,
        area: Area,
        parts: &[&str],
    ) -> Result<(), Error> {
        self.op.delete(&object_path(domain, area, parts)?).await?;
        Ok(())
    }

    /// The names under a directory of an area, sorted: its files with `dirs` false, its
    /// subdirectories with `dirs` true. Nothing when the directory is not there.
    ///
    /// # Errors
    ///
    /// A bad name or a backend error.
    pub async fn objects(
        &self,
        domain: &str,
        area: Area,
        dir: &[&str],
        dirs: bool,
    ) -> Result<Vec<String>, Error> {
        let path = format!("{}/", object_path(domain, area, dir)?);
        if !self.op.exists(&path).await? {
            return Ok(Vec::new());
        }
        let mut names: Vec<String> = self
            .op
            .list(&path)
            .await?
            .into_iter()
            .filter(|e| e.path() != path && e.metadata().is_dir() == dirs)
            .map(|e| e.name().trim_end_matches('/').to_string())
            .filter(|n| !n.is_empty())
            .collect();
        names.sort();
        Ok(names)
    }

    async fn read_text(&self, path: &str) -> Result<Option<String>, Error> {
        match self.op.read(path).await {
            Ok(buf) => String::from_utf8(buf.to_vec())
                .map(Some)
                .map_err(|_| Error::NotText(path.to_string())),
            Err(e) if e.kind() == opendal::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dm_core::{EntryType, Frontmatter, Label, TrustTier};
    use time::macros::datetime;

    fn entry(domain: &str, id: &str, body: &str) -> Entry {
        Entry {
            front: Frontmatter {
                id: id.into(),
                entry_type: EntryType::Fact,
                domain: domain.into(),
                author: "human:test:me".into(),
                created: datetime!(2026-10-01 10:00 UTC),
                source: vec![],
                confidence: None,
                trust_tier: TrustTier::Raw,
                review_by: None,
                supersedes: None,
                policy_context: vec![],
                classification: Label::Public,
                status: None,
                origin: None,
                summary: None,
                title: None,
            },
            body: body.into(),
        }
    }

    #[tokio::test]
    async fn put_then_get_round_trips() {
        let s = Store::in_memory().unwrap();
        let e = entry("dom-a", "mem-a-1", "First.\n");
        let w = s.put(&e).await.unwrap();
        assert!(w.changed);
        assert!(w.archived.is_none());
        assert_eq!(w.digest, digest(&e.render()));
        assert_eq!(s.get("dom-a", "mem-a-1").await.unwrap(), Some(e));
        assert_eq!(s.get("dom-a", "mem-a-9").await.unwrap(), None);
    }

    #[tokio::test]
    async fn every_write_is_a_new_version_and_the_old_text_survives() {
        let s = Store::in_memory().unwrap();
        let v1 = entry("dom-a", "mem-a-1", "First.\n");
        let v2 = entry("dom-a", "mem-a-1", "Second.\n");
        s.put(&v1).await.unwrap();
        let w = s.put(&v2).await.unwrap();
        assert!(w.changed);
        let archived = w.archived.expect("previous version archived");
        let hist = s.history("dom-a", "mem-a-1").await.unwrap();
        assert_eq!(hist.len(), 1);
        assert_eq!(hist[0].name, archived);
        assert_eq!(hist[0].text, v1.render(), "archived byte for byte");
        assert_eq!(s.get("dom-a", "mem-a-1").await.unwrap(), Some(v2));
    }

    #[tokio::test]
    async fn identical_write_changes_nothing() {
        let s = Store::in_memory().unwrap();
        let e = entry("dom-a", "mem-a-1", "Same.\n");
        s.put(&e).await.unwrap();
        let w = s.put(&e).await.unwrap();
        assert!(!w.changed);
        assert!(w.archived.is_none());
        assert!(s.history("dom-a", "mem-a-1").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn history_is_oldest_first() {
        let s = Store::in_memory().unwrap();
        for i in 1..=3 {
            s.put(&entry("dom-a", "mem-a-1", &format!("v{i}\n")))
                .await
                .unwrap();
        }
        let hist = s.history("dom-a", "mem-a-1").await.unwrap();
        let bodies: Vec<_> = hist
            .iter()
            .map(|v| Entry::parse(&v.text).unwrap().body)
            .collect();
        assert_eq!(bodies, vec!["v1\n", "v2\n"]);
        assert!(hist[0].name < hist[1].name);
    }

    #[tokio::test]
    async fn demotion_leaves_a_tombstone_with_the_body() {
        let s = Store::in_memory().unwrap();
        s.put(&entry("dom-a", "mem-a-1", "Kept.\n")).await.unwrap();
        s.demote("dom-a", "mem-a-1").await.unwrap();
        let t = s.get("dom-a", "mem-a-1").await.unwrap().unwrap();
        assert_eq!(t.front.status, Some(Status::Demoted));
        assert!(!t.is_active());
        assert_eq!(t.body, "Kept.\n");
        assert_eq!(s.list("dom-a").await.unwrap(), vec!["mem-a-1"]);
        assert!(matches!(
            s.demote("dom-a", "mem-a-nope").await,
            Err(Error::NotFound { .. })
        ));
    }

    #[tokio::test]
    async fn superseding_marks_the_old_entry() {
        let s = Store::in_memory().unwrap();
        s.put(&entry("dom-a", "mem-a-1", "Old.\n")).await.unwrap();
        let mut newer = entry("dom-a", "mem-a-2", "New.\n");
        newer.front.supersedes = Some("mem-a-1".into());
        s.put(&newer).await.unwrap();
        let old = s.get("dom-a", "mem-a-1").await.unwrap().unwrap();
        assert_eq!(old.front.status, Some(Status::Superseded));
        assert_eq!(s.history("dom-a", "mem-a-1").await.unwrap().len(), 1);
        // superseding again is idempotent: the old one is no longer active
        s.put(&newer).await.unwrap();
        assert_eq!(s.history("dom-a", "mem-a-1").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn list_domains_and_all_for_rebuild() {
        let s = Store::in_memory().unwrap();
        s.put(&entry("dom-b", "mem-b-1", "b1\n")).await.unwrap();
        s.put(&entry("dom-a", "mem-a-2", "a2\n")).await.unwrap();
        s.put(&entry("dom-a", "mem-a-1", "a1\n")).await.unwrap();
        s.put(&entry("dom-a", "mem-a-1", "a1 again\n"))
            .await
            .unwrap();
        assert_eq!(s.domains().await.unwrap(), vec!["dom-a", "dom-b"]);
        assert_eq!(s.list("dom-a").await.unwrap(), vec!["mem-a-1", "mem-a-2"]);
        let all = s.all().await.unwrap();
        let ids: Vec<_> = all.iter().map(|e| e.front.id.as_str()).collect();
        assert_eq!(ids, vec!["mem-a-1", "mem-a-2", "mem-b-1"]);
        assert_eq!(
            all[0].body, "a1 again\n",
            "all() reads current versions only"
        );
        assert!(s.list("dom-none").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn names_that_could_leave_the_root_are_refused() {
        let s = Store::in_memory().unwrap();
        for bad in ["../x", "a/b", ".hidden", "", "a b", "x\\y", "..", "."] {
            assert!(
                matches!(s.get(bad, "mem-a-1").await, Err(Error::BadName(_))),
                "domain {bad:?}"
            );
            assert!(
                matches!(s.get("dom-a", bad).await, Err(Error::BadName(_))),
                "id {bad:?}"
            );
        }
        let mut e = entry("dom-a", "../escape", "x\n");
        assert!(matches!(s.put(&e).await, Err(Error::BadName(_))));
        e.front.id = "ok".into();
        e.front.domain = "../escape".into();
        assert!(matches!(s.put(&e).await, Err(Error::BadName(_))));
    }

    #[tokio::test]
    async fn on_disk_layout_is_what_the_docs_say() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open_dir(dir.path()).unwrap();
        s.put(&entry("dom-a", "mem-a-1", "v1\n")).await.unwrap();
        let w = s.put(&entry("dom-a", "mem-a-1", "v2\n")).await.unwrap();
        let current = dir.path().join("record/dom-a/mem-a-1.md");
        assert!(current.is_file());
        assert!(std::fs::read_to_string(&current).unwrap().ends_with("v2\n"));
        let hist = dir
            .path()
            .join("record/dom-a/.history/mem-a-1")
            .join(format!("{}.md", w.archived.unwrap()));
        assert!(hist.is_file(), "{}", hist.display());
        assert!(std::fs::read_to_string(&hist).unwrap().ends_with("v1\n"));
        // a second store over the same directory sees the same record
        let again = Store::open_dir(dir.path()).unwrap();
        assert_eq!(again.list("dom-a").await.unwrap(), vec!["mem-a-1"]);
        assert_eq!(again.history("dom-a", "mem-a-1").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn text_that_is_not_an_entry_is_an_error_not_a_panic() {
        let s = Store::in_memory().unwrap();
        s.op.write("record/dom-a/mem-a-1.md", b"not an entry".to_vec())
            .await
            .unwrap();
        assert!(matches!(
            s.get("dom-a", "mem-a-1").await,
            Err(Error::Entry { .. })
        ));
        s.op.write("record/dom-a/mem-a-2.md", vec![0xff, 0xfe])
            .await
            .unwrap();
        assert!(matches!(
            s.get("dom-a", "mem-a-2").await,
            Err(Error::NotText(_))
        ));
    }

    #[tokio::test]
    async fn objects_beside_the_entries_are_not_entries() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open_dir(dir.path()).unwrap();
        s.put(&entry("dom-a", "mem-a-1", "an entry\n"))
            .await
            .unwrap();
        s.put_object("dom-a", Area::Transcripts, &["sess-1.json"], b"{}".to_vec())
            .await
            .unwrap();
        s.put_object(
            "dom-a",
            Area::Excerpts,
            &["mem-a-1", "sess-1.3.json"],
            b"{}".to_vec(),
        )
        .await
        .unwrap();
        assert!(dir
            .path()
            .join("record/dom-a/.transcripts/sess-1.json")
            .is_file());
        assert_eq!(s.list("dom-a").await.unwrap(), vec!["mem-a-1"]);
        assert_eq!(s.all().await.unwrap().len(), 1);
        assert_eq!(s.domains().await.unwrap(), vec!["dom-a"]);
        assert_eq!(
            s.objects("dom-a", Area::Transcripts, &[], false)
                .await
                .unwrap(),
            vec!["sess-1.json"]
        );
        assert_eq!(
            s.objects("dom-a", Area::Excerpts, &[], true).await.unwrap(),
            vec!["mem-a-1"]
        );
        assert_eq!(
            s.objects("dom-a", Area::Excerpts, &["mem-a-1"], false)
                .await
                .unwrap(),
            vec!["sess-1.3.json"]
        );
        assert!(s
            .objects("dom-b", Area::Excerpts, &[], true)
            .await
            .unwrap()
            .is_empty());
        s.remove_object("dom-a", Area::Excerpts, &["mem-a-1", "sess-1.3.json"])
            .await
            .unwrap();
        s.remove_object("dom-a", Area::Excerpts, &["mem-a-1", "sess-1.3.json"])
            .await
            .unwrap();
        assert!(s
            .get_object("dom-a", Area::Excerpts, &["mem-a-1", "sess-1.3.json"])
            .await
            .unwrap()
            .is_none());
        assert!(matches!(
            s.put_object("dom-a", Area::Transcripts, &["../x"], vec![])
                .await,
            Err(Error::BadName(_))
        ));
    }

    /// How many files are under `dir`, and which of them mention `word`.
    fn files_under(dir: &Path, word: &str) -> (usize, Vec<std::path::PathBuf>) {
        let (mut files, mut holding) = (0, Vec::new());
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for f in std::fs::read_dir(&d).unwrap().flatten() {
                if f.path().is_dir() {
                    stack.push(f.path());
                } else {
                    files += 1;
                    if std::fs::read_to_string(f.path()).unwrap().contains(word) {
                        holding.push(f.path());
                    }
                }
            }
        }
        (files, holding)
    }

    #[tokio::test]
    async fn erasure_writes_the_stub_over_every_version_and_keeps_a_receipt() {
        use dm_core::erasure::{stub, Reason, Receipt};
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open_dir(dir.path()).unwrap();
        for i in 1..=3 {
            s.put(&entry(
                "dom-a",
                "mem-a-1",
                &format!("version {i} of the secret\n"),
            ))
            .await
            .unwrap();
        }
        s.put(&entry("dom-a", "mem-a-2", "another entry, kept\n"))
            .await
            .unwrap();
        let current = s.get("dom-a", "mem-a-1").await.unwrap().unwrap();
        let before = s.digests("dom-a", "mem-a-1").await.unwrap();
        assert_eq!(before.len(), 3, "read before anything is removed");
        let removed = s
            .erase("dom-a", "mem-a-1", &stub(&current, Reason::Mistaken))
            .await
            .unwrap();
        assert_eq!(removed, before, "the current version and two archived ones");
        assert_eq!(
            removed[0].body_digest.as_deref(),
            Some(dm_core::erasure::body_digest("version 3 of the secret\n").as_str())
        );
        assert_eq!(removed[0].version, "current");
        assert_eq!(removed[0].digest, digest(&current.render()));
        let after = s.get("dom-a", "mem-a-1").await.unwrap().unwrap();
        assert_eq!(after.front.status, Some(Status::Erased));
        assert_eq!(after.body, "");
        for v in s.history("dom-a", "mem-a-1").await.unwrap() {
            assert_eq!(Entry::parse(&v.text).unwrap().body, "", "{}", v.name);
        }
        let receipt = Receipt {
            id: "mem-a-1".into(),
            domain: "dom-a".into(),
            erased_at: OffsetDateTime::UNIX_EPOCH,
            erased_by: "human:test:me".into(),
            reason: Reason::Mistaken,
            removed,
            flagged: vec![],
            unresolved: vec![],
        };
        s.put_receipt(&receipt).await.unwrap();
        assert!(receipt.to_yaml().contains("body_digest:"));
        assert_eq!(
            s.receipt("dom-a", "mem-a-1").await.unwrap().as_ref(),
            Some(&receipt)
        );
        assert_eq!(s.receipt("dom-a", "mem-a-2").await.unwrap(), None);
        assert_eq!(s.receipts("dom-a").await.unwrap(), vec![receipt]);
        assert!(s.receipts("dom-b").await.unwrap().is_empty());
        // the word is in no file under the root, and the other entry is untouched
        let (files, holding) = files_under(dir.path(), "secret");
        assert!(holding.is_empty(), "{holding:?}");
        assert_eq!(
            files, 5,
            "current, two archived, the receipt, the other entry"
        );
        assert_eq!(s.list("dom-a").await.unwrap(), vec!["mem-a-1", "mem-a-2"]);
        let missing = stub(&entry("dom-a", "mem-a-9", "x\n"), Reason::Legal);
        assert!(matches!(
            s.erase("dom-a", "mem-a-9", &missing).await,
            Err(Error::NotFound { .. })
        ));
    }
}
