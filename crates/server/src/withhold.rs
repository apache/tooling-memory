//! Withholding: an erasure follows lineage (the paper's rule that affected entries are
//! withheld from retrieval while they are derived again or erased in turn).
//!
//! An entry is affected when it names the erased one, in its body, its citations or as
//! the entry it replaces, or when a session wrote it after that session read the erased
//! one. That is one step of lineage forward ([`crate::lineage`]). Such an entry gets
//! `status: withheld` in the record, through the log and the feed as every version
//! goes: its body stays, and search, list, T0, recall, `get` and a bundle do not serve
//! it. The security role reads it from the archive.
//!
//! The erasure withholds what the log holds when it runs. A session that read the erased
//! entry can write after that, from the same context. So each write looks at the log
//! once the record took it: when its session read an entry before the entry's erasure,
//! or a turn of a transcript erased since, the new entry is withheld too, and the
//! receipt of that erasure names it.
//!
//! One kind of entry is listed and not withheld: a verified rule of the policy. A rule
//! refuses, so to withhold it would allow what it refused, and an agent could cause that
//! with an entry that the owner must erase. The rule stays in force and on the receipt,
//! for the owner to erase or to replace. An exception permits, so it is withheld as
//! every other entry is, and it leaves force.
//!
//! A withheld entry ends in one of three ways. It is derived again: a new entry names
//! it in `supersedes`, the record keeps the withheld one as it is, and readers go to
//! the new one. The owner releases it ([`Memory::release_withheld`]). Or it is erased
//! in turn.
//!
//! Only one step is withheld. The edge from a read to a later write is coarse: a
//! session that loaded T0 read everything T0 listed, so the whole closure would hide
//! most of a busy domain. Entries further downstream are listed on the receipt, and so
//! is every send in the lineage, at any depth.

use crate::lineage::{self, Direction};
use crate::log::{self, Change};
use crate::{cites, transcripts, Error, Memory};
use dm_core::erasure::{Receipt, Sent};
use dm_core::{Entry, EntryType, Status, TrustTier};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// One withheld entry that no entry replaced yet.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Withheld {
    /// The entry.
    pub id: String,
    /// The erased entry, or the erased transcript, whose receipt names this entry as
    /// withheld. Absent when no receipt in a granted domain names it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub because: Option<String>,
}

/// What the lineage forward of an erased entry gave its erasure.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Downstream {
    /// The active entries one step away, each as its domain and its id: what the
    /// erasure withholds.
    pub near: Vec<(String, String)>,
    /// The entries downstream that the erasure does not withhold: two steps or more
    /// away, or one step away and out of the active set already, or one step away and a
    /// verified rule of the policy.
    pub further: Vec<String>,
    /// Every send in the lineage, at any depth.
    pub sends: Vec<Sent>,
}

/// Why a withheld entry is not served.
pub(crate) fn gone(id: &str) -> Error {
    Error::Gone(format!(
        "{id} is withheld after an erasure until it is derived again"
    ))
}

/// Put what a run found on the receipt, and give the entries to withhold. A receipt
/// keeps what an earlier run listed as withheld: that run changed those entries, so
/// this run finds them out of the active set and does not list them again. The entries
/// further downstream and the sends are as this run found them.
pub(crate) fn onto_receipt(receipt: &mut Receipt, found: Downstream) -> Vec<(String, String)> {
    for (_, id) in &found.near {
        if !receipt.withheld.contains(id) {
            receipt.withheld.push(id.clone());
        }
    }
    receipt.further = found
        .further
        .into_iter()
        .filter(|id| !receipt.withheld.contains(id))
        .collect();
    receipt.sends = found.sends;
    found.near
}

/// Whether `entry` names `id`: in its body, in a citation, or as the entry it replaces.
pub(crate) fn names(entry: &Entry, id: &str) -> bool {
    cites(&entry.body, id)
        || entry.front.cites.iter().any(|c| cites(c, id))
        || entry.front.supersedes.as_deref() == Some(id)
}

/// What a session read that is erased now.
enum ErasedRead {
    /// An entry, read before its erasure: its domain and its id.
    Entry {
        /// The domain of the entry and of its receipt.
        domain: String,
        /// The entry.
        id: String,
    },
    /// A transcript that the session read a turn of, by its session.
    Transcript(String),
}

/// Whether `entry` is a verified rule of the policy: one that a decision can take, and
/// that refuses and never permits. An exception is not one. Neither is a policy entry
/// below verified, which is in no decision.
fn verified_rule(entry: &Entry) -> bool {
    entry.front.entry_type == EntryType::Policy
        && entry.front.trust_tier == TrustTier::Verified
        && dm_policy::record::parse(&entry.front.id, entry.front.trust_tier, &entry.body)
            .is_ok_and(|rule| !rule.meta.is_exception())
}

/// The first of `events`, the log of one domain, in which `session` read an entry
/// before the `erased` event of that entry: the id of the entry. A read after the
/// erasure was of the stub, which holds nothing of the entry.
fn read_before_erasure<'a>(events: &'a [log::Event], session: &str) -> Option<&'a str> {
    let mut erased: BTreeMap<&str, u64> = BTreeMap::new();
    for e in events {
        if let Change::Erased { id } = &e.change {
            erased.entry(id.as_str()).or_insert(e.seq);
        }
    }
    events
        .iter()
        .filter(|e| e.session.as_deref() == Some(session))
        .find_map(|e| match &e.change {
            Change::Read { ids, .. } => ids
                .iter()
                .map(String::as_str)
                .find(|id| erased.get(id).is_some_and(|at| e.seq < *at)),
            _ => None,
        })
}

/// The transcripts that `session` read a turn of, by the `read` events of `events`: a
/// transcript is named by its session, which is no entry id.
fn transcripts_read<'a>(events: &'a [log::Event], session: &str) -> BTreeSet<&'a str> {
    events
        .iter()
        .filter(|e| e.session.as_deref() == Some(session))
        .filter_map(|e| match &e.change {
            Change::Read { ids, .. } => Some(ids),
            _ => None,
        })
        .flatten()
        .map(String::as_str)
        .filter(|id| !dm_core::id::is_entry_id(id))
        .collect()
}

impl Memory {
    /// Whether `entry` is withheld from this session's author: a withheld entry is read
    /// by the security role alone.
    pub(crate) fn withheld_from_author(&self, entry: &Entry) -> bool {
        entry.front.status == Some(Status::Withheld) && !self.grants().is_security(self.author())
    }

    /// What is downstream of `id`, the entry or the transcript an erasure removed: the
    /// active entries to withhold, the entries further downstream, and the sends. An
    /// entry is one step away when it names `id`, or when lineage reaches it in one
    /// step forward. Lineage is read from the whole log of each granted domain, since an
    /// erasure must miss nothing. Nothing is changed here: the erasure puts the answer
    /// on its receipt first, and then withholds ([`Memory::withhold`]).
    ///
    /// # Errors
    ///
    /// A store error.
    pub(crate) async fn downstream(&self, id: &str) -> Result<Downstream, Error> {
        let forward = lineage::trace(&self.logs_whole(), id, Direction::Forward, None);
        let hops: BTreeMap<&str, u32> = forward
            .entries
            .iter()
            .map(|e| (e.id.as_str(), e.hops))
            .collect();
        let mut found = Downstream {
            sends: forward.sends.clone(),
            ..Downstream::default()
        };
        for domain in self.domains() {
            for other in self.store.list(&domain).await? {
                if other == id {
                    continue;
                }
                let Some(e) = self.store.get(&domain, &other).await? else {
                    continue;
                };
                let near = names(&e, id) || hops.get(other.as_str()) == Some(&1);
                // nothing is left of an erased entry to withhold or to follow up
                if e.front.status == Some(Status::Erased)
                    || !(near || hops.contains_key(other.as_str()))
                {
                    continue;
                }
                // a verified rule stays in force: it is listed, and not withheld
                if near && e.is_active() && !verified_rule(&e) {
                    found.near.push((domain.clone(), other));
                } else {
                    found.further.push(other);
                }
            }
        }
        Ok(found)
    }

    /// Withhold each of `near`, an entry by its domain and its id: an active one gets
    /// `status: withheld` as a new version, through the log and the feed, and one that
    /// is out of the active set by now is left as it is.
    ///
    /// # Errors
    ///
    /// A store or index error, or the file system.
    pub(crate) async fn withhold(&self, near: &[(String, String)]) -> Result<(), Error> {
        for (domain, id) in near {
            let Some(mut e) = self.store.get(domain, id).await? else {
                continue;
            };
            if !e.is_active() {
                continue;
            }
            e.front.status = Some(Status::Withheld);
            let put = self.put_logged(&e).await?;
            self.feed_from(domain, put).await?;
        }
        // a withheld policy entry takes its rule or its exception out of force, as an
        // erased one does
        self.reload_policy().await
    }

    /// What this session read that is erased now: an entry that it read before the
    /// erasure, or a transcript that it read a turn of. The logs are read whole, and a
    /// log that does not read is an error, which the caller does not take as "nothing".
    async fn erased_read(&self) -> Result<Option<ErasedRead>, Error> {
        let session = self.session_id();
        let mut turns_of = BTreeSet::new();
        for domain in self.domains() {
            let events = log::read(self.root(), &domain, 0)?;
            if let Some(id) = read_before_erasure(&events, session) {
                let id = id.to_string();
                return Ok(Some(ErasedRead::Entry { domain, id }));
            }
            turns_of.extend(
                transcripts_read(&events, session)
                    .into_iter()
                    .map(str::to_string),
            );
        }
        if turns_of.is_empty() {
            return Ok(None);
        }
        let erased = self
            .transcripts()
            .await?
            .into_iter()
            .find(|t| t.state == transcripts::State::Erased && turns_of.contains(&t.session));
        Ok(erased.map(|t| ErasedRead::Transcript(t.session)))
    }

    /// Withhold `entry`, which this session just wrote, when the session read something
    /// that is erased now ([`Memory::erased_read`]): the entry is downstream of that
    /// erasure, as an entry written before the erasure is. The receipt of the erasure
    /// takes the entry in its withheld list. Returns the erased entry or transcript.
    ///
    /// The check comes after the record took the entry, so an erasure that runs at the
    /// same time finds the entry in the record, or this check finds the erasure in the
    /// log. A verified rule is never new, so the exception of [`Memory::downstream`]
    /// does not apply here.
    ///
    /// When the check cannot be made, the entry is withheld and the error is returned:
    /// the record holds the entry by then, and it must not be served unchecked.
    ///
    /// # Errors
    ///
    /// A log that does not read, or a store or index error.
    pub(crate) async fn withhold_after_erased_read(
        &self,
        entry: &Entry,
    ) -> Result<Option<String>, Error> {
        let erased = match self.erased_read().await {
            Ok(None) => return Ok(None),
            found => found,
        };
        let id = &entry.front.id;
        self.withhold(&[(entry.front.domain.clone(), id.clone())])
            .await?;
        let Some(erased) = erased? else {
            return Ok(None);
        };
        let listed = |receipt: &mut Receipt| {
            let new = !receipt.withheld.contains(id);
            if new {
                receipt.withheld.push(id.clone());
            }
            new
        };
        match &erased {
            ErasedRead::Entry { domain, id: gone } => {
                if let Some(mut receipt) = self.store.receipt(domain, gone).await? {
                    if listed(&mut receipt) {
                        self.store.put_receipt(&receipt).await?;
                    }
                }
            }
            ErasedRead::Transcript(session) => {
                if let Some(mut receipt) = self.transcript_erasure(session).await? {
                    if listed(&mut receipt) {
                        self.put_transcript_receipt(&receipt).await?;
                    }
                }
            }
        }
        Ok(Some(match erased {
            ErasedRead::Entry { id, .. } => id,
            ErasedRead::Transcript(session) => session,
        }))
    }

    /// The entries that replaced an entry of `domain`, as the record holds them: each
    /// replaced id with the active entry that names it in `supersedes`.
    pub(crate) async fn successors(&self, domain: &str) -> Result<BTreeMap<String, String>, Error> {
        let mut out = BTreeMap::new();
        for id in self.store.list(domain).await? {
            if let Some(e) = self.store.get(domain, &id).await? {
                if let (true, Some(old)) = (e.is_active(), e.front.supersedes) {
                    out.insert(old, id);
                }
            }
        }
        Ok(out)
    }

    /// For each withheld entry, the erased entry or transcript whose receipt names it,
    /// from the receipts of every granted domain. The first receipt that names an
    /// entry is the one given.
    async fn withheld_because(&self) -> Result<BTreeMap<String, String>, Error> {
        let mut receipts = Vec::new();
        for domain in self.domains() {
            receipts.extend(self.store.receipts(&domain).await?);
        }
        for t in self.transcripts().await? {
            if t.state == transcripts::State::Erased {
                receipts.extend(self.transcript_erasure(&t.session).await?);
            }
        }
        let mut out = BTreeMap::new();
        for r in receipts {
            for id in r.withheld {
                out.entry(id).or_insert_with(|| r.id.clone());
            }
        }
        Ok(out)
    }

    /// The withheld entries of a domain that no entry replaced: what is still to derive
    /// again, release or erase. For the security role, read from the record, and within
    /// what the session may handle. Each comes with the erased entry that the receipts
    /// name for it. It names entries and serves no text of them.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for an author outside the security role or an ungranted domain;
    /// a store error.
    pub async fn withheld(&self, domain: Option<&str>) -> Result<Vec<Withheld>, Error> {
        if !self.grants().is_security(self.author()) {
            return Err(Error::Denied(
                "the withheld entries are for the owner and the security role".into(),
            ));
        }
        let domain = self.resolve_domain(domain)?;
        let handling = self.handling()?;
        let replaced = self.successors(&domain).await?;
        let mut because = self.withheld_because().await?;
        let mut out = Vec::new();
        for id in self.store.list(&domain).await? {
            let Some(e) = self.store.get(&domain, &id).await? else {
                continue;
            };
            if e.front.status != Some(Status::Withheld)
                || e.front.classification > handling
                || replaced.contains_key(&id)
            {
                continue;
            }
            out.push(Withheld {
                because: because.remove(&id),
                id,
            });
        }
        Ok(out)
    }

    /// Release a withheld entry: the owner looked at it and it stands. Its status is
    /// cleared with a new version, through the log and the feed, and retrieval serves it
    /// again. The receipt that names it stays as it is: it says what the erasure did.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for anyone but the owner, for an entry that is not withheld,
    /// and for one that was derived again, which stays behind its successor;
    /// [`Error::NotFound`] for an entry in no granted domain; a store or index error.
    pub async fn release_withheld(&self, id: &str) -> Result<(), Error> {
        if self.author() != self.grants().owner {
            return Err(Error::Denied(format!(
                "only the owner {} releases a withheld entry",
                self.grants().owner
            )));
        }
        let (domain, mut entry) = self.find(id).await?;
        if entry.front.status != Some(Status::Withheld) {
            return Err(Error::Denied(format!("{id} is not withheld")));
        }
        if let Some(successor) = self.successors(&domain).await?.get(id) {
            return Err(Error::Denied(format!(
                "{id} was derived again as {successor} and stays withheld"
            )));
        }
        entry.front.status = None;
        let put = self.put_logged(&entry).await?;
        self.feed_from(&domain, put).await?;
        self.refresh_projection().await;
        if entry.front.entry_type == EntryType::Policy {
            self.reload_policy().await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{bundle, WriteRequest, INDEX_FILE};
    use dm_core::erasure::Reason;
    use dm_core::Label;
    use std::collections::BTreeSet;
    use std::path::Path;

    fn mem(dir: &Path) -> Memory {
        Memory::init(dir, "human:test:me", "dom-me").unwrap()
    }

    fn agent(dir: &Path, name: &str) -> Memory {
        Memory::open(dir).unwrap().with_author(name)
    }

    fn req(body: &str) -> WriteRequest {
        WriteRequest {
            body: body.into(),
            ..WriteRequest::default()
        }
    }

    /// An entry as a text editor would write it into the record, with `fields` among
    /// its frontmatter: one that the log of the domain does not hold.
    fn by_hand(id: &str, fields: &str) -> Entry {
        Entry::parse(&format!(
            "---\nid: {id}\ntype: fact\ndomain: dom-me\nauthor: human:test:me\n\
             created: 2026-10-01T09:00:00Z\n{fields}trust_tier: raw\n\
             classification: internal\n---\nFrom before the log.\n"
        ))
        .unwrap()
    }

    async fn wrote(m: &Memory, body: &str) -> String {
        m.write(req(body)).await.unwrap().id
    }

    fn set(ids: &[String]) -> BTreeSet<&str> {
        ids.iter().map(String::as_str).collect()
    }

    async fn status(m: &Memory, id: &str) -> Option<Status> {
        let kept = m.store.get("dom-me", id).await.unwrap().unwrap();
        kept.front.status
    }

    /// A root where the owner wrote `gone`, and an agent read it and then wrote `after`:
    /// the memory of the owner, and the two ids.
    async fn read_then_wrote(dir: &Path, after: WriteRequest) -> (Memory, String, String) {
        let owner = mem(dir);
        let gone = wrote(&owner, "Written in error.\n").await;
        let reader = agent(dir, "agent:r");
        reader.get(&gone).await.unwrap();
        let after = reader.write(after).await.unwrap().id;
        (owner, gone, after)
    }

    #[tokio::test]
    async fn an_erasure_withholds_what_a_reader_wrote_after_and_what_cites_it_and_nothing_before() {
        let dir = tempfile::tempdir().unwrap();
        let owner = mem(dir.path());
        let gone = wrote(&owner, "Written in error.\n").await;
        let reader = agent(dir.path(), "agent:r");
        let before = wrote(&reader, "Before reading.\n").await;
        reader.get(&gone).await.unwrap();
        let after = wrote(&reader, "Drawn from what it read.\n").await;
        // two sessions that did not read it: one cites it, one names it in its body
        let citing = WriteRequest {
            cites: vec![gone.clone()],
            ..req("It cites the entry.\n")
        };
        let cited = agent(dir.path(), "agent:c").write(citing).await.unwrap().id;
        let named = wrote(&agent(dir.path(), "agent:n"), &format!("See {gone}.\n")).await;
        let unrelated = wrote(&owner, "Unrelated, and stays so.\n").await;
        let review = owner.get(&after).await.unwrap().front.review_by;
        // two that the log does not hold, put in the record by hand: one cites it, and
        // one replaces it
        let (early_citer, early_successor) = ("mem-me-early001", "mem-me-early002");
        let cites = format!("cites:\n- {gone}\n");
        owner
            .store
            .put(&by_hand(early_citer, &cites))
            .await
            .unwrap();
        let supersedes = format!("supersedes: {gone}\n");
        let successor = by_hand(early_successor, &supersedes);
        owner.store.put(&successor).await.unwrap();

        let receipt = owner.erase(&gone, Reason::Mistaken).await.unwrap();
        let withheld = [
            after.clone(),
            cited.clone(),
            named.clone(),
            early_citer.to_string(),
            early_successor.to_string(),
        ];
        assert_eq!(set(&receipt.withheld), set(&withheld));
        assert!(receipt.flagged.is_empty() && receipt.further.is_empty());
        for id in &withheld {
            assert_eq!(status(&owner, id).await, Some(Status::Withheld), "{id}");
        }
        for id in [&before, &unrelated] {
            assert_eq!(status(&owner, id).await, None, "{id}");
            owner.get(id).await.unwrap();
        }
        // withheld, and nothing else of the entry changed: no review date is moved
        let kept = owner.store.get("dom-me", &after).await.unwrap().unwrap();
        assert_eq!(kept.front.review_by, review);
        assert_eq!(kept.body, "Drawn from what it read.\n");
    }

    #[tokio::test]
    async fn a_write_after_the_erasure_by_a_session_that_read_the_entry_lands_withheld() {
        let dir = tempfile::tempdir().unwrap();
        let owner = mem(dir.path());
        let gone = wrote(&owner, "Written in error.\n").await;
        let reader = agent(dir.path(), "agent:r");
        reader.get(&gone).await.unwrap();
        let before = wrote(&reader, "The yak is shaved.\n").await;
        let receipt = owner.erase(&gone, Reason::Mistaken).await.unwrap();
        assert_eq!(receipt.withheld, std::slice::from_ref(&before));

        // the same session writes again, from the same context
        let after = reader.write(req("The yak is shorn.\n")).await.unwrap();
        assert_eq!(after.withheld.as_deref(), Some(gone.as_str()));
        assert_eq!(status(&owner, &after.id).await, Some(Status::Withheld));
        assert!(matches!(owner.get(&after.id).await, Err(Error::Gone(_))));
        assert!(owner.search("shorn", None, 10).await.unwrap().is_empty());
        // and it cannot derive the withheld entry again: a reader of the old id is not
        // sent to an entry from the same context
        let again = WriteRequest {
            supersedes: Some(before.clone()),
            ..req("The yak is shaved, again.\n")
        };
        let again = reader.write(again).await.unwrap();
        assert_eq!(status(&owner, &again.id).await, Some(Status::Withheld));
        let refused = owner.get_following(&before).await;
        assert!(
            matches!(&refused, Err(Error::Gone(why)) if why.contains("withheld")),
            "{refused:?}"
        );
        // the receipt names each, and the owner still finds all three to follow up
        let receipt = owner.receipt(&gone).await.unwrap().unwrap();
        let all = [before.clone(), after.id.clone(), again.id.clone()];
        assert_eq!(set(&receipt.withheld), set(&all));
        let listed: Vec<String> = owner
            .withheld(None)
            .await
            .unwrap()
            .into_iter()
            .map(|w| {
                assert_eq!(w.because.as_deref(), Some(gone.as_str()), "{}", w.id);
                w.id
            })
            .collect();
        assert_eq!(set(&listed), set(&all));

        // a session that read only the stub, after the erasure, read nothing of the entry
        let late = agent(dir.path(), "agent:late");
        late.archived(&gone).await.unwrap();
        let free = late.write(req("Unrelated, and served.\n")).await.unwrap();
        assert_eq!(free.withheld, None);
        assert_eq!(status(&owner, &free.id).await, None);
        // and so did a session that read nothing
        let other = agent(dir.path(), "agent:o")
            .write(req("Also served.\n"))
            .await
            .unwrap();
        assert_eq!(other.withheld, None);
    }

    #[tokio::test]
    async fn a_write_is_withheld_when_a_log_does_not_read_for_the_check() {
        let dir = tempfile::tempdir().unwrap();
        drop(mem(dir.path()));
        // a second granted domain, with a directory where its log would be
        let grants = dir.path().join(crate::GRANTS_FILE);
        let mut text = std::fs::read_to_string(&grants).unwrap();
        text.push_str("\n[domains.dom-b]\ndefault_classification = \"internal\"\n");
        std::fs::write(&grants, text).unwrap();
        std::fs::create_dir_all(dir.path().join(log::DIR).join("dom-b.jsonl")).unwrap();
        let m = Memory::open(dir.path()).unwrap();
        let note = WriteRequest {
            domain: Some("dom-me".into()),
            ..req("The yak is shaved.\n")
        };
        let failed = m.write(note).await;
        assert!(matches!(failed, Err(Error::Io { .. })), "{failed:?}");
        // the record took the entry before the check: no read gives it
        let ids = m.store.list("dom-me").await.unwrap();
        let [id] = &ids[..] else {
            panic!("one entry: {ids:?}")
        };
        assert_eq!(status(&m, id).await, Some(Status::Withheld));
        let hits = m.search("yak", Some("dom-me"), 10).await.unwrap();
        assert!(hits.is_empty(), "{hits:?}");
    }

    #[tokio::test]
    async fn a_withheld_entry_is_in_no_answer_and_its_body_is_still_in_the_record() {
        let dir = tempfile::tempdir().unwrap();
        let decision = WriteRequest {
            entry_type: EntryType::Decision,
            title: Some("Zebra crossing".into()),
            ..req("The zebra crossing moves north.\n")
        };
        let (owner, gone, held) = read_then_wrote(dir.path(), decision).await;
        let ids = |hits: Vec<dm_index::Hit>| hits.into_iter().map(|h| h.id).collect::<Vec<_>>();
        // served before the erasure, by each way in
        assert!(ids(owner.search("zebra", None, 10).await.unwrap()).contains(&held));
        assert!(owner.t0(None).await.unwrap().contains("Zebra crossing"));
        let render = dir
            .path()
            .join(crate::projection::PROJECTIONS)
            .join("claude/sess-a");
        owner.project_into(&render).await.unwrap();
        let rendered = render.join(format!("dom-me/{held}.md"));
        assert!(rendered.is_file());

        owner.erase(&gone, Reason::Mistaken).await.unwrap();
        // the next render of a projection leaves it out
        owner.project_into(&render).await.unwrap();
        assert!(!rendered.exists());
        assert!(ids(owner.search("zebra", None, 10).await.unwrap()).is_empty());
        assert!(!ids(owner.list(None, None, 100).await.unwrap()).contains(&held));
        assert!(!owner.t0(None).await.unwrap().contains("Zebra"));
        let recall = owner
            .recall(
                "zebra crossing",
                None,
                None,
                crate::recall::Budget::default(),
            )
            .await
            .unwrap();
        assert!(recall.pack.pieces.is_empty(), "{:?}", recall.pack.pieces);
        let refused = owner.get(&held).await;
        assert!(
            matches!(&refused, Err(Error::Gone(why)) if why.contains("withheld")),
            "{refused:?}"
        );
        let file = dir.path().join(format!("record/dom-me/{held}.md"));
        let text = std::fs::read_to_string(file).unwrap();
        assert!(
            text.contains("status: withheld") && text.contains("The zebra crossing moves north."),
            "{text}"
        );
        // a rebuild from the record alone keeps it withheld
        owner.reindex().await.unwrap();
        assert!(ids(owner.search("zebra", None, 10).await.unwrap()).is_empty());
        // and it is not promoted
        let refused = owner
            .promote(&held, dm_core::TrustTier::Curated, "why")
            .await;
        assert!(
            matches!(&refused, Err(Error::Gone(why)) if why.contains("withheld")),
            "{refused:?}"
        );
    }

    const REQUEST: &[u8] = br#"{"messages":[{"role":"user","content":"Which port?"}]}"#;

    async fn model_call(m: &Memory, to: &str) {
        m.keep_context(to, "/v1/messages", REQUEST, b"{}")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn the_receipt_lists_what_was_withheld_what_is_further_and_each_send_after_the_read() {
        let dir = tempfile::tempdir().unwrap();
        let owner = mem(dir.path());
        let gone = wrote(&owner, "Written in error.\n").await;
        let reader = agent(dir.path(), "agent:r");
        model_call(&reader, "before-the-read").await;
        reader.get(&gone).await.unwrap();
        let near = wrote(&reader, "Drawn from what it read.\n").await;
        model_call(&reader, "model-one").await;
        // a second session reads what the first wrote, writes, and sends
        let second = agent(dir.path(), "agent:s");
        second.get(&near).await.unwrap();
        let far = wrote(&second, "A step further on.\n").await;
        model_call(&second, "model-two").await;

        let receipt = owner.erase(&gone, Reason::Mistaken).await.unwrap();
        assert_eq!(receipt.withheld, std::slice::from_ref(&near));
        assert_eq!(receipt.further, std::slice::from_ref(&far));
        assert_eq!(status(&owner, &far).await, None, "listed, and not withheld");
        let sent: Vec<(&str, &str, &str, u64)> = receipt
            .sends
            .iter()
            .map(|s| {
                (
                    s.to.as_str(),
                    s.session.as_str(),
                    s.domain.as_str(),
                    s.count,
                )
            })
            .collect();
        assert_eq!(
            sent,
            [
                ("model-one", reader.session_id(), "dom-me", 1),
                ("model-two", second.session_id(), "dom-me", 1)
            ]
        );
        assert!(
            receipt.unresolved.iter().any(|u| u.contains("event log")),
            "{:?}",
            receipt.unresolved
        );
        // the receipt on disk says the same
        assert_eq!(owner.receipt(&gone).await.unwrap(), Some(receipt));
    }

    #[tokio::test]
    async fn an_erasure_run_again_keeps_the_withheld_list() {
        let dir = tempfile::tempdir().unwrap();
        let (owner, gone, after) = read_then_wrote(dir.path(), req("Drawn from it.\n")).await;
        // another process is mid-query on the index, so the erasure stops at its vacuum
        let other = rusqlite::Connection::open(dir.path().join(INDEX_FILE)).unwrap();
        let mut stmt = other.prepare("SELECT id FROM entries").unwrap();
        let mut rows = stmt.query([]).unwrap();
        let _first = rows.next().unwrap();
        let first = owner.erase(&gone, Reason::Mistaken).await;
        assert!(
            matches!(first, Err(Error::Index(dm_index::Error::Held))),
            "{first:?}"
        );
        drop(rows);
        drop(stmt);
        drop(other);
        let pending = owner.receipt(&gone).await.unwrap().unwrap();
        assert!(!pending.is_finished(), "{pending:?}");
        assert_eq!(
            pending.withheld,
            std::slice::from_ref(&after),
            "on the receipt already"
        );
        assert_eq!(status(&owner, &after).await, Some(Status::Withheld));
        let again = owner.erase(&gone, Reason::Mistaken).await.unwrap();
        assert!(again.is_finished(), "{again:?}");
        assert_eq!(again.withheld, std::slice::from_ref(&after));
        assert!(again.further.is_empty(), "{:?}", again.further);
    }

    #[tokio::test]
    async fn a_run_that_stopped_before_it_withheld_leaves_the_list_and_the_next_run_withholds() {
        let dir = tempfile::tempdir().unwrap();
        let (owner, gone, after) = read_then_wrote(dir.path(), req("Drawn from it.\n")).await;
        let first = owner.erase(&gone, Reason::Mistaken).await.unwrap();
        // as a run leaves the record that stopped after it wrote the list on the receipt
        // and before it changed the entry
        owner.release_withheld(&after).await.unwrap();
        let mut pending = first.clone();
        pending.unresolved.push(dm_core::erasure::UNFINISHED.into());
        owner.store.put_receipt(&pending).await.unwrap();
        let again = owner.erase(&gone, Reason::Mistaken).await.unwrap();
        assert!(again.is_finished(), "{again:?}");
        assert_eq!(again.withheld, std::slice::from_ref(&after), "listed once");
        assert_eq!(status(&owner, &after).await, Some(Status::Withheld));
    }

    #[tokio::test]
    async fn the_archive_gives_a_withheld_entry_to_the_security_role_alone() {
        let dir = tempfile::tempdir().unwrap();
        let (owner, gone, held) = read_then_wrote(dir.path(), req("Drawn from it.\n")).await;
        owner
            .promote(&held, dm_core::TrustTier::Curated, "it was checked")
            .await
            .unwrap();
        owner.erase(&gone, Reason::Mistaken).await.unwrap();
        let stranger = agent(dir.path(), "agent:x");
        let refused = stranger.archived(&held).await;
        assert!(
            matches!(&refused, Err(Error::Gone(why)) if why.contains("withheld")),
            "{refused:?}"
        );
        let listed = stranger.list_archive(None, 100).await.unwrap();
        assert!(listed.iter().all(|h| h.id != held), "{listed:?}");
        assert!(matches!(
            stranger.promotions(&held).await,
            Err(Error::NotFound(_))
        ));
        assert!(matches!(
            stranger.withheld(None).await,
            Err(Error::Denied(_))
        ));
        // the owner is of the security role
        assert_eq!(
            owner.archived(&held).await.unwrap().body,
            "Drawn from it.\n"
        );
        let listed = owner.list_archive(None, 100).await.unwrap();
        assert!(listed.iter().any(|h| h.id == held), "{listed:?}");
        assert_eq!(owner.promotions(&held).await.unwrap().len(), 1);
        assert_eq!(
            owner.withheld(None).await.unwrap(),
            [Withheld {
                id: held,
                because: Some(gone)
            }]
        );
        // within what the session may handle, as every read is: the entry is internal
        let capped = Memory::open(dir.path())
            .unwrap()
            .with_handling_cap(Label::Public);
        assert!(capped.withheld(None).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_export_leaves_a_withheld_entry_out() {
        let dir = tempfile::tempdir().unwrap();
        let (owner, gone, held) = read_then_wrote(dir.path(), req("Drawn from it.\n")).await;
        let kept = wrote(&owner, "Unrelated, and stays so.\n").await;
        owner.erase(&gone, Reason::Mistaken).await.unwrap();
        let dest = bundle::bundle_dir(dir.path(), "dom-me", Label::Internal);
        let exported = owner
            .export(Some("dom-me"), Label::Internal, &dest)
            .await
            .unwrap();
        let carried: Vec<&str> = exported
            .manifest
            .entries
            .iter()
            .map(|e| e.id.as_str())
            .collect();
        assert!(carried.contains(&kept.as_str()), "{carried:?}");
        assert!(!carried.contains(&held.as_str()), "{carried:?}");
        assert!(!dest.join(format!("{held}.md")).exists());
        assert!(dest.join(format!("{kept}.md")).is_file());
    }

    #[tokio::test]
    async fn only_the_owner_releases_and_only_a_withheld_entry_and_it_is_served_again() {
        let dir = tempfile::tempdir().unwrap();
        let (owner, gone, held) = read_then_wrote(dir.path(), req("The yak is shaved.\n")).await;
        let live = wrote(&owner, "Unrelated, and stays so.\n").await;
        owner.erase(&gone, Reason::Mistaken).await.unwrap();
        let stranger = agent(dir.path(), "agent:x");
        assert!(matches!(
            stranger.release_withheld(&held).await,
            Err(Error::Denied(_))
        ));
        assert_eq!(status(&owner, &held).await, Some(Status::Withheld));
        let refused = owner.release_withheld(&live).await;
        assert!(
            matches!(&refused, Err(Error::Denied(why)) if why.contains("not withheld")),
            "{refused:?}"
        );
        assert!(matches!(
            owner.release_withheld("mem-me-nothere").await,
            Err(Error::NotFound(_))
        ));
        owner.release_withheld(&held).await.unwrap();
        assert_eq!(status(&owner, &held).await, None);
        assert_eq!(
            stranger.get(&held).await.unwrap().body,
            "The yak is shaved.\n"
        );
        let hits = stranger.search("yak", None, 10).await.unwrap();
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert!(owner.withheld(None).await.unwrap().is_empty());
        let refused = owner.release_withheld(&held).await;
        assert!(matches!(refused, Err(Error::Denied(_))), "released once");
    }

    #[tokio::test]
    async fn an_entry_that_supersedes_a_withheld_one_is_served_and_the_old_one_is_listed_no_more() {
        let dir = tempfile::tempdir().unwrap();
        let (owner, gone, held) = read_then_wrote(dir.path(), req("The yak is shaved.\n")).await;
        owner.erase(&gone, Reason::Mistaken).await.unwrap();
        assert_eq!(owner.withheld(None).await.unwrap().len(), 1);
        // derived again, by a session that read nothing erased
        let writer = agent(dir.path(), "agent:w");
        let again = WriteRequest {
            supersedes: Some(held.clone()),
            ..req("The yak is shaved, from what remains.\n")
        };
        let new = writer.write(again).await.unwrap().id;
        assert_eq!(status(&owner, &new).await, None);
        assert_eq!(
            writer.get(&new).await.unwrap().front.supersedes,
            Some(held.clone())
        );
        assert_eq!(writer.search("yak", None, 10).await.unwrap().len(), 1);
        // the record leaves the withheld one as it is, and nothing lists it
        assert_eq!(status(&owner, &held).await, Some(Status::Withheld));
        assert!(owner.withheld(None).await.unwrap().is_empty());
        // a reader of the old id goes to the new one
        let moved = writer.get(&held).await;
        assert!(
            matches!(&moved, Err(Error::Moved { to, .. }) if *to == new),
            "{moved:?}"
        );
        // and the old one is not released behind its successor
        let refused = owner.release_withheld(&held).await;
        assert!(
            matches!(&refused, Err(Error::Denied(why)) if why.contains("derived again")),
            "{refused:?}"
        );
    }

    #[tokio::test]
    async fn an_entry_out_of_the_active_set_is_listed_and_left_as_it_is() {
        let dir = tempfile::tempdir().unwrap();
        let (owner, gone, old) = read_then_wrote(dir.path(), req("The yak is shaved.\n")).await;
        // replaced before the erasure, by a session that read nothing erased
        let writer = agent(dir.path(), "agent:w");
        let newer = WriteRequest {
            supersedes: Some(old.clone()),
            ..req("The yak is shorn.\n")
        };
        let new = writer.write(newer).await.unwrap().id;
        // and one that another reader wrote after it read the entry, erased itself
        // before: nothing is left of it to follow up
        let other = agent(dir.path(), "agent:r2");
        other.get(&gone).await.unwrap();
        let dropped = wrote(&other, "Also drawn from it.\n").await;
        owner.erase(&dropped, Reason::Mistaken).await.unwrap();
        let receipt = owner.erase(&gone, Reason::Mistaken).await.unwrap();
        assert_eq!(status(&owner, &old).await, Some(Status::Superseded));
        assert!(receipt.withheld.is_empty(), "{:?}", receipt.withheld);
        // the replaced one, one step away, and its successor, two steps away, are
        // listed for whoever follows up; neither is changed
        assert_eq!(set(&receipt.further), set(&[old.clone(), new.clone()]));
        assert_eq!(status(&owner, &new).await, None);
        // and an entry that left the active set after the receipt listed it stays as
        // it is
        owner
            .withhold(&[("dom-me".into(), old.clone())])
            .await
            .unwrap();
        assert_eq!(status(&owner, &old).await, Some(Status::Superseded));
    }

    #[tokio::test]
    async fn a_proposal_to_verify_a_withheld_entry_is_not_approved() {
        let dir = tempfile::tempdir().unwrap();
        let (owner, gone, held) = read_then_wrote(dir.path(), req("Drawn from it.\n")).await;
        let proposed = agent(dir.path(), "agent:curator")
            .promote(&held, dm_core::TrustTier::Verified, "checked")
            .await
            .unwrap();
        let crate::Promotion::Proposed { task } = proposed else {
            panic!("verified is proposed")
        };
        owner.erase(&gone, Reason::Mistaken).await.unwrap();
        let refused = owner.approve(&task.id).await;
        assert!(
            matches!(&refused, Err(Error::Denied(why)) if why.contains("withheld")),
            "{refused:?}"
        );
    }

    #[test]
    fn a_receipt_keeps_what_an_earlier_run_withheld_and_lists_it_once() {
        let mut receipt = Receipt::from_yaml(
            "id: mem-me-1\ndomain: dom-me\nerased_at: 1970-01-01T00:00:00Z\nerased_by: human:test:me\nreason: legal\nremoved: []\nwithheld:\n- mem-me-2\n",
        )
        .unwrap();
        let found = Downstream {
            near: vec![
                ("dom-me".into(), "mem-me-3".into()),
                ("dom-me".into(), "mem-me-2".into()),
            ],
            // withheld by the earlier run, so out of the active set for this one
            further: vec!["mem-me-2".into(), "mem-me-4".into()],
            sends: Vec::new(),
        };
        let near = onto_receipt(&mut receipt, found);
        assert_eq!(near.len(), 2);
        assert_eq!(receipt.withheld, ["mem-me-2", "mem-me-3"]);
        assert_eq!(receipt.further, ["mem-me-4"]);
    }
}
