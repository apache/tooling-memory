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

use dm_core::erasure::{self, Reason, Receipt};
use dm_core::{Confidence, Entry, EntryType, Frontmatter, Label, ReadFilter, Status, TrustTier};
use dm_index::{Hit, Index};
use dm_ledger::{Claim, Created, Ledger, Task, TaskSpec};
use dm_policy::{Grants, Session};
use dm_store::Store;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError, Weak};
use time::{Date, Duration, OffsetDateTime};
use tokio::sync::Mutex as AsyncMutex;

pub mod bundle;
pub mod mcp;
pub mod projection;
pub mod reads;
pub mod recall;
pub mod t0;
mod taps;

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
    /// The recall deputy failed or timed out.
    #[error("deputy: {0}")]
    Deputy(String),
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
    /// The recall deputy, held weakly: it lives as long as the launcher that built it.
    deputy: OnceLock<Weak<dyn recall::Deputy>>,
    /// Whether this session has issued two gets inside one turn.
    fan_out: Mutex<recall::FanOut>,
    /// What packs pushed to this session, so a later `get` of the same version is
    /// counted as pushed and then opened.
    cache: Mutex<recall::Cache>,
    /// Where the record is projected as files for this session, when it is; refreshed
    /// after every write, import and erasure made through this service.
    projection: Option<PathBuf>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What a fan-out filed: the subagent tasks, the ones it did not file because the same
/// spawn already exists in the chain, and the continuation that waits on the new ones.
#[derive(Clone, Debug, Serialize)]
pub struct FanOut {
    /// Newly filed, one per item that was new.
    pub children: Vec<Task>,
    /// Items the chain already holds, filed by an earlier run of the same work: done,
    /// with a handoff, or still open. A resumed run asks for what it asked for before;
    /// the answer is the earlier task, not another.
    pub existing: Vec<Task>,
    /// Resumes the parent when the new children are done; none when nothing was new.
    pub continuation: Option<Task>,
}

/// A task with everything filed from it, recursively.
#[derive(Clone, Debug, Serialize)]
pub struct TaskTree {
    /// The task.
    #[serde(flatten)]
    pub task: Task,
    /// Its subagent tasks, continuations and retries, each with theirs.
    pub children: Vec<TaskTree>,
}

impl TaskTree {
    /// Every task in the tree, this one first.
    pub fn flatten(&self, out: &mut Vec<Task>) {
        out.push(self.task.clone());
        for c in &self.children {
            c.flatten(out);
        }
    }

    /// Every handoff in the tree, as `(task id, title, handoff entry id)`, this task first.
    pub fn handoffs(&self, out: &mut Vec<(String, String, String)>) {
        if let Some(h) = &self.task.handoff {
            out.push((
                self.task.id.clone(),
                self.task.spec.title.clone(),
                h.clone(),
            ));
        }
        for c in &self.children {
            c.handoffs(out);
        }
    }
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
            deputy: OnceLock::new(),
            fan_out: Mutex::new(recall::FanOut::default()),
            cache: Mutex::new(recall::Cache::default()),
            projection: None,
        })
    }

    /// Keep the record projected as files under `dir` for this session: rendered on
    /// demand with [`Memory::project_into`] and again after every write, import and
    /// erasure made through this service.
    #[must_use]
    pub fn with_projection(mut self, dir: PathBuf) -> Memory {
        self.projection = Some(dir);
        self
    }

    /// Read without recording: for a report or a tool whose reads are not a session's.
    #[must_use]
    pub fn without_read_log(self) -> Memory {
        self.disable_read_log();
        self
    }

    /// Stop recording reads from now on, behind a shared handle: for a measurement that
    /// runs over a memory a launcher already built.
    pub fn disable_read_log(&self) {
        self.reads.disable();
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
    /// The index decides admission and the record serves the body, always; a current
    /// version the record has moved above the session's handling, or out of the active
    /// set, is refused, whatever the index row said. When the body is the version a pack pushed to this session,
    /// the read log says so. Two admitted gets inside one turn mark the session as one
    /// that fans out.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] when absent or not for this session, or a store error.
    pub async fn get(&self, id: &str) -> Result<Entry, Error> {
        let f = self.grants.read_filter();
        let meta = self.index.lock().await.meta(id, &f)?;
        let Some(meta) = meta else {
            self.forget_pushed(id);
            return Err(Error::NotFound(id.to_string()));
        };
        self.note_get();
        let entry = self
            .store
            .get(&meta.hit.domain, id)
            .await?
            .ok_or_else(|| Error::NotFound(id.to_string()))?;
        if !lock(&self.session).observe(entry.front.classification)
            || (!entry.is_active() && !f.include_inactive)
        {
            self.forget_pushed(id);
            return Err(Error::NotFound(id.to_string()));
        }
        let pushed = self.was_pushed(&entry);
        self.reads
            .served("get", None, 1, dm_core::approx_tokens(&entry.body), pushed);
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
        self.refresh_projection().await;
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

    /// Whether tasks a session files from a task it holds are admitted as they are
    /// filed in this domain; see `admit_own` in the grants.
    fn admits_own(&self, domain: &str) -> bool {
        self.grants.domains.get(domain).is_some_and(|g| g.admit_own)
    }

    /// Admit `task` where the domain's grant says so and it is still proposed.
    fn admit_own_in(&self, l: &mut Ledger, task: Task) -> Result<Task, Error> {
        if task.status == dm_ledger::Status::Proposed && self.admits_own(&task.spec.domain) {
            Ok(l.admit(&task.id)?)
        } else {
            Ok(task)
        }
    }

    /// Fan a held task out: file the subagent tasks its run asked for, one per item,
    /// and the continuation that resumes the parent once they are done. A child carries
    /// `origin: task:<parent>` and a hash of its body, so the same spawn asked twice is
    /// one task; the continuation carries `origin: continues:<parent>`, depends on the
    /// children, inherits the parent's requirements and priority, and its prompt will
    /// hold every child's handoff and the parent's own, which is what resuming is. Where
    /// the domain admits a session's own filings, everything filed here is admitted.
    ///
    /// # Errors
    ///
    /// A parent outside the grants, or a ledger error.
    pub async fn fan_out(&self, parent: &str, items: &[(String, String)]) -> Result<FanOut, Error> {
        let mut l = self.ledger.lock().await;
        let parent = l.get(parent)?;
        self.task_in_grants(&parent)?;
        // the chain this parent belongs to: a continuation or a retry resumes a root,
        // and what an earlier run of the chain already asked for is not asked again
        let root = Memory::root_of(&l, &parent);
        let mut done = Vec::new();
        Memory::tree_in(&l, root)?.flatten(&mut done);
        let mut children = Vec::new();
        let mut existing = Vec::new();
        for (title, body) in items {
            let hash = dm_core::digest(body)[..16].to_string();
            if let Some(earlier) = done.iter().find(|t| {
                t.spec.upstream_event.as_deref() == Some(hash.as_str())
                    && t.spec
                        .origin
                        .as_deref()
                        .is_some_and(|o| o.starts_with("task:"))
            }) {
                existing.push(earlier.clone());
                continue;
            }
            let created = l.create(TaskSpec {
                domain: parent.spec.domain.clone(),
                title: title.clone(),
                body: body.clone(),
                requirements: Vec::new(),
                depends_on: Vec::new(),
                origin: Some(format!("task:{}", parent.id)),
                upstream_event: Some(hash),
                created_by: self.author.clone(),
                classification: parent.spec.classification.max(self.label()),
                priority: parent.spec.priority,
            })?;
            children.push(self.admit_own_in(&mut l, created.task)?);
        }
        if children.is_empty() {
            return Ok(FanOut {
                children,
                existing,
                continuation: None,
            });
        }
        let ids: Vec<String> = children.iter().map(|c| c.id.clone()).collect();
        let created = l.create(TaskSpec {
            domain: parent.spec.domain.clone(),
            title: format!(
                "Resume: {}",
                parent.spec.title.trim_start_matches("Resume: ")
            ),
            body: format!(
                "{}\n\n## Continuation\n\nThis resumes `{}`. The subagents its run asked for have run; their handoffs, and the run's own, are in the prompt above. Do not ask for them again: finish the task from what they found, and leave a handoff.",
                parent.spec.body.trim_end(),
                parent.id
            ),
            requirements: parent.spec.requirements.clone(),
            depends_on: ids.clone(),
            origin: Some(format!("continues:{}", parent.id)),
            upstream_event: Some(dm_core::digest(&ids.join(","))[..16].to_string()),
            created_by: self.author.clone(),
            classification: parent.spec.classification.max(self.label()),
            priority: parent.spec.priority,
        })?;
        let continuation = self.admit_own_in(&mut l, created.task)?;
        Ok(FanOut {
            children,
            existing,
            continuation: Some(continuation),
        })
    }

    /// The root of a task's chain: what a continuation or a retry ultimately resumes.
    fn root_of(l: &Ledger, task: &Task) -> Task {
        let mut current = task.clone();
        let mut seen = HashSet::new();
        while let Some(up) = current
            .spec
            .origin
            .as_deref()
            .and_then(|o| {
                o.strip_prefix("continues:")
                    .or_else(|| o.strip_prefix("retries:"))
            })
            .map(str::to_string)
        {
            if !seen.insert(up.clone()) {
                break;
            }
            match l.get(&up) {
                Ok(t) => current = t,
                Err(_) => break,
            }
        }
        current
    }

    /// A task with everything filed from it, recursively: its subagent tasks, its
    /// continuations and its retries, each with theirs.
    ///
    /// # Errors
    ///
    /// A task outside the grants, or a ledger error.
    pub async fn task_tree(&self, id: &str) -> Result<TaskTree, Error> {
        let l = self.ledger.lock().await;
        let task = l.get(id)?;
        self.task_in_grants(&task)?;
        Ok(Memory::tree_in(&l, task)?)
    }

    fn tree_in(l: &Ledger, task: Task) -> Result<TaskTree, dm_ledger::Error> {
        let mut children = Vec::new();
        for t in l.filed_from(&task.id)? {
            children.push(Memory::tree_in(l, t)?);
        }
        Ok(TaskTree { task, children })
    }

    /// File a task again from one that dead-ended: the same spec, with the chain's
    /// handoffs attached as lessons, so the next claimant starts from what was learned
    /// rather than from zero. The original's intent is untouched; the new task carries
    /// `origin: retries:<id>`.
    ///
    /// # Errors
    ///
    /// A task outside the grants, or a ledger error.
    pub async fn retry_task(&self, id: &str) -> Result<Task, Error> {
        let mut l = self.ledger.lock().await;
        let root = l.get(id)?;
        self.task_in_grants(&root)?;
        let tree = Memory::tree_in(&l, root.clone())?;
        let mut lessons = Vec::new();
        tree.handoffs(&mut lessons);
        let mut body = root.spec.body.trim_end().to_string();
        if !lessons.is_empty() {
            body.push_str(
                "

## Lessons from the attempts so far

",
            );
            for (task, title, handoff) in &lessons {
                body.push_str(&format!(
                    "- `{task}` {title}: handoff `{handoff}`
"
                ));
            }
        }
        let created = l.create(TaskSpec {
            domain: root.spec.domain.clone(),
            title: root.spec.title.clone(),
            body,
            requirements: root.spec.requirements.clone(),
            depends_on: Vec::new(),
            origin: Some(format!("retries:{}", root.id)),
            upstream_event: None,
            created_by: self.author.clone(),
            classification: root.spec.classification.max(self.label()),
            priority: root.spec.priority,
        })?;
        self.admit_own_in(&mut l, created.task)
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

    /// Close a task without doing it, and everything still open that was filed from it:
    /// its subagent tasks, their continuations, and theirs, since a promise withdrawn
    /// withdraws the promises made to keep it.
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
        let task = l.invalidate(id, reason, superseded_by)?;
        let mut pending = l.filed_from(id)?;
        while let Some(t) = pending.pop() {
            pending.extend(l.filed_from(&t.id)?);
            if !t.status.is_terminal() {
                l.invalidate(
                    &t.id,
                    &format!("filed from {id}, which was invalidated: {reason}"),
                    superseded_by,
                )?;
            }
        }
        Ok(task)
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
        let scope = bundle::export_scope(&self.grants.read_filter(), &domain, max_label)?;
        // the index is read and released before the record is, so the future is Send
        let hits = self.index.lock().await.list(&scope, None, usize::MAX / 2)?;
        Ok(bundle::export(
            &self.store,
            hits,
            &domain,
            max_label,
            &self.grants.owner,
            dest,
        )
        .await?)
    }

    /// Erase an entry: the body leaves every version in the record and the index, the
    /// stub, the erasure record and the receipt remain. Only the owner may, and only
    /// once. The receipt is written first, with the versions about to be removed and a
    /// line saying the erasure has not finished; then the record, the citing entries,
    /// the index, the served bundles; then the receipt again without that line. A run
    /// that stops short leaves that receipt, and the next run picks up where it stopped,
    /// keeping the first run's reason. Entries that cite the erased one are flagged for
    /// review today rather than re-derived here. Exports already handed out, the
    /// transcripts of sessions that read the entry, and the queries in the read log are
    /// beyond reach and the receipt says so.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for a session that is not the owner or an entry already
    /// erased, [`Error::NotFound`] for an entry in no granted domain, or a store, index
    /// or I/O error; an index another connection holds is [`dm_index::Error::Held`],
    /// and the erasure is finished by running it again.
    pub async fn erase(&self, id: &str, reason: Reason) -> Result<Receipt, Error> {
        if self.author != self.grants.owner {
            return Err(Error::Denied(format!(
                "only the owner {} may erase",
                self.grants.owner
            )));
        }
        let (domain, entry) = self.find(id).await?;
        let stubbed = entry.front.status == Some(Status::Erased);
        let mut receipt = match self.store.receipt(&domain, id).await? {
            Some(r) if r.is_finished() => {
                return Err(Error::Denied(format!("{id} is already erased")));
            }
            Some(unfinished) => unfinished,
            None => {
                let removed = if stubbed {
                    // a stub with no receipt: the versions it replaced are not known
                    Vec::new()
                } else {
                    self.store.digests(&domain, id).await?
                };
                let pending = Receipt {
                    id: id.to_string(),
                    domain: domain.clone(),
                    erased_at: OffsetDateTime::now_utc(),
                    erased_by: self.author.clone(),
                    reason,
                    removed,
                    flagged: Vec::new(),
                    unresolved: vec![erasure::UNFINISHED.into()],
                };
                self.store.put_receipt(&pending).await?;
                pending
            }
        };
        if !stubbed {
            let mut stub = erasure::stub(&entry, receipt.reason);
            stub.front.domain.clone_from(&domain);
            stub.front.id = id.to_string();
            self.store.erase(&domain, id, &stub).await?;
        }
        let stub = self
            .store
            .get(&domain, id)
            .await?
            .ok_or_else(|| Error::NotFound(id.to_string()))?;
        receipt.flagged = self.flag_citers(id).await?;
        {
            let mut ix = self.index.lock().await;
            ix.remove(id)?;
            ix.upsert(&stub)?;
            ix.vacuum()?;
        }
        self.forget_pushed(id);
        self.drop_bundles(&domain)?;
        // every session's render, before the receipt says the erasure is done; what it
        // could not reach is said on the receipt
        let missed = self.erase_from_projections(&domain, id);
        close_receipt(&mut receipt, missed);
        self.store.put_receipt(&receipt).await?;
        self.refresh_projection_after_erasure().await;
        Ok(receipt)
    }

    /// The receipt of an erasure, if the entry was erased in a granted domain.
    ///
    /// # Errors
    ///
    /// A store error.
    pub async fn receipt(&self, id: &str) -> Result<Option<Receipt>, Error> {
        for domain in self.domains() {
            if let Some(r) = self.store.receipt(&domain, id).await? {
                return Ok(Some(r));
            }
        }
        Ok(None)
    }

    /// An entry by id across the granted domains, from the record, whatever its status.
    async fn find(&self, id: &str) -> Result<(String, Entry), Error> {
        for domain in self.domains() {
            if let Some(e) = self.store.get(&domain, id).await? {
                return Ok((domain, e));
            }
        }
        Err(Error::NotFound(id.to_string()))
    }
}

/// What an erasure here cannot reach, named on every receipt so nobody believes it did.
/// What the receipt still owes once every step ran: the standing list, the versions it
/// could not name, and the renders the sweep missed. A missed render leaves the erasure
/// unfinished, so running it again sweeps again.
fn close_receipt(receipt: &mut Receipt, missed: Vec<String>) {
    receipt.unresolved = UNRESOLVED.iter().map(|u| (*u).to_string()).collect();
    if receipt.removed.is_empty() {
        receipt
            .unresolved
            .push("the versions removed were not recorded: the stub was there first".into());
    }
    if !missed.is_empty() {
        receipt.unresolved.push(erasure::UNFINISHED.into());
    }
    receipt.unresolved.extend(missed);
}

const UNRESOLVED: &[&str] = &[
    "exports issued before the erasure",
    "transcripts of sessions that read it",
    "search queries and recall questions in the read log",
    "the history of entries that cite it, and the ledger's handoffs, which are not scanned",
    "copies the digests do not recognise: a reworded one is a new text to an import",
];

/// Whether `text` names `id`: the whole id, not a prefix of a longer one, and not the
/// tail of one.
fn cites(text: &str, id: &str) -> bool {
    let id_char = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
    text.match_indices(id).any(|(i, _)| {
        let before = text[..i].chars().next_back().is_none_or(|c| !id_char(c));
        let after = text[i + id.len()..]
            .chars()
            .next()
            .is_none_or(|c| !id_char(c));
        before && after
    })
}

impl Memory {
    /// Flag every granted entry that names `id`, in its body, its sources or as the
    /// entry it supersedes: its review date becomes today, so curation re-derives it from
    /// what remains or erases it too. Returns the ids flagged.
    async fn flag_citers(&self, id: &str) -> Result<Vec<String>, Error> {
        let today = OffsetDateTime::now_utc().date();
        let mut flagged = Vec::new();
        for domain in self.domains() {
            for other in self.store.list(&domain).await? {
                if other == id {
                    continue;
                }
                let Some(mut e) = self.store.get(&domain, &other).await? else {
                    continue;
                };
                let names_it = cites(&e.body, id)
                    || e.front.source.iter().any(|s| cites(s, id))
                    || e.front.supersedes.as_deref() == Some(id);
                if !names_it || e.front.status == Some(Status::Erased) {
                    continue;
                }
                e.front.review_by = Some(today);
                self.store.put(&e).await?;
                self.index.lock().await.upsert(&e)?;
                flagged.push(other);
            }
        }
        Ok(flagged)
    }

    /// Remove the bundles served from this root for a domain, at every label, and the
    /// working copies an export in progress leaves beside them.
    fn drop_bundles(&self, domain: &str) -> Result<(), Error> {
        for label in [Label::Public, Label::Internal, Label::Restricted] {
            let dir = bundle::bundle_dir(&self.root, domain, label);
            let name = dir.file_name().unwrap_or_default().to_os_string();
            for suffix in ["", ".new", ".old"] {
                let mut n = name.clone();
                n.push(suffix);
                let path = dir.with_file_name(n);
                if path.is_dir() {
                    std::fs::remove_dir_all(&path).map_err(io(&path))?;
                }
            }
        }
        Ok(())
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
        let erased: HashSet<String> = self
            .store
            .receipts(&domain)
            .await?
            .into_iter()
            .flat_map(|r| {
                r.removed
                    .into_iter()
                    .flat_map(|v| std::iter::once(v.digest).chain(v.body_digest))
            })
            .collect();
        let imported = bundle::import(
            &self.store,
            &mut ix,
            &self.grants.owner,
            src,
            &domain,
            default,
            &erased,
        )
        .await?;
        drop(ix);
        self.refresh_projection().await;
        Ok(imported)
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
    async fn the_read_log_can_be_switched_off_behind_an_arc() {
        let dir = tempfile::tempdir().unwrap();
        let m = std::sync::Arc::new(mem(dir.path()));
        m.write(req("Operation R retries.\n")).await.unwrap();
        m.disable_read_log();
        m.search("retries", None, 10).await.unwrap();
        assert!(reads::all(dir.path()).unwrap().is_empty());
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
    async fn a_fan_out_files_children_and_a_continuation_that_waits_on_them() {
        let dir = tempfile::tempdir().unwrap();
        let m = mem(dir.path()).with_manifest(vec!["tool:fs".into()]);
        let parent = m
            .create_task(TaskSpec {
                requirements: vec!["tool:fs".into()],
                priority: 3,
                ..spec("Ship the import")
            })
            .await
            .unwrap()
            .task;
        m.admit(&parent.id).await.unwrap();
        let f = m
            .fan_out(
                &parent.id,
                &[
                    (
                        "Count crates".into(),
                        "List the crates and count them.".into(),
                    ),
                    ("Read the plan".into(), "Summarise PLAN.md.".into()),
                ],
            )
            .await
            .unwrap();
        assert_eq!(f.children.len(), 2);
        // the starter grants admit a session's own filings
        assert!(f.children.iter().all(|c| c.status == Status::Admitted));
        assert_eq!(
            f.children[0].spec.origin.as_deref(),
            Some(format!("task:{}", parent.id).as_str())
        );
        let continuation = f.continuation.clone().unwrap();
        assert_eq!(continuation.status, Status::Admitted);
        assert_eq!(continuation.spec.title, "Resume: Ship the import");
        assert_eq!(continuation.spec.requirements, vec!["tool:fs".to_string()]);
        assert_eq!(continuation.spec.priority, 3);
        assert_eq!(
            continuation.spec.depends_on,
            f.children.iter().map(|c| c.id.clone()).collect::<Vec<_>>()
        );
        // the same spawn asked twice is the same task, and so is the continuation
        let again = m
            .fan_out(
                &parent.id,
                &[
                    (
                        "Count crates".into(),
                        "List the crates and count them.".into(),
                    ),
                    ("Read the plan".into(), "Summarise PLAN.md.".into()),
                ],
            )
            .await
            .unwrap();
        assert!(again.children.is_empty(), "nothing new");
        assert_eq!(again.existing.len(), 2);
        assert_eq!(again.existing[0].id, f.children[0].id);
        assert!(
            again.continuation.is_none(),
            "no continuation for nothing new"
        );
        // the continuation is not claimable until the children are complete
        let claimed = m
            .claim_next(None, Duration::minutes(30))
            .await
            .unwrap()
            .unwrap();
        assert_ne!(
            claimed.task.id, continuation.id,
            "a child or the parent first"
        );
        // the tree shows it all under the parent
        let tree = m.task_tree(&parent.id).await.unwrap();
        assert_eq!(tree.children.len(), 3, "two children and the continuation");
        assert!(tree.children.iter().any(|c| c.task.id == continuation.id));
    }

    #[tokio::test]
    async fn a_resumed_run_asks_for_a_spawn_once_per_chain() {
        let dir = tempfile::tempdir().unwrap();
        let m = mem(dir.path());
        let parent = m.create_task(spec("Ship the import")).await.unwrap().task;
        let f = m
            .fan_out(
                &parent.id,
                &[(
                    "Count crates".into(),
                    "List the crates and count them.".into(),
                )],
            )
            .await
            .unwrap();
        let continuation = f.continuation.clone().unwrap();
        // the same spawn under the continuation gets the earlier child, not another:
        // the chain is one
        let resumed = m
            .fan_out(
                &continuation.id,
                &[(
                    "Count crates".into(),
                    "List the crates and count them.".into(),
                )],
            )
            .await
            .unwrap();
        assert!(resumed.children.is_empty());
        assert_eq!(resumed.existing[0].id, f.children[0].id);
        assert!(resumed.continuation.is_none());
        // something new under the continuation is filed, with its own continuation,
        // titled after the root rather than stacking "Resume:"
        let more = m
            .fan_out(&continuation.id, &[("Lint".into(), "Run clippy.".into())])
            .await
            .unwrap();
        assert_eq!(more.children.len(), 1);
        assert_eq!(
            more.continuation.unwrap().spec.title,
            "Resume: Ship the import"
        );
    }

    #[tokio::test]
    async fn invalidation_is_recursive_and_a_retry_carries_the_lessons() {
        let dir = tempfile::tempdir().unwrap();
        let m = mem(dir.path());
        let parent = m.create_task(spec("Root")).await.unwrap().task;
        let f = m
            .fan_out(&parent.id, &[("Child".into(), "Do the child.".into())])
            .await
            .unwrap();
        let child = &f.children[0];
        // the child completes with a handoff, which the retry will carry as a lesson
        m.claim(&child.id, Duration::minutes(30)).await.unwrap();
        let h = m
            .write(WriteRequest {
                entry_type: EntryType::Handoff,
                ..req("The child found the crates.\n")
            })
            .await
            .unwrap();
        m.complete(&child.id, &h.id).await.unwrap();
        // a grandchild under the continuation, still open
        let g = m
            .fan_out(
                &f.continuation.clone().unwrap().id,
                &[("Grandchild".into(), "Deeper.".into())],
            )
            .await
            .unwrap();
        m.invalidate(&parent.id, "obsolete", None).await.unwrap();
        assert_eq!(
            m.get_task(&f.continuation.unwrap().id)
                .await
                .unwrap()
                .status,
            Status::Invalidated
        );
        assert_eq!(
            m.get_task(&g.children[0].id).await.unwrap().status,
            Status::Invalidated,
            "the grandchild goes with the tree"
        );
        assert_eq!(
            m.get_task(&child.id).await.unwrap().status,
            Status::Completed,
            "what was done stays done"
        );
        let retry = m.retry_task(&parent.id).await.unwrap();
        assert_eq!(retry.spec.title, "Root");
        assert_eq!(
            retry.spec.origin.as_deref(),
            Some(format!("retries:{}", parent.id).as_str())
        );
        assert!(
            retry.spec.body.contains("## Lessons"),
            "{}",
            retry.spec.body
        );
        assert!(retry.spec.body.contains(&h.id), "{}", retry.spec.body);
        assert_eq!(retry.status, Status::Admitted);
        assert_eq!(m.task_tree(&parent.id).await.unwrap().children.len(), 3);
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

    /// Every file under a directory, read as bytes.
    fn files_under(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for f in std::fs::read_dir(&d).unwrap().flatten() {
                if f.path().is_dir() {
                    stack.push(f.path());
                } else {
                    out.push((f.path(), std::fs::read(f.path()).unwrap()));
                }
            }
        }
        out
    }

    const MARKER: &str = "passphrase-Qz7xx";

    /// A root with a secret entry, an entry citing it in its body, an unrelated one, one
    /// citing it as a source, one superseding it, and a served bundle holding a copy;
    /// the ids in that order, and the bundle's directory. The read log stays on.
    async fn vault(dir: &Path) -> (Memory, [String; 5], PathBuf) {
        let m = mem(dir);
        let a = m
            .write(req(&format!("The vault opens with {MARKER}.\n")))
            .await
            .unwrap();
        let b = m
            .write(req(&format!("See {} for how the vault opens.\n", a.id)))
            .await
            .unwrap();
        let c = m.write(req("Unrelated, and stays so.\n")).await.unwrap();
        let d = m
            .write(WriteRequest {
                source: vec![a.id.clone()],
                ..req("Distilled from the vault entry.\n")
            })
            .await
            .unwrap();
        let e = m
            .write(WriteRequest {
                supersedes: Some(a.id.clone()),
                ..req("The vault now opens otherwise.\n")
            })
            .await
            .unwrap();
        let bundle = bundle::bundle_dir(dir, "dom-me", Label::Internal);
        m.export(Some("dom-me"), Label::Internal, &bundle)
            .await
            .unwrap();
        assert!(bundle.join(format!("{}.md", a.id)).is_file());
        (m, [a.id, b.id, c.id, d.id, e.id], bundle)
    }

    #[tokio::test]
    async fn only_the_owner_erases_and_only_an_entry_that_exists() {
        let dir = tempfile::tempdir().unwrap();
        let (m, [a, ..], _) = vault(dir.path()).await;
        let agent = Memory::open(dir.path()).unwrap().with_author("agent:x");
        assert!(
            matches!(
                agent.erase(&a, Reason::Requested).await,
                Err(Error::Denied(_))
            ),
            "only the owner erases"
        );
        assert!(matches!(
            m.erase("mem-me-nothere", Reason::Requested).await,
            Err(Error::NotFound(_))
        ));
        let kept = m.store.get("dom-me", &a).await.unwrap().unwrap();
        assert!(
            kept.body.contains(MARKER),
            "a refused erasure changes nothing"
        );
    }

    #[tokio::test]
    async fn erasure_leaves_a_stub_a_receipt_and_the_body_nowhere_under_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let (m, ids, bundle) = vault(dir.path()).await;
        let marker = MARKER;
        let [a, b, c, d, e] = ids;
        let r = m.erase(&a, Reason::Requested).await.unwrap();
        assert_eq!((r.id.as_str(), r.domain.as_str()), (a.as_str(), "dom-me"));
        assert_eq!(r.reason, Reason::Requested);
        assert_eq!(r.erased_by, "human:test:me");
        assert_eq!(
            r.removed.len(),
            2,
            "the current version and the one superseding archived"
        );
        assert_eq!(r.removed[0].version, "current");
        let mut flagged = r.flagged.clone();
        flagged.sort();
        let mut citing = vec![b.clone(), d.clone(), e.clone()];
        citing.sort();
        assert_eq!(flagged, citing, "body, source and supersedes all cite");
        assert!(r.unresolved.iter().any(|u| u.contains("exports")));

        assert!(
            matches!(m.get(&a).await, Err(Error::NotFound(_))),
            "the stub is not an active entry"
        );
        let hits = m.search("vault", None, 10).await.unwrap();
        assert!(
            hits.iter()
                .all(|h| h.id != a && !h.summary.contains(marker)),
            "{hits:?}"
        );
        assert!(m
            .search("vault", None, 10)
            .await
            .unwrap()
            .iter()
            .all(|h| h.id != a));
        let flagged = m.get(&b).await.unwrap();
        assert_eq!(
            flagged.front.review_by,
            Some(OffsetDateTime::now_utc().date()),
            "review today"
        );
        assert!(m.get(&c).await.unwrap().front.review_by.is_none());
        assert!(!bundle.exists(), "the served bundle held a copy");
        let kept = m.receipt(&a).await.unwrap().unwrap();
        assert_eq!(
            (kept.id, kept.reason, kept.removed),
            (r.id, r.reason, r.removed)
        );
        assert!(m.receipt(&b).await.unwrap().is_none());
        assert!(
            matches!(m.erase(&a, Reason::Legal).await, Err(Error::Denied(_))),
            "an entry is erased once"
        );
        assert_eq!(
            m.receipt(&a).await.unwrap().unwrap().reason,
            Reason::Requested,
            "the first receipt stands"
        );
        let stub =
            std::fs::read_to_string(dir.path().join(format!("record/dom-me/{}.md", a))).unwrap();
        assert!(
            stub.contains("status: erased") && stub.contains("author: erased"),
            "{stub}"
        );
        drop(m);
        for (path, bytes) in files_under(dir.path()) {
            assert!(
                !bytes.windows(marker.len()).any(|w| w == marker.as_bytes()),
                "{} still holds the erased body",
                path.display()
            );
        }
    }

    #[tokio::test]
    async fn the_erasure_sweep_reaches_old_renders_and_half_written_copies_only() {
        let dir = tempfile::tempdir().unwrap();
        let (m, ids, _bundle) = vault(dir.path()).await;
        let [_a, _b, c, _d, _e] = ids;
        let titled = m
            .write(WriteRequest {
                title: Some("Alice diagnosis\n- [Alice has condition X".into()),
                ..req("Private.\n")
            })
            .await
            .unwrap()
            .id;
        let citing = m
            .write(req(&format!("See [the old note]({c}.md) for that.\n")))
            .await
            .unwrap()
            .id;
        let base = dir.path().join(projection::PROJECTIONS);
        // a render from before renders were per session, at <harness>/ itself
        let old = base.join("gemini");
        m.project_into(&old).await.unwrap();
        // as main rendered a title with a newline in it: over two lines
        let page = std::fs::read_to_string(old.join("dom-me/README.md")).unwrap();
        let split = page.replace("Alice diagnosis -  Alice", "Alice diagnosis\n-  Alice");
        assert_ne!(page, split, "{page}");
        std::fs::write(old.join("dom-me/README.md"), split).unwrap();
        let page = old.join(projection::RECALLS).join("sess-old");
        std::fs::create_dir_all(&page).unwrap();
        std::fs::write(page.join("p.md"), format!("served: {MARKER}\n")).unwrap();
        // and a session's, with a body a kill left half written
        let live = base.join("claude/sess-live");
        m.project_into(&live).await.unwrap();
        std::fs::write(
            live.join(format!("dom-me/{c}.md.tmp")),
            "Unrelated, half.\n",
        )
        .unwrap();
        // a render killed while it wrote its summaries: the page half written, none whole
        let killed = base.join("claude/sess-killed/dom-me");
        std::fs::create_dir_all(&killed).unwrap();
        std::fs::write(killed.join("README.md.tmp"), "- [Alice diagnosis, half\n").unwrap();
        m.erase(&c, Reason::Requested).await.unwrap();
        m.erase(&titled, Reason::Requested).await.unwrap();
        assert!(
            !killed.join("README.md.tmp").exists(),
            "a half-written page goes"
        );
        for p in [&old, &live] {
            assert!(
                !p.join(format!("dom-me/{c}.md")).exists(),
                "{}",
                p.display()
            );
            assert!(
                !p.join(format!("dom-me/{c}.md.tmp")).exists(),
                "{}",
                p.display()
            );
            assert!(
                !p.join(projection::RECALLS).exists()
                    || std::fs::read_dir(p.join(projection::RECALLS))
                        .unwrap()
                        .next()
                        .is_none()
            );
            let summaries = std::fs::read_to_string(p.join("dom-me/README.md")).unwrap();
            assert!(
                !summaries.contains("Alice"),
                "a title is about the person too: {summaries}"
            );
            assert!(
                summaries.contains(&format!("]({citing}.md)")),
                "a line that only cites the erased file is another entry's: {summaries}"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_erasure_sweep_follows_no_link_and_says_what_it_missed() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let (m, ids, _bundle) = vault(dir.path()).await;
        let [_a, b, c, d, e] = ids;
        // state/projection a link out of the root, onto something that looks like one
        let outside = tempfile::tempdir().unwrap();
        let precious = outside.path().join("proj/work/recall/precious");
        std::fs::create_dir_all(&precious).unwrap();
        std::fs::write(precious.join("thesis.txt"), "years of work").unwrap();
        let base = dir.path().join(projection::PROJECTIONS);
        std::os::unix::fs::symlink(outside.path(), &base).unwrap();
        let r = m.erase(&b, Reason::Requested).await.unwrap();
        assert!(
            precious.join("thesis.txt").exists(),
            "the sweep went through the link"
        );
        assert!(
            r.unresolved
                .iter()
                .any(|u| u.contains("not a directory of the root's own")),
            "{:?}",
            r.unresolved
        );
        // a render it cannot clean is on the receipt, not only on standard error
        std::fs::remove_file(&base).unwrap();
        let locked = base.join("claude/sess-locked");
        m.project_into(&locked).await.unwrap();
        let dom = locked.join("dom-me");
        std::fs::set_permissions(&dom, std::fs::Permissions::from_mode(0o555)).unwrap();
        let r = m.erase(&c, Reason::Requested).await.unwrap();
        std::fs::set_permissions(&dom, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            r.unresolved.iter().any(|u| u.contains("sess-locked")),
            "{:?}",
            r.unresolved
        );
        assert!(
            !r.is_finished(),
            "what the sweep missed, a run again sweeps"
        );
        let again = m.erase(&c, Reason::Requested).await.unwrap();
        assert!(again.is_finished(), "{:?}", again.unresolved);
        assert!(!dom.join(format!("{c}.md")).exists());
        // a summaries page it cannot read
        let readme = dom.join("README.md");
        std::fs::set_permissions(&readme, std::fs::Permissions::from_mode(0o000)).unwrap();
        let r = m.erase(&d, Reason::Requested).await.unwrap();
        std::fs::set_permissions(&readme, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            r.unresolved.iter().any(|u| u.contains("README.md")),
            "{:?}",
            r.unresolved
        );
        // and the projections it cannot list at all
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o000)).unwrap();
        let r = m.erase(&e, Reason::Requested).await.unwrap();
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            r.unresolved
                .iter()
                .any(|u| u.contains("the projections under")),
            "{:?}",
            r.unresolved
        );
        // and state itself a link out of the root
        let dir = tempfile::tempdir().unwrap();
        let (m, ids, _bundle) = vault(dir.path()).await;
        let away = outside.path().join("state-away");
        std::fs::rename(dir.path().join("state"), &away).unwrap();
        std::os::unix::fs::symlink(&away, dir.path().join("state")).unwrap();
        let kept = away.join("projection/proj/work/recall/precious");
        std::fs::create_dir_all(&kept).unwrap();
        std::fs::write(kept.join("thesis.txt"), "years of work").unwrap();
        let r = m.erase(&ids[1], Reason::Requested).await.unwrap();
        assert!(
            kept.join("thesis.txt").exists(),
            "the sweep went through state"
        );
        assert!(
            r.unresolved
                .iter()
                .any(|u| u.contains("not a directory of the root's own")),
            "{:?}",
            r.unresolved
        );
    }

    #[tokio::test]
    async fn erasure_reaches_every_session_s_projection_not_only_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let (m, ids, _bundle) = vault(dir.path()).await;
        let [a, b, c, _d, _e] = ids;
        // two sessions' renders under the root, a live one and one a kill left behind,
        // and a recall page one of them landed
        let base = dir.path().join(projection::PROJECTIONS);
        let peers = [base.join("claude/sess-live"), base.join("codex/run-41")];
        for p in &peers {
            m.project_into(p).await.unwrap();
        }
        let page = peers[0].join(projection::RECALLS).join("sess-live");
        std::fs::create_dir_all(&page).unwrap();
        std::fs::write(page.join("p.md"), format!("served: {MARKER}\n")).unwrap();
        // the CLI's memory erases, and renders no projection of its own: the vault entry,
        // superseded and so never rendered but quoted on a recall page, and one listed
        m.erase(&a, Reason::Requested).await.unwrap();
        m.erase(&c, Reason::Requested).await.unwrap();
        drop(m);
        for (path, bytes) in files_under(dir.path()) {
            assert!(
                !bytes.windows(MARKER.len()).any(|w| w == MARKER.as_bytes()),
                "{} still holds the erased body",
                path.display()
            );
        }
        for p in &peers {
            let summaries = std::fs::read_to_string(p.join("dom-me/README.md")).unwrap();
            assert!(
                !p.join(format!("dom-me/{c}.md")).exists(),
                "{}",
                p.display()
            );
            assert!(!summaries.contains(&format!("]({c}.md)")), "{summaries}");
            assert!(
                summaries.contains(&format!("]({b}.md)"))
                    && p.join(format!("dom-me/{b}.md")).exists(),
                "what was not erased stays: {summaries}"
            );
            let listed = summaries.lines().filter(|l| l.starts_with("- [")).count();
            assert!(
                summaries.starts_with(&format!("# dom-me: {listed} entries\n")),
                "{summaries}"
            );
        }
    }

    #[test]
    fn a_citation_is_the_whole_id_not_a_prefix_of_a_longer_one() {
        assert!(cites("See mem-me-e52gu4j6.", "mem-me-e52gu4j6"));
        assert!(cites("(mem-me-e52gu4j6)", "mem-me-e52gu4j6"));
        assert!(cites("mem-me-e52gu4j6", "mem-me-e52gu4j6"));
        assert!(!cites("See mem-me-e52gu4j6zz.", "mem-me-e52gu4j6"));
        assert!(!cites("xmem-me-e52gu4j6", "mem-me-e52gu4j6"));
        assert!(!cites("nothing here", "mem-me-e52gu4j6"));
    }

    #[tokio::test]
    async fn an_older_bundle_cannot_bring_an_erased_body_back() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        let (m, [a, ..], _) = vault(&root).await;
        let handed_out = dir.path().join("handed-out");
        m.export(Some("dom-me"), Label::Internal, &handed_out)
            .await
            .unwrap();
        m.erase(&a, Reason::Requested).await.unwrap();
        let im = m.import(&handed_out, Some("dom-me")).await.unwrap();
        assert_eq!(im.erased, vec![a.clone()], "{im:?}");
        assert!(!im.written.contains(&a));
        let hits = m.search("vault", None, 10).await.unwrap();
        assert!(hits.iter().all(|h| !h.summary.contains(MARKER)), "{hits:?}");
        drop(m);
        for (path, bytes) in files_under(&root) {
            assert!(
                !bytes.windows(MARKER.len()).any(|w| w == MARKER.as_bytes()),
                "{} holds the erased body after the import",
                path.display()
            );
        }
    }

    #[tokio::test]
    async fn the_record_s_status_wins_over_the_index_s_row() {
        let dir = tempfile::tempdir().unwrap();
        let m = mem(dir.path());
        let w = m.write(req("Still indexed as active.\n")).await.unwrap();
        // the record moves on without the index: another process demoted it
        m.store.demote("dom-me", &w.id).await.unwrap();
        assert!(
            matches!(m.get(&w.id).await, Err(Error::NotFound(_))),
            "an inactive record is not served because the index is behind"
        );
    }

    #[tokio::test]
    async fn an_erasure_refused_before_the_record_step_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (m, [a, ..], _) = vault(dir.path()).await;
        // the receipt cannot be written: the erasures directory is a file
        std::fs::write(dir.path().join("record/dom-me/.erasures"), "x").unwrap();
        assert!(m.erase(&a, Reason::Legal).await.is_err());
        let still = m.store.get("dom-me", &a).await.unwrap().unwrap();
        assert!(still.body.contains(MARKER), "nothing was removed");
        assert!(
            !matches!(m.receipt(&a).await, Ok(Some(_))),
            "no receipt was written"
        );
        std::fs::remove_file(dir.path().join("record/dom-me/.erasures")).unwrap();
        let r = m.erase(&a, Reason::Legal).await.unwrap();
        assert!(r.is_finished(), "{r:?}");
    }

    #[tokio::test]
    async fn an_erasure_that_stops_short_finishes_on_the_next_run() {
        let dir = tempfile::tempdir().unwrap();
        let (m, [a, ..], bundle) = vault(dir.path()).await;
        // another process is mid-query on the index, so its log cannot be checkpointed
        let reader = rusqlite::Connection::open(dir.path().join(INDEX_FILE)).unwrap();
        let mut stmt = reader.prepare("SELECT id FROM entries").unwrap();
        let mut rows = stmt.query([]).unwrap();
        let _first = rows.next().unwrap();
        let first = m.erase(&a, Reason::Sensitive).await;
        assert!(
            matches!(first, Err(Error::Index(dm_index::Error::Held))),
            "{first:?}"
        );
        drop(rows);
        drop(stmt);
        drop(reader);
        assert!(
            matches!(m.get(&a).await, Err(Error::NotFound(_))),
            "the stub is not served"
        );
        let pending = m.receipt(&a).await.unwrap().unwrap();
        assert!(!pending.is_finished(), "{pending:?}");
        assert_eq!(pending.removed.len(), 2);
        assert!(bundle.exists(), "the bundle step had not run");
        let again = m.erase(&a, Reason::Legal).await.unwrap();
        assert!(again.is_finished(), "{again:?}");
        assert_eq!(
            again.reason,
            Reason::Sensitive,
            "the first run's reason stands"
        );
        assert_eq!(again.removed, pending.removed);
        assert!(!bundle.exists());
        assert!(matches!(
            m.erase(&a, Reason::Legal).await,
            Err(Error::Denied(_))
        ));
        drop(m);
        for (path, bytes) in files_under(dir.path()) {
            assert!(
                !bytes.windows(MARKER.len()).any(|w| w == MARKER.as_bytes()),
                "{} still holds the erased body",
                path.display()
            );
        }
    }

    /// A second domain in the grants file, so a root holds two.
    fn grant_domain(root: &Path, domain: &str) {
        let grants = root.join(GRANTS_FILE);
        let mut text = std::fs::read_to_string(&grants).unwrap();
        text.push_str(&format!(
            "\n[domains.{domain}]\ndefault_classification = \"internal\"\n"
        ));
        std::fs::write(&grants, text).unwrap();
    }

    #[tokio::test]
    async fn an_edited_copy_of_an_erased_body_is_refused_too() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        let (m, [a, ..], _) = vault(&root).await;
        let handed_out = dir.path().join("handed-out");
        m.export(Some("dom-me"), Label::Internal, &handed_out)
            .await
            .unwrap();
        m.erase(&a, Reason::Requested).await.unwrap();
        // one byte appended and the manifest regenerated: the text's digest is new
        let file = handed_out.join(format!("{a}.md"));
        let edited = format!("{}\n", std::fs::read_to_string(&file).unwrap());
        std::fs::write(&file, &edited).unwrap();
        let mpath = handed_out.join("manifest.yaml");
        let mut manifest =
            dm_core::manifest::Manifest::from_yaml(&std::fs::read_to_string(&mpath).unwrap())
                .unwrap();
        manifest
            .entries
            .iter_mut()
            .find(|e| e.id == a)
            .unwrap()
            .digest = dm_core::digest(&edited);
        std::fs::write(&mpath, manifest.to_yaml()).unwrap();
        let im = m.import(&handed_out, Some("dom-me")).await.unwrap();
        assert_eq!(im.erased, vec![a.clone()], "{im:?}");
        drop(m);
        for (path, bytes) in files_under(&root) {
            assert!(
                !bytes.windows(MARKER.len()).any(|w| w == MARKER.as_bytes()),
                "{} holds the erased body after the import",
                path.display()
            );
        }
    }

    #[tokio::test]
    async fn the_same_bundle_lands_in_each_domain_that_imports_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let m = mem(&root);
        m.write(req("Shared knowledge.\n")).await.unwrap();
        let out = dir.path().join("out");
        m.export(Some("dom-me"), Label::Internal, &out)
            .await
            .unwrap();
        grant_domain(&root, "dom-x");
        grant_domain(&root, "dom-y");
        let m = Memory::open(&root).unwrap();
        let x = m.import(&out, Some("dom-x")).await.unwrap();
        let y = m.import(&out, Some("dom-y")).await.unwrap();
        assert_eq!((x.written.len(), x.skipped.len()), (1, 0), "{x:?}");
        assert_eq!(
            (y.written.len(), y.skipped.len()),
            (1, 0),
            "the other domain's copy is not this domain's duplicate: {y:?}"
        );
        let again = m.import(&out, Some("dom-y")).await.unwrap();
        assert_eq!((again.written.len(), again.skipped.len()), (0, 1));
    }
}
