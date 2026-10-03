//! The surface agents see: the operations of the paper's Appendix A, as one service the
//! CLI and the MCP handler both call. Nothing lives only in a binding.
//!
//! A [`Memory`] is one root directory, mounted deliberately:
//!
//! ```text
//! <root>/grants.toml        what the owner may read, and where things may go
//! <root>/record/            the record (dm-store), versioned Markdown
//! <root>/state/index.db     the index (dm-index), derived, rebuildable
//! <root>/state/ledger.db    the ledger (dm-ledger), events plus projection
//! ```
//!
//! Every read goes through the grants' filter. Every write is classified at least as high
//! as what the session has read. The session's label rises and never falls.

use dm_core::{Confidence, Entry, EntryType, Frontmatter, Label, ReadFilter, TrustTier};
use dm_index::{Hit, Index};
use dm_ledger::{Claim, Created, Ledger, Task, TaskSpec};
use dm_policy::{Grants, Session};
use dm_store::Store;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use time::{Date, Duration, OffsetDateTime};
use tokio::sync::Mutex as AsyncMutex;

pub mod bundle;
pub mod mcp;
pub mod reads;
pub mod t0;

/// The grants file, relative to the root.
pub const GRANTS_FILE: &str = "grants.toml";
/// The index, relative to the root.
pub const INDEX_FILE: &str = "state/index.db";
/// The ledger, relative to the root.
pub const LEDGER_FILE: &str = "state/ledger.db";

/// What can go wrong at the surface.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The record.
    #[error(transparent)]
    Store(#[from] dm_store::Error),
    /// The index.
    #[error(transparent)]
    Index(#[from] dm_index::Error),
    /// The ledger.
    #[error(transparent)]
    Ledger(#[from] dm_ledger::Error),
    /// The grants file.
    #[error(transparent)]
    Policy(#[from] dm_policy::Error),
    /// An entry that does not parse or build.
    #[error(transparent)]
    Core(#[from] dm_core::Error),
    /// A bundle.
    #[error(transparent)]
    Bundle(#[from] bundle::Error),
    /// The root directory.
    #[error("{path}: {source}")]
    Io {
        /// The path.
        path: String,
        /// Why.
        source: std::io::Error,
    },
    /// The grants do not allow it.
    #[error("not allowed: {0}")]
    Denied(String),
    /// Not there, or not for this session to see. The two are deliberately the same.
    #[error("not found: {0}")]
    NotFound(String),
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> Error + '_ {
    move |source| Error::Io {
        path: path.display().to_string(),
        source,
    }
}

/// What a caller asks to write. The service fills in id, author, time, and the
/// classification floor.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct WriteRequest {
    /// The domain; the only granted domain when omitted.
    #[serde(default)]
    pub domain: Option<String>,
    /// The kind of entry.
    #[serde(default = "default_type")]
    pub entry_type: EntryType,
    /// A title; else the body's first heading, else the id.
    #[serde(default)]
    pub title: Option<String>,
    /// The Markdown body.
    pub body: String,
    /// Links or citations.
    #[serde(default)]
    pub source: Vec<String>,
    /// The author's confidence.
    #[serde(default)]
    pub confidence: Option<Confidence>,
    /// A requested classification. Raised to the session label and the domain default.
    #[serde(default)]
    pub classification: Option<Label>,
    /// The entry this one replaces.
    #[serde(default)]
    pub supersedes: Option<String>,
    /// When to check it again.
    #[serde(default)]
    pub review_by: Option<Date>,
    /// A one-line summary; else the body's first paragraph.
    #[serde(default)]
    pub summary: Option<String>,
}

fn default_type() -> EntryType {
    EntryType::Fact
}

/// What a write produced.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Written {
    /// The new entry's id.
    pub id: String,
    /// The classification it was given.
    pub classification: Label,
    /// Its domain.
    pub domain: String,
}

/// One root, opened.
pub struct Memory {
    root: PathBuf,
    grants: Grants,
    author: String,
    manifest: Vec<String>,
    session_id: String,
    store: Store,
    index: AsyncMutex<Index>,
    ledger: AsyncMutex<Ledger>,
    session: Mutex<Session>,
    reads: reads::ReadLog,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Memory {
    /// Create a root: the grants file for one owner and one domain, the record, the
    /// state directory, an empty index and ledger. Refuses a root that already has a
    /// grants file.
    ///
    /// # Errors
    ///
    /// I/O on the root, or [`Error::Denied`] if it is already initialised.
    pub fn init(root: &Path, owner: &str, domain: &str) -> Result<Memory, Error> {
        let grants_path = root.join(GRANTS_FILE);
        if grants_path.exists() {
            return Err(Error::Denied(format!(
                "{} already exists",
                grants_path.display()
            )));
        }
        std::fs::create_dir_all(root.join("state")).map_err(io(root))?;
        std::fs::create_dir_all(root.join("record").join(domain)).map_err(io(root))?;
        let grants = Grants::starter(owner, domain);
        std::fs::write(&grants_path, grants.render()?).map_err(io(&grants_path))?;
        Memory::open(root)
    }

    /// Open an initialised root.
    ///
    /// # Errors
    ///
    /// A missing or bad grants file, or a store, index, or ledger that will not open.
    pub fn open(root: &Path) -> Result<Memory, Error> {
        let grants = Grants::load(&root.join(GRANTS_FILE))?;
        std::fs::create_dir_all(root.join("state")).map_err(io(root))?;
        let store = Store::open_dir(root)?;
        let index = Index::open(&root.join(INDEX_FILE))?;
        let ledger = Ledger::open(&root.join(LEDGER_FILE))?;
        let session = grants.session();
        let session_id = dm_core::id::new_task_id("session").replace("task-session-", "sess-");
        Ok(Memory {
            root: root.to_path_buf(),
            author: grants.owner.clone(),
            grants,
            manifest: Vec::new(),
            reads: reads::ReadLog::new(root, &session_id),
            session_id,
            store,
            index: AsyncMutex::new(index),
            ledger: AsyncMutex::new(ledger),
            session: Mutex::new(session),
        })
    }

    /// Record writes under this author instead of the owner: `agent:<name> (model: <id>)`.
    #[must_use]
    pub fn with_author(mut self, author: impl Into<String>) -> Memory {
        self.author = author.into();
        self
    }

    /// The capabilities this session's manifest carries, matched against task
    /// requirements on claim.
    #[must_use]
    pub fn with_manifest(mut self, manifest: Vec<String>) -> Memory {
        self.manifest = manifest;
        self
    }

    /// Cap what this session may read at `cap`, whatever the grants say. The launcher
    /// sets this from the model destination it allowed: a session whose model calls go
    /// to a destination with a public ceiling reads only public entries, so nothing it
    /// reads can leave above that ceiling.
    #[must_use]
    pub fn with_handling_cap(mut self, cap: Label) -> Memory {
        if cap < self.grants.handling {
            self.grants.handling = cap;
            self.session = Mutex::new(self.grants.session());
        }
        self
    }

    /// A short identifier for this session, for telemetry.
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// The root directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The grants in force.
    #[must_use]
    pub fn grants(&self) -> &Grants {
        &self.grants
    }

    /// Who writes are attributed to.
    #[must_use]
    pub fn author(&self) -> &str {
        &self.author
    }

    /// The session's current label.
    #[must_use]
    pub fn label(&self) -> Label {
        lock(&self.session).label
    }

    /// Granted domains, sorted.
    #[must_use]
    pub fn domains(&self) -> Vec<String> {
        self.grants.domains.keys().cloned().collect()
    }

    /// The domain a request means: the one named if granted, else the only granted one.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for an ungranted domain, or when none is named and several are
    /// granted.
    pub fn resolve_domain(&self, domain: Option<&str>) -> Result<String, Error> {
        match domain {
            Some(d) if self.grants.may_write(d) => Ok(d.to_string()),
            Some(d) => Err(Error::Denied(format!("domain {d} is not granted"))),
            None => {
                let mut keys = self.grants.domains.keys();
                match (keys.next(), keys.next()) {
                    (Some(only), None) => Ok(only.clone()),
                    _ => Err(Error::Denied("name a domain; several are granted".into())),
                }
            }
        }
    }

    fn filter_for(&self, domain: Option<&str>) -> Result<ReadFilter, Error> {
        let mut f = self.grants.read_filter();
        if let Some(d) = domain {
            let d = self.resolve_domain(Some(d))?;
            f.domains = vec![d];
        }
        Ok(f)
    }

    fn observe(&self, hits: &[Hit]) {
        let mut s = lock(&self.session);
        for h in hits {
            s.observe(h.classification);
        }
    }

    /// Ranked search, summaries only, within the session's filter.
    ///
    /// # Errors
    ///
    /// An ungranted domain, or an index error.
    pub async fn search(
        &self,
        query: &str,
        domain: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Hit>, Error> {
        let f = self.filter_for(domain)?;
        let hits = self.index.lock().await.search(query, &f, limit)?;
        self.observe(&hits);
        self.reads.hits("search", Some(query), &hits);
        Ok(hits)
    }

    /// Entries of one kind, newest first, within the session's filter.
    ///
    /// # Errors
    ///
    /// An ungranted domain, or an index error.
    pub async fn list(
        &self,
        domain: Option<&str>,
        kind: Option<EntryType>,
        limit: usize,
    ) -> Result<Vec<Hit>, Error> {
        let f = self.filter_for(domain)?;
        let hits = self.index.lock().await.list(&f, kind, limit)?;
        self.observe(&hits);
        self.reads.hits("list", None, &hits);
        Ok(hits)
    }

    /// One entry, body included, if the filter admits it. Raises the session label.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] when absent or not for this session, or a store error.
    pub async fn get(&self, id: &str) -> Result<Entry, Error> {
        let f = self.grants.read_filter();
        let meta = self
            .index
            .lock()
            .await
            .meta(id, &f)?
            .ok_or_else(|| Error::NotFound(id.to_string()))?;
        let entry = self
            .store
            .get(&meta.hit.domain, id)
            .await?
            .ok_or_else(|| Error::NotFound(id.to_string()))?;
        lock(&self.session).observe(entry.front.classification);
        self.reads.text("get", &entry.body);
        Ok(entry)
    }

    /// Write an entry at raw trust, attributed to this session's author, classified at
    /// least as high as what the session has read.
    ///
    /// # Errors
    ///
    /// An ungranted domain, an empty body, or a store or index error.
    pub async fn write(&self, req: WriteRequest) -> Result<Written, Error> {
        let domain = self.resolve_domain(req.domain.as_deref())?;
        if req.body.trim().is_empty() {
            return Err(Error::Denied("an entry needs a body".into()));
        }
        let classification = {
            let s = lock(&self.session);
            self.grants
                .classification_for_write(&domain, req.classification, &s)
                .ok_or_else(|| Error::Denied(format!("domain {domain} is not granted")))?
        };
        let entry = Entry {
            front: Frontmatter {
                id: dm_core::id::new_entry_id(&domain),
                entry_type: req.entry_type,
                domain: domain.clone(),
                author: self.author.clone(),
                created: OffsetDateTime::now_utc(),
                source: req.source,
                confidence: req.confidence,
                trust_tier: TrustTier::Raw,
                review_by: req.review_by,
                supersedes: req.supersedes,
                policy_context: Vec::new(),
                classification,
                status: None,
                origin: None,
                summary: req.summary,
                title: req.title,
            },
            body: req.body,
        };
        self.store.put(&entry).await?;
        {
            let mut ix = self.index.lock().await;
            ix.upsert(&entry)?;
            if let Some(old) = &entry.front.supersedes {
                if let Some(old_entry) = self.store.get(&domain, old).await? {
                    ix.upsert(&old_entry)?;
                }
            }
        }
        Ok(Written {
            id: entry.front.id,
            classification,
            domain,
        })
    }

    /// The T0 for a domain: the latest handoffs, standing decisions, conventions, and
    /// pointers, under the budget. What a session loads first.
    ///
    /// # Errors
    ///
    /// An ungranted domain, or an index or ledger error.
    pub async fn t0(&self, domain: Option<&str>) -> Result<String, Error> {
        let domain = self.resolve_domain(domain)?;
        let f = self.filter_for(Some(&domain))?;
        let (handoffs, decisions, conventions, total, due) = {
            let ix = self.index.lock().await;
            let mut conventions = ix.list(&f, Some(EntryType::Convention), 20)?;
            conventions.extend(ix.list(&f, Some(EntryType::Method), 20)?);
            (
                ix.list(&f, Some(EntryType::Handoff), 5)?,
                ix.list(&f, Some(EntryType::Decision), 30)?,
                conventions,
                ix.count(&f)?,
                ix.due_for_review(&f, OffsetDateTime::now_utc().date(), 1000)?
                    .len(),
            )
        };
        let waiting = self
            .ledger
            .lock()
            .await
            .list(&domain, dm_ledger::Status::Admitted)?
            .len();
        self.observe(&handoffs);
        self.observe(&decisions);
        self.observe(&conventions);
        let generated_by = format!("memory {}", env!("CARGO_PKG_VERSION"));
        let text = t0::render(&t0::Inputs {
            domain: &domain,
            generated_by: &generated_by,
            handoffs: &handoffs,
            decisions: &decisions,
            conventions: &conventions,
            total_active: total,
            due_for_review: due,
            tasks_waiting: waiting,
        });
        self.reads.text("t0", &text);
        Ok(text)
    }

    // ---- tasks ----

    fn task_in_grants(&self, task: &Task) -> Result<(), Error> {
        if self.grants.may_write(&task.spec.domain) {
            Ok(())
        } else {
            Err(Error::NotFound(task.id.clone()))
        }
    }

    /// File a task in a granted domain. It is born proposed.
    ///
    /// # Errors
    ///
    /// An ungranted domain, or a ledger error.
    pub async fn create_task(&self, mut spec: TaskSpec) -> Result<Created, Error> {
        spec.domain = self.resolve_domain(
            Some(&spec.domain)
                .filter(|d| !d.is_empty())
                .map(String::as_str),
        )?;
        spec.created_by = self.author.clone();
        spec.classification = spec.classification.max(self.label());
        Ok(self.ledger.lock().await.create(spec)?)
    }

    /// Admit a proposed task. On a laptop the owner is the triage.
    ///
    /// # Errors
    ///
    /// A ledger error, or a task outside the grants.
    pub async fn admit(&self, id: &str) -> Result<Task, Error> {
        let mut l = self.ledger.lock().await;
        self.task_in_grants(&l.get(id)?)?;
        Ok(l.admit(id)?)
    }

    /// Claim the highest-ranked claimable task in a domain that this session's manifest
    /// can take.
    ///
    /// # Errors
    ///
    /// An ungranted domain, or a ledger error.
    pub async fn claim_next(
        &self,
        domain: Option<&str>,
        lease: Duration,
    ) -> Result<Option<Claim>, Error> {
        let domain = self.resolve_domain(domain)?;
        let claim =
            self.ledger
                .lock()
                .await
                .claim_next(&domain, &self.author, lease, &self.manifest)?;
        if let Some(c) = &claim {
            lock(&self.session).observe(c.task.spec.classification);
        }
        Ok(claim)
    }

    /// Claim one named task.
    ///
    /// # Errors
    ///
    /// A ledger error, or a task outside the grants.
    pub async fn claim(&self, id: &str, lease: Duration) -> Result<Claim, Error> {
        let mut l = self.ledger.lock().await;
        self.task_in_grants(&l.get(id)?)?;
        let c = l.claim(id, &self.author, lease, &self.manifest)?;
        lock(&self.session).observe(c.task.spec.classification);
        Ok(c)
    }

    /// Renew this session's lease on a task.
    ///
    /// # Errors
    ///
    /// A ledger error, including a lease already lost, or a task outside the grants.
    pub async fn heartbeat(&self, id: &str, lease: Duration) -> Result<Task, Error> {
        let mut l = self.ledger.lock().await;
        self.task_in_grants(&l.get(id)?)?;
        Ok(l.heartbeat(id, &self.author, lease)?)
    }

    /// Complete a task with the handoff entry that records what happened. The handoff
    /// must exist in the task's domain.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for a missing handoff, a ledger error, or a task outside the
    /// grants.
    pub async fn complete(&self, id: &str, handoff: &str) -> Result<Task, Error> {
        let task = self.ledger.lock().await.get(id)?;
        self.task_in_grants(&task)?;
        if self.store.get(&task.spec.domain, handoff).await?.is_none() {
            return Err(Error::NotFound(format!("handoff {handoff}")));
        }
        Ok(self
            .ledger
            .lock()
            .await
            .complete(id, &self.author, handoff)?)
    }

    /// Give a task back to the pool.
    ///
    /// # Errors
    ///
    /// A ledger error, or a task outside the grants.
    pub async fn release(&self, id: &str, handoff: Option<&str>) -> Result<Task, Error> {
        if let Some(h) = handoff {
            self.get(h).await?; // the handoff must exist and be readable here
        }
        let mut l = self.ledger.lock().await;
        self.task_in_grants(&l.get(id)?)?;
        Ok(l.release(id, &self.author, handoff)?)
    }

    /// Close a task without doing it.
    ///
    /// # Errors
    ///
    /// A ledger error, or a task outside the grants.
    pub async fn invalidate(
        &self,
        id: &str,
        reason: &str,
        superseded_by: Option<&str>,
    ) -> Result<Task, Error> {
        let mut l = self.ledger.lock().await;
        self.task_in_grants(&l.get(id)?)?;
        Ok(l.invalidate(id, reason, superseded_by)?)
    }

    /// One task.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`], including for a task outside the grants.
    pub async fn get_task(&self, id: &str) -> Result<Task, Error> {
        let t = self.ledger.lock().await.get(id)?;
        self.task_in_grants(&t)?;
        Ok(t)
    }

    /// Tasks in a domain with a status, ranked.
    ///
    /// # Errors
    ///
    /// An ungranted domain, or a ledger error.
    pub async fn tasks(
        &self,
        domain: Option<&str>,
        status: dm_ledger::Status,
    ) -> Result<Vec<Task>, Error> {
        let domain = self.resolve_domain(domain)?;
        Ok(self.ledger.lock().await.list(&domain, status)?)
    }

    // ---- leaving, arriving, rebuilding ----

    /// Export a domain at or below a label into a bundle directory.
    ///
    /// # Errors
    ///
    /// An ungranted domain, a label above the session's handling, or a bundle error.
    pub async fn export(
        &self,
        domain: Option<&str>,
        max_label: Label,
        dest: &Path,
    ) -> Result<bundle::Exported, Error> {
        let domain = self.resolve_domain(domain)?;
        if max_label > self.grants.handling {
            return Err(Error::Denied(format!(
                "export at {max_label} exceeds handling {}",
                self.grants.handling
            )));
        }
        let f = self.grants.read_filter();
        let ix = self.index.lock().await;
        Ok(bundle::export(
            &self.store,
            &ix,
            &f,
            &domain,
            max_label,
            &self.grants.owner,
            dest,
        )
        .await?)
    }

    /// Import a bundle into a granted domain. Entries land raw unless this owner
    /// exported them.
    ///
    /// # Errors
    ///
    /// An ungranted domain, or a bundle error.
    pub async fn import(
        &self,
        src: &Path,
        into_domain: Option<&str>,
    ) -> Result<bundle::Imported, Error> {
        let domain = self.resolve_domain(into_domain)?;
        let default = self
            .grants
            .domains
            .get(&domain)
            .map(|d| d.default_classification)
            .unwrap_or_default();
        let mut ix = self.index.lock().await;
        Ok(bundle::import(
            &self.store,
            &mut ix,
            &self.grants.owner,
            src,
            &domain,
            default,
        )
        .await?)
    }

    /// Rebuild the index from the record and the ledger's projection from its events.
    /// Returns entries indexed and events replayed.
    ///
    /// # Errors
    ///
    /// A store, index, or ledger error.
    pub async fn reindex(&self) -> Result<(usize, u64), Error> {
        let entries = self.store.all().await?;
        let n = self.index.lock().await.rebuild(entries)?;
        let m = self.ledger.lock().await.rebuild()?;
        Ok((n, m))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dm_ledger::Status;

    fn mem(dir: &Path) -> Memory {
        Memory::init(dir, "human:test:me", "dom-me").unwrap()
    }

    fn req(body: &str) -> WriteRequest {
        WriteRequest {
            body: body.into(),
            ..WriteRequest::default()
        }
    }

    fn spec(title: &str) -> TaskSpec {
        TaskSpec {
            domain: String::new(),
            title: title.into(),
            body: "Do it.\n".into(),
            requirements: vec![],
            depends_on: vec![],
            origin: None,
            upstream_event: None,
            created_by: String::new(),
            classification: Label::Public,
            priority: 0,
        }
    }

    #[tokio::test]
    async fn init_lays_out_the_root_and_open_reads_it_back() {
        let dir = tempfile::tempdir().unwrap();
        let m = mem(dir.path());
        assert!(dir.path().join(GRANTS_FILE).is_file());
        assert!(dir.path().join("record/dom-me").is_dir());
        assert!(dir.path().join(INDEX_FILE).is_file());
        assert!(dir.path().join(LEDGER_FILE).is_file());
        assert_eq!(m.domains(), vec!["dom-me"]);
        assert_eq!(m.author(), "human:test:me");
        assert!(matches!(
            Memory::init(dir.path(), "x", "y"),
            Err(Error::Denied(_))
        ));
        drop(m);
        let again = Memory::open(dir.path()).unwrap();
        assert_eq!(again.resolve_domain(None).unwrap(), "dom-me");
        assert!(matches!(
            again.resolve_domain(Some("dom-x")),
            Err(Error::Denied(_))
        ));
    }

    #[tokio::test]
    async fn write_search_get_and_the_label_rises() {
        let dir = tempfile::tempdir().unwrap();
        let m = mem(dir.path());
        let w = m
            .write(WriteRequest {
                title: Some("Retry policy".into()),
                ..req("Operation R retries with exponential backoff.\n")
            })
            .await
            .unwrap();
        assert_eq!(w.classification, Label::Internal, "the domain default");
        assert_eq!(m.label(), Label::Public, "writing reads nothing");
        let hits = m.search("backoff", None, 10).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, w.id);
        assert_eq!(m.label(), Label::Internal, "a summary is a read");
        let e = m.get(&w.id).await.unwrap();
        assert_eq!(e.front.author, "human:test:me");
        assert_eq!(e.front.trust_tier, TrustTier::Raw);
        assert!(e.body.contains("exponential"));
        assert!(matches!(
            m.get("mem-me-none").await,
            Err(Error::NotFound(_))
        ));
        let w2 = m
            .write(WriteRequest {
                classification: Some(Label::Public),
                ..req("Written after reading internal.\n")
            })
            .await
            .unwrap();
        assert_eq!(
            w2.classification,
            Label::Internal,
            "writes inherit the label"
        );
        assert!(matches!(m.write(req("   ")).await, Err(Error::Denied(_))));
        assert!(matches!(
            m.write(WriteRequest {
                domain: Some("dom-else".into()),
                ..req("x")
            })
            .await,
            Err(Error::Denied(_))
        ));
    }

    #[tokio::test]
    async fn a_fresh_session_recovers_from_t0_and_the_handoff() {
        let dir = tempfile::tempdir().unwrap();
        {
            let first = mem(dir.path());
            first
                .write(WriteRequest {
                    entry_type: EntryType::Decision,
                    title: Some("Use SQLite for the pilot".into()),
                    ..req("Nothing to operate.\n")
                })
                .await
                .unwrap();
            first
                .write(WriteRequest {
                    entry_type: EntryType::Handoff,
                    title: Some("Stopped at step 3 of the import".into()),
                    ..req("Steps 1 and 2 done; the manifest parses. Next: digests.\n")
                })
                .await
                .unwrap();
        }
        let fresh = Memory::open(dir.path()).unwrap();
        let t0 = fresh.t0(None).await.unwrap();
        assert!(t0.contains("Stopped at step 3 of the import"), "{t0}");
        assert!(t0.contains("Next: digests."), "{t0}");
        assert!(t0.contains("Use SQLite for the pilot"), "{t0}");
        assert!(t0.contains("2 active entries"), "{t0}");
        assert!(dm_core::approx_tokens(&t0) <= t0::BUDGET_TOKENS);
        let handoffs = fresh.list(None, Some(EntryType::Handoff), 5).await.unwrap();
        assert_eq!(handoffs.len(), 1);
        let h = fresh.get(&handoffs[0].id).await.unwrap();
        assert!(h.body.contains("Next: digests"));
    }

    #[tokio::test]
    async fn tasks_end_to_end_and_complete_needs_a_real_handoff() {
        let dir = tempfile::tempdir().unwrap();
        let m = mem(dir.path()).with_manifest(vec!["tool:fs".into()]);
        let c = m.create_task(spec("Write the import")).await.unwrap();
        assert_eq!(c.task.status, Status::Proposed);
        assert_eq!(c.task.spec.domain, "dom-me");
        assert_eq!(c.task.spec.created_by, "human:test:me");
        assert!(m
            .claim_next(None, Duration::minutes(30))
            .await
            .unwrap()
            .is_none());
        m.admit(&c.task.id).await.unwrap();
        assert_eq!(m.tasks(None, Status::Admitted).await.unwrap().len(), 1);
        let claim = m
            .claim_next(None, Duration::minutes(30))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claim.task.id, c.task.id);
        m.heartbeat(&c.task.id, Duration::minutes(30))
            .await
            .unwrap();
        assert!(matches!(
            m.complete(&c.task.id, "mem-me-nonexistent").await,
            Err(Error::NotFound(_))
        ));
        let h = m
            .write(WriteRequest {
                entry_type: EntryType::Handoff,
                ..req("Done; see the import module.\n")
            })
            .await
            .unwrap();
        let done = m.complete(&c.task.id, &h.id).await.unwrap();
        assert_eq!(done.status, Status::Completed);
        assert_eq!(done.handoff.as_deref(), Some(h.id.as_str()));
        assert_eq!(
            m.get_task(&c.task.id).await.unwrap().status,
            Status::Completed
        );
        assert!(matches!(
            m.get_task("task-me-none").await,
            Err(Error::Ledger(_))
        ));
        let needs = TaskSpec {
            requirements: vec!["tool:github".into()],
            ..spec("Needs github")
        };
        let t = m.create_task(needs).await.unwrap().task;
        m.admit(&t.id).await.unwrap();
        assert!(
            m.claim_next(None, Duration::minutes(5))
                .await
                .unwrap()
                .is_none(),
            "manifest lacks it"
        );
        m.invalidate(&t.id, "obsolete", None).await.unwrap();
    }

    #[tokio::test]
    async fn export_then_import_elsewhere_lands_raw_and_dedups() {
        let a_dir = tempfile::tempdir().unwrap();
        let a = Memory::init(a_dir.path(), "human:test:alice", "dom-a").unwrap();
        let w = a
            .write(WriteRequest {
                title: Some("Alice's decision".into()),
                entry_type: EntryType::Decision,
                classification: Some(Label::Public),
                ..req("Alice decided.\n")
            })
            .await
            .unwrap();
        let secret = a
            .write(WriteRequest {
                classification: Some(Label::Restricted),
                ..req("Alice's secret.\n")
            })
            .await
            .unwrap();
        let bundle_dir = tempfile::tempdir().unwrap();
        let ex = a
            .export(None, Label::Internal, bundle_dir.path())
            .await
            .unwrap();
        let ids: Vec<_> = ex.manifest.entries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec![w.id.as_str()], "the restricted entry stays home");
        assert_eq!(ex.manifest.max_label, Label::Internal);
        assert_eq!(
            ex.manifest.highest_label,
            Label::Public,
            "what is inside, not the bound"
        );
        assert!(!bundle_dir.path().join(format!("{}.md", secret.id)).exists());
        let full_dir = tempfile::tempdir().unwrap();
        let full = a
            .export(None, Label::Restricted, full_dir.path())
            .await
            .unwrap();
        assert_eq!(
            full.manifest.entries.len(),
            2,
            "at handling, everything leaves"
        );
        assert_eq!(full.manifest.highest_label, Label::Restricted);

        let b_dir = tempfile::tempdir().unwrap();
        let b = Memory::init(b_dir.path(), "human:test:bob", "dom-b").unwrap();
        let im = b.import(bundle_dir.path(), None).await.unwrap();
        assert_eq!(im.written.len(), 1);
        assert!(im.skipped.is_empty());
        let got = b.get(&w.id).await.unwrap();
        assert_eq!(got.front.domain, "dom-b");
        assert_eq!(got.front.trust_tier, TrustTier::Raw);
        assert_eq!(got.front.author, "human:test:alice", "provenance kept");
        assert_eq!(
            got.front.origin.as_deref(),
            Some(format!("bundle:human:test:alice/dom-a/{}", w.id).as_str())
        );
        let again = b.import(bundle_dir.path(), None).await.unwrap();
        assert!(again.written.is_empty());
        assert_eq!(
            again.skipped.len(),
            1,
            "a second import creates nothing twice"
        );

        // a tampered entry is refused before anything is written
        let c_dir = tempfile::tempdir().unwrap();
        let c = Memory::init(c_dir.path(), "human:test:carol", "dom-c").unwrap();
        let p = bundle_dir.path().join(format!("{}.md", w.id));
        let mut text = std::fs::read_to_string(&p).unwrap();
        text.push_str("tampered\n");
        std::fs::write(&p, text).unwrap();
        assert!(matches!(
            c.import(bundle_dir.path(), None).await,
            Err(Error::Bundle(bundle::Error::Invalid(_)))
        ));
        assert_eq!(c.search("Alice", None, 10).await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn a_manifest_path_that_leaves_the_bundle_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let m = mem(dir.path());
        let bad = tempfile::tempdir().unwrap();
        let mut man = dm_core::manifest::Manifest::new("dom-x", "human:test:eve", Label::Public);
        man.push(dm_core::manifest::ManifestEntry {
            id: "mem-x-1".into(),
            path: "../grants.toml".into(),
            digest: "00".into(),
            classification: Label::Public,
        })
        .unwrap();
        std::fs::write(bad.path().join("manifest.yaml"), man.to_yaml()).unwrap();
        assert!(matches!(
            m.import(bad.path(), None).await,
            Err(Error::Bundle(bundle::Error::Invalid(_)))
        ));
    }

    #[tokio::test]
    async fn a_handling_cap_lowers_what_the_session_reads() {
        let dir = tempfile::tempdir().unwrap();
        {
            let m = mem(dir.path());
            m.write(WriteRequest {
                classification: Some(Label::Internal),
                ..req("internal note\n")
            })
            .await
            .unwrap();
            m.write(WriteRequest {
                classification: Some(Label::Public),
                ..req("public note\n")
            })
            .await
            .unwrap();
        }
        let capped = Memory::open(dir.path())
            .unwrap()
            .with_handling_cap(Label::Public);
        assert_eq!(capped.grants().handling, Label::Public);
        assert_eq!(capped.search("note", None, 10).await.unwrap().len(), 1);
        let full = Memory::open(dir.path())
            .unwrap()
            .with_handling_cap(Label::Restricted);
        assert_eq!(
            full.grants().handling,
            Label::Restricted,
            "a cap never raises"
        );
        assert_eq!(full.search("note", None, 10).await.unwrap().len(), 2);
        assert!(full.session_id().starts_with("sess-"));
    }

    #[tokio::test]
    async fn reindex_rebuilds_from_the_record_alone() {
        let dir = tempfile::tempdir().unwrap();
        let ids: Vec<String> = {
            let m = mem(dir.path());
            let mut ids = Vec::new();
            for i in 0..3 {
                ids.push(
                    m.write(req(&format!("entry number {i} about kafka\n")))
                        .await
                        .unwrap()
                        .id,
                );
            }
            let t = m.create_task(spec("A task")).await.unwrap().task;
            m.admit(&t.id).await.unwrap();
            ids
        };
        std::fs::remove_file(dir.path().join(INDEX_FILE)).unwrap();
        let m = Memory::open(dir.path()).unwrap();
        assert!(
            m.search("kafka", None, 10).await.unwrap().is_empty(),
            "the index is gone"
        );
        let (n, events) = m.reindex().await.unwrap();
        assert_eq!(n, 3);
        assert_eq!(events, 2);
        let mut found: Vec<String> = m
            .search("kafka", None, 10)
            .await
            .unwrap()
            .into_iter()
            .map(|h| h.id)
            .collect();
        found.sort();
        let mut want = ids;
        want.sort();
        assert_eq!(found, want);
        assert_eq!(m.tasks(None, Status::Admitted).await.unwrap().len(), 1);
    }
}
