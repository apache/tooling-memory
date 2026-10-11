//! The event log: one append-only log per domain, the order of record of what happened
//! to the domain's entries (the paper's log, of which the record, the index and the
//! ledger are projections).
//!
//! Every write of an entry, every version, demotion, promotion and erasure, goes into
//! its domain's log before the record takes it, and one materializer per domain builds
//! the index from the log and the record: what the log says is live, at the version the
//! log names, is what the index holds. The log carries ids and digests, never bodies:
//! bodies stay in the record's versions, so an erasure removes a body from the record
//! and the log, which never held it, needs no rewriting. The ledger is a log already,
//! its events table; the materializer replays it into its projection beside the index.
//!
//! On disk, under the root: `state/log/<domain>.jsonl`, a line per event, and
//! `state/log/<domain>.seq`, the last sequence number given. One writer per domain:
//! an append holds an exclusive lock on the domain's lock file, across processes.
//!
//! Production: the same events on a partitioned log (Kafka in the reference stack), a
//! partition per domain, and the materializer a consumer per partition.

use crate::{Error, Memory};
use dm_core::Entry;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Where the logs are, under the root.
pub const DIR: &str = "state/log";

/// What happened.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Change {
    /// A version of an entry was written: its digest, and whether it is active.
    Put {
        /// The entry.
        id: String,
        /// The version's digest.
        digest: String,
        /// Whether the version is active, rather than superseded or demoted.
        active: bool,
    },
    /// An entry was erased.
    Erased {
        /// The entry.
        id: String,
    },
}

/// One event in a domain's log.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Event {
    /// Its place in the domain's log, from one.
    pub seq: u64,
    /// When.
    pub at: String,
    /// Who: the session's author.
    pub by: String,
    /// What.
    #[serde(flatten)]
    pub change: Change,
}

/// What a materialization did.
#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub struct Materialized {
    /// The domain.
    pub domain: String,
    /// Events read.
    pub events: u64,
    /// Entries the index holds as live.
    pub live: usize,
    /// Entries the log names whose record is not at the version the log names: the
    /// record behind the log, or ahead of it.
    pub behind: Vec<String>,
    /// Ledger events replayed into its projection.
    pub ledger_events: u64,
}

fn valid_domain(domain: &str) -> Result<&str, Error> {
    if !domain.is_empty()
        && domain
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        Ok(domain)
    } else {
        Err(Error::Denied(format!("{domain:?} is not a domain")))
    }
}

fn paths(root: &Path, domain: &str) -> Result<(PathBuf, PathBuf, PathBuf), Error> {
    let d = valid_domain(domain)?;
    let dir = root.join(DIR);
    Ok((
        dir.join(format!("{d}.jsonl")),
        dir.join(format!("{d}.seq")),
        dir.join(format!("{d}.lock")),
    ))
}

fn now() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// Append one change to `domain`'s log, as its one writer: the lock is held from reading
/// the last sequence number to writing the next. Returns the event.
///
/// # Errors
///
/// A domain that is not a name, or the file system.
pub fn append(root: &Path, domain: &str, by: &str, change: Change) -> Result<Event, Error> {
    let (log, seq_file, lock_file) = paths(root, domain)?;
    let dir = root.join(DIR);
    std::fs::create_dir_all(&dir).map_err(crate::io(&dir))?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_file)
        .map_err(crate::io(&lock_file))?;
    lock.lock().map_err(crate::io(&lock_file))?;
    let last: u64 = std::fs::read_to_string(&seq_file)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    let event = Event {
        seq: last + 1,
        at: now(),
        by: by.to_string(),
        change,
    };
    let line = serde_json::to_string(&event).map_err(|e| Error::Transcript(format!("log: {e}")))?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .map_err(crate::io(&log))?;
    f.write_all(format!("{line}\n").as_bytes())
        .and_then(|()| f.sync_data())
        .map_err(crate::io(&log))?;
    std::fs::write(&seq_file, event.seq.to_string()).map_err(crate::io(&seq_file))?;
    drop(lock);
    Ok(event)
}

/// A domain's log from `from` on, in order. A line that does not parse is skipped: a
/// write cut short by a crash leaves at most the last line torn.
///
/// # Errors
///
/// A domain that is not a name, or the file system.
pub fn read(root: &Path, domain: &str, from: u64) -> Result<Vec<Event>, Error> {
    let (log, _, _) = paths(root, domain)?;
    let text = match std::fs::read_to_string(&log) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(crate::io(&log)(e)),
    };
    Ok(text
        .lines()
        .filter_map(|l| serde_json::from_str::<Event>(l).ok())
        .filter(|e| e.seq >= from)
        .collect())
}

/// What the log says each entry is now: the version last put, unless erased since.
#[must_use]
pub fn state(events: &[Event]) -> BTreeMap<String, Option<(String, bool)>> {
    let mut out = BTreeMap::new();
    for e in events {
        match &e.change {
            Change::Put { id, digest, active } => {
                out.insert(id.clone(), Some((digest.clone(), *active)));
            }
            Change::Erased { id } => {
                out.insert(id.clone(), None);
            }
        }
    }
    out
}

impl Memory {
    /// Write a version of an entry through its domain's log: the change is appended
    /// first, then the record takes it. Every record write of an entry comes here.
    pub(crate) async fn put_logged(&self, entry: &Entry) -> Result<(), Error> {
        append(
            &self.root,
            &entry.front.domain,
            &self.author,
            Change::Put {
                id: entry.front.id.clone(),
                digest: dm_core::digest(&entry.render()),
                active: entry.is_active(),
            },
        )?;
        self.store.put(entry).await?;
        // the record marks the entry this one replaces as superseded as it takes it: that
        // version goes in the log too, so the log says what the record holds
        if let Some(older) = &entry.front.supersedes {
            if let Some(old) = self.store.get(&entry.front.domain, older).await? {
                append(
                    &self.root,
                    &entry.front.domain,
                    &self.author,
                    Change::Put {
                        id: old.front.id.clone(),
                        digest: dm_core::digest(&old.render()),
                        active: old.is_active(),
                    },
                )?;
            }
        }
        Ok(())
    }

    /// Say in its domain's log that an entry was erased; before the record erases it.
    pub(crate) fn log_erased(&self, domain: &str, id: &str) -> Result<(), Error> {
        append(
            &self.root,
            domain,
            &self.author,
            Change::Erased { id: id.to_string() },
        )?;
        Ok(())
    }

    /// A domain's log, the owner's: what happened to its entries, in order.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for anyone but the owner or an ungranted domain; the file
    /// system.
    pub fn log(&self, domain: Option<&str>, from: u64) -> Result<Vec<Event>, Error> {
        if self.author != self.grants.owner {
            return Err(Error::Denied("the log is the owner's".into()));
        }
        let domain = self.resolve_domain(domain)?;
        read(&self.root, &domain, from)
    }

    /// The materializer: build the domain's index from its log and the record, and
    /// replay the ledger's events into its projection. What the log says is live is
    /// indexed at the version the record holds; an entry the record holds at another
    /// version than the log names is reported behind; what the log says is gone leaves
    /// the index.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for anyone but the owner, a store, index or ledger error.
    pub async fn materialize(&self, domain: Option<&str>) -> Result<Materialized, Error> {
        if self.author != self.grants.owner {
            return Err(Error::Denied("materializing is the owner's".into()));
        }
        let domain = self.resolve_domain(domain)?;
        let events = read(&self.root, &domain, 0)?;
        let mut out = Materialized {
            domain: domain.clone(),
            events: events.len() as u64,
            ..Materialized::default()
        };
        let named = state(&events);
        let mut entries = Vec::new();
        for (id, now) in named.clone() {
            let held = self.store.get(&domain, &id).await?;
            match (now, held) {
                (Some((digest, _)), Some(e)) => {
                    if dm_core::digest(&e.render()) != digest {
                        out.behind.push(id.clone());
                    }
                    if e.is_active() {
                        out.live += 1;
                    }
                    entries.push(e);
                }
                (Some(_), None) => out.behind.push(id),
                (None, _) => {}
            }
        }
        // what the record holds and the log never named, written before the log was
        // kept, is indexed as the record has it: the log starts where it starts
        for id in self.store.list(&domain).await? {
            if !named.contains_key(&id) {
                if let Some(e) = self.store.get(&domain, &id).await? {
                    if e.is_active() {
                        out.live += 1;
                    }
                    entries.push(e);
                }
            }
        }
        self.index.lock().await.rebuild_domain(&domain, entries)?;
        out.ledger_events = self.ledger.lock().await.rebuild().await?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WriteRequest;

    fn req(body: &str) -> WriteRequest {
        WriteRequest {
            body: body.into(),
            ..WriteRequest::default()
        }
    }

    #[test]
    fn one_writer_numbers_the_log_and_a_torn_line_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let put = |id: &str| Change::Put {
            id: id.into(),
            digest: "d".into(),
            active: true,
        };
        assert_eq!(append(dir.path(), "dom-a", "me", put("a")).unwrap().seq, 1);
        assert_eq!(append(dir.path(), "dom-a", "me", put("b")).unwrap().seq, 2);
        assert_eq!(
            append(dir.path(), "dom-b", "me", put("c")).unwrap().seq,
            1,
            "per domain"
        );
        let log = dir.path().join(DIR).join("dom-a.jsonl");
        let mut text = std::fs::read_to_string(&log).unwrap();
        text.push_str("{\"seq\": 3, \"at\"");
        std::fs::write(&log, text).unwrap();
        let read = read(dir.path(), "dom-a", 0).unwrap();
        assert_eq!(read.iter().map(|e| e.seq).collect::<Vec<_>>(), [1, 2]);
        assert_eq!(super::read(dir.path(), "dom-a", 2).unwrap().len(), 1);
        assert!(append(dir.path(), "../x", "me", put("a")).is_err());
    }

    #[tokio::test]
    async fn every_write_goes_through_the_log_and_the_materializer_rebuilds_the_index() {
        let dir = tempfile::tempdir().unwrap();
        let m = Memory::init(dir.path(), "human:test:me", "dom-me").unwrap();
        let a = m.write(req("# Import\n\nFive steps.\n")).await.unwrap().id;
        let b = m
            .write(WriteRequest {
                supersedes: Some(a.clone()),
                ..req("# Import\n\nSix steps.\n")
            })
            .await
            .unwrap()
            .id;
        let gone = m
            .write(req("# Key\n\nThe deploy key is on the wiki page.\n"))
            .await
            .unwrap()
            .id;
        m.erase(&gone, dm_core::erasure::Reason::Mistaken)
            .await
            .unwrap();
        let log = m.log(None, 0).unwrap();
        let kinds: Vec<(String, bool)> = log
            .iter()
            .map(|e| match &e.change {
                Change::Put { id, active, .. } => (id.clone(), *active),
                Change::Erased { id } => (format!("erased {id}"), false),
            })
            .collect();
        assert_eq!(
            kinds,
            [
                (a.clone(), true),
                (b.clone(), true),
                (a.clone(), false),
                (gone.clone(), true),
                (format!("erased {gone}"), false)
            ]
        );
        assert!(
            log.iter()
                .all(|e| !serde_json::to_string(e).unwrap().contains("steps")),
            "no bodies"
        );
        // the index lost, and a record written before the log was kept
        m.index.lock().await.rebuild(Vec::new()).unwrap();
        let early = Entry::parse(
            "---\nid: mem-me-early001\ntype: fact\ndomain: dom-me\nauthor: human:test:me\n\
             created: 2026-10-01T09:00:00Z\ntrust_tier: raw\nclassification: internal\n---\n\
             # Early\n\nBefore the log.\n",
        )
        .unwrap();
        m.store.put(&early).await.unwrap();
        assert!(m.search("steps", None, 10).await.unwrap().is_empty());
        let done = m.materialize(None).await.unwrap();
        assert_eq!((done.events, done.live), (5, 2), "{done:?}");
        assert!(done.behind.is_empty(), "{done:?}");
        let hits = m.search("steps", None, 10).await.unwrap();
        assert_eq!(
            hits.iter().map(|h| h.id.clone()).collect::<Vec<_>>(),
            vec![b.clone()]
        );
        assert_eq!(m.search("Before", None, 10).await.unwrap().len(), 1);
        assert!(
            m.search("wiki", None, 10).await.unwrap().is_empty(),
            "erased stays out"
        );
        // a record changed behind the log's back is reported
        let path = dir.path().join(format!("record/dom-me/{b}.md"));
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, text.replace("Six", "Seven")).unwrap();
        assert_eq!(m.materialize(None).await.unwrap().behind, [b]);
        let agent = Memory::open(dir.path()).unwrap().with_author("agent:test");
        assert!(agent.log(None, 0).is_err() && agent.materialize(None).await.is_err());
    }
}
