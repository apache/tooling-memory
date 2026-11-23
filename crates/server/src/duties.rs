//! The duties of the curator and the auditor that the library holds: the demotion of
//! dead material behind a tombstone, and the convention checks.
//!
//! A demotion writes a new version of the entry with `status: demoted`, through the log
//! of the domain and the feed of the index. The body stays in the record, and the
//! archive serves it for audit. The log takes two events: the `put` of the version, and
//! then a `demoted` event with the id and a reason code. A reason is a code and never a
//! text, so an erasure has nothing to remove from the log.
//!
//! An entry is dead when nothing used it inside a window of time: no read of it is in
//! the log, and no active entry names it. An agent demotes only a dead entry, and only
//! on a task of a curator or of the auditor. A person demotes for either reason, and
//! only a person restores.
//!
//! The convention checks read the active entries and write nothing. Each failure names
//! the entry and the person who answers for it. [`Memory::file_failures`] files one task
//! for each failure. An entry that cites a replaced entry gets a task for a curator, at
//! a priority above each other curation task.

use crate::log::{self, Change};
use crate::{reach, withhold, Error, Memory};
use dm_core::{Entry, EntryType, Label, Status, TrustTier};
use dm_ledger::TaskSpec;
use dm_policy::decide::Class;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use time::format_description::well_known::Rfc3339;
use time::{Date, Duration, OffsetDateTime};

/// The window of [`Memory::demote`] for an agent, in days: an entry that was read
/// inside this many days is not dead.
pub const DEAD_WINDOW_DAYS: i64 = 90;

/// The citation that marks a canary. A canary is planted, and nothing reads it by design.
const CANARY: &str = "canary";

/// Why an entry is demoted. A code, and never a text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DemoteReason {
    /// Nothing used the entry inside the window.
    Dead,
    /// The entry says something that does not hold. A person judges that.
    Wrong,
}

impl DemoteReason {
    /// The code, as the log holds it.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            DemoteReason::Dead => "dead",
            DemoteReason::Wrong => "wrong",
        }
    }
}

/// What a demotion did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Demotion {
    /// The entry.
    pub id: String,
    /// Its domain.
    pub domain: String,
    /// Why.
    pub reason: DemoteReason,
    /// Who demoted it.
    pub by: String,
    /// The digest of the tombstone version.
    pub digest: String,
}

/// A dead entry: a candidate for demotion.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Dead {
    /// The entry.
    pub id: String,
    /// Its title, or its id when it has none.
    pub title: String,
    /// When it was written.
    pub created: String,
    /// The time of the last read that the log holds, when there is one.
    pub last_read: Option<String>,
}

/// Why an entry is not dead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Alive {
    /// It is not in the active set.
    Inactive,
    /// A handoff, a policy entry or a canary: demotion is not how these end.
    Kind,
    /// It is verified.
    Verified,
    /// It was written inside the window.
    Young,
    /// It was read inside the window.
    Read,
    /// An active entry names it.
    Named,
}

impl Alive {
    fn why(self) -> &'static str {
        match self {
            Alive::Inactive => "it is not an active entry",
            Alive::Kind => "it is a handoff, a policy entry or a canary",
            Alive::Verified => "it is verified",
            Alive::Young => "it was written inside the window",
            Alive::Read => "it was read inside the window",
            Alive::Named => "an active entry names it",
        }
    }
}

/// The reads of a log: the last read of each entry whose time parses, and the entries
/// with a read inside the window.
#[derive(Default)]
struct Reads<'a> {
    last: BTreeMap<&'a str, OffsetDateTime>,
    recent: BTreeSet<&'a str>,
}

/// The reads among `events`. A read at or after `since` is inside the window. A read
/// whose time does not parse counts as inside the window.
fn reads_of(events: &[log::Event], since: OffsetDateTime) -> Reads<'_> {
    let mut out = Reads::default();
    for e in events {
        let Change::Read { ids } = &e.change else {
            continue;
        };
        let at = OffsetDateTime::parse(&e.at, &Rfc3339).ok();
        for id in ids {
            if at.is_none_or(|t| t >= since) {
                out.recent.insert(id);
            }
            if let Some(t) = at {
                let last = out.last.entry(id).or_insert(t);
                *last = (*last).max(t);
            }
        }
    }
    out
}

/// Whether an entry is a canary.
fn is_canary(entry: &Entry) -> bool {
    entry.front.cites.iter().any(|c| c == CANARY)
}

/// Whether a name is the name of a person: `human:<system>:<id>`.
pub(crate) fn is_person(name: &str) -> bool {
    name.starts_with("human:")
}

/// Why `entry` is not dead, or `None` when it is dead. `entries` are the entries that
/// can name it.
fn alive(
    entry: &Entry,
    entries: &[Entry],
    reads: &Reads<'_>,
    since: OffsetDateTime,
) -> Option<Alive> {
    let front = &entry.front;
    if !entry.is_active() {
        return Some(Alive::Inactive);
    }
    if matches!(front.entry_type, EntryType::Handoff | EntryType::Policy) || is_canary(entry) {
        return Some(Alive::Kind);
    }
    if front.trust_tier == TrustTier::Verified {
        return Some(Alive::Verified);
    }
    if front.created >= since {
        return Some(Alive::Young);
    }
    if reads.recent.contains(front.id.as_str()) {
        return Some(Alive::Read);
    }
    let named = entries.iter().any(|other| {
        other.is_active() && other.front.id != front.id && withhold::names(other, &front.id)
    });
    named.then_some(Alive::Named)
}

/// The dead entries among `entries`, as of `now`. An entry is dead when each of these
/// is true:
///
/// - It is active.
/// - It is not a handoff, not a policy entry and not a canary.
/// - It is not verified.
/// - It was written more than `window` before `now`.
/// - No `read` event among `events` names it with a time inside the window. A time that
///   does not parse counts as inside the window.
/// - No other active entry names it: in a citation, as the entry that it replaces, or
///   in its body.
#[must_use]
pub fn dead_among(
    entries: &[Entry],
    events: &[log::Event],
    now: OffsetDateTime,
    window: Duration,
) -> Vec<Dead> {
    let since = now - window;
    let reads = reads_of(events, since);
    entries
        .iter()
        .filter(|e| alive(e, entries, &reads, since).is_none())
        .map(|e| Dead {
            id: e.front.id.clone(),
            title: e.title(),
            created: e.front.created.format(&Rfc3339).unwrap_or_default(),
            last_read: reads
                .last
                .get(e.front.id.as_str())
                .and_then(|t| t.format(&Rfc3339).ok()),
        })
        .collect()
}

/// A convention check.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Check {
    /// A fact has a review date, and no entry is far past its review date.
    Freshness,
    /// A convention and a method name the person who answers for them.
    Ownership,
    /// An entry names only entries that the record serves.
    CrossLink,
    /// An entry does not cite an entry that another entry replaced.
    Superseded,
}

impl Check {
    /// The word for the check, as a task origin holds it.
    #[must_use]
    pub fn word(self) -> &'static str {
        match self {
            Check::Freshness => "freshness",
            Check::Ownership => "ownership",
            Check::CrossLink => "cross-link",
            Check::Superseded => "superseded",
        }
    }
}

/// One failure of a convention check. It names entries, and holds no text of them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Failure {
    /// The check that failed.
    pub check: Check,
    /// The entry that fails it.
    pub entry: String,
    /// The person who answers for the entry: its owner, or the owner of the grants.
    pub owner: String,
    /// What is wrong, in one sentence.
    pub why: String,
    /// For the check `superseded`, the entry that replaced the cited entry.
    pub names: Option<String>,
    /// The classification of the entry.
    pub classification: Label,
}

/// How the convention checks run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CheckOptions {
    /// An entry fails the freshness check when its review date is more than this many
    /// days in the past. An entry that is due inside these days is the curator's to
    /// verify.
    pub overdue_days: i64,
    /// The most entries that one run reads.
    pub limit: usize,
}

impl Default for CheckOptions {
    fn default() -> Self {
        CheckOptions {
            overdue_days: 30,
            limit: 2000,
        }
    }
}

/// The freshness check on one entry.
fn freshness(entry: &Entry, today: Date, overdue_days: i64) -> Option<String> {
    let id = &entry.front.id;
    match entry.front.review_by {
        None if entry.front.entry_type == EntryType::Fact => {
            Some(format!("{id} is a fact and has no review date."))
        }
        Some(due) if due + Duration::days(overdue_days) < today => Some(format!(
            "The review date {due} of {id} is more than {overdue_days} days in the past."
        )),
        _ => None,
    }
}

/// The ownership check on one entry.
fn ownership(entry: &Entry) -> Option<String> {
    let kind = entry.front.entry_type;
    (matches!(kind, EntryType::Convention | EntryType::Method) && entry.front.owner.is_none())
        .then(|| format!("{} is a {kind} and names no owner.", entry.front.id))
}

/// The entries that `entry` links to: the entry that it replaces, and then each
/// citation that is an entry id. The flag says whether the link is a citation.
fn links(entry: &Entry) -> impl Iterator<Item = (&str, bool)> {
    let cited = entry
        .front
        .cites
        .iter()
        .filter(|c| c.starts_with("mem-"))
        .map(|c| (c.as_str(), true));
    entry
        .front
        .supersedes
        .as_deref()
        .map(|s| (s, false))
        .into_iter()
        .chain(cited)
}

/// The upstream event of the task for a failure: the same failure gives the same task.
fn failure_event(f: &Failure) -> String {
    let of = format!(
        "{}\n{}\n{}",
        f.check.word(),
        f.entry,
        f.names.as_deref().unwrap_or("")
    );
    dm_core::digest(&of).chars().take(16).collect()
}

/// The task for one failure, in `domain`.
fn failure_task(domain: &str, f: &Failure) -> TaskSpec {
    let event = Some(failure_event(f));
    let entry = &f.entry;
    if f.check == Check::Superseded {
        let from = f.names.as_deref();
        let source = from.map_or_else(
            || "the current version of what it cites, which `search` finds".to_string(),
            |n| format!("`{n}`"),
        );
        return TaskSpec {
            domain: domain.to_string(),
            title: format!(
                "Curator: derive {entry} again from {}",
                from.unwrap_or("the current version of its source")
            ),
            body: format!(
                "{} Read `{entry}` and {source} with `get`. Then `write` `{entry}` again from the current version, with `supersedes: {entry}` and the current version in `cites`.",
                f.why
            ),
            origin: Some("curator:rederive".into()),
            upstream_event: event,
            classification: f.classification,
            priority: 3,
            ..TaskSpec::default()
        };
    }
    TaskSpec {
        domain: domain.to_string(),
        title: format!("Convention, {}: {entry}", f.check.word()),
        body: format!("{} The owner of `{entry}` is {}.", f.why, f.owner),
        requirements: vec!["human".into()],
        origin: Some(format!("convention:{}", f.check.word())),
        upstream_event: event,
        classification: f.classification,
        priority: 2,
        ..TaskSpec::default()
    }
}

/// Why an entry that is not live is not demoted.
fn not_live(id: &str, status: Status) -> Error {
    Error::Denied(match status {
        Status::Demoted => format!("{id} is demoted already"),
        Status::Superseded => format!("{id} was replaced, and is out of the active set already"),
        Status::Erased => format!("{id} is erased; nothing is left to demote"),
        Status::Withheld => {
            format!("{id} is withheld after an erasure; the owner releases it or erases it")
        }
    })
}

impl Memory {
    /// Whether this session is the session of a person: the owner, or another person.
    fn of_a_person(&self) -> bool {
        matches!(
            Class::of(self.author(), &self.grants().owner),
            Class::Owner | Class::Human
        )
    }

    /// Whether this session holds a task of a curator or of the auditor.
    async fn on_duty(&self) -> bool {
        let Some(held) = self.held_task() else {
            return false;
        };
        let task = self.ledger.lock().await.get(&held).await;
        task.is_ok_and(|t| reach::curates(&t))
    }

    /// Each entry of each granted domain, as the record holds it, and each event of the
    /// logs of those domains. Nothing is marked as read, and nothing of this is served.
    async fn record_and_logs(&self) -> Result<(Vec<Entry>, Vec<log::Event>), Error> {
        let mut entries = Vec::new();
        let mut events = Vec::new();
        for domain in self.domains() {
            for id in self.store.list(&domain).await? {
                entries.extend(self.store.get(&domain, &id).await?);
            }
            events.extend(log::read(&self.root, &domain, 0)?);
        }
        Ok((entries, events))
    }

    /// The dead entries that the read filter of this session admits, in one domain or
    /// in each domain that the session reads: the candidates for a demotion. See
    /// [`dead_among`] for the rule. An active entry that this session does not read
    /// still keeps an entry that it names out of the list.
    ///
    /// The list gives the title of each entry, so the label of the session rises to the
    /// classification of each. The log takes no `read` event: a survey is not a use,
    /// and a `read` event would make each candidate live again.
    ///
    /// # Errors
    ///
    /// A domain that the session does not read, or a store or index error.
    pub async fn dead(&self, domain: Option<&str>, window_days: i64) -> Result<Vec<Dead>, Error> {
        let f = self.filter_for(domain)?;
        let admitted: BTreeMap<String, Label> = {
            let ix = self.index.lock().await;
            let hits = ix.list_every(&f, None)?;
            hits.into_iter().map(|h| (h.id, h.classification)).collect()
        };
        let (entries, events) = self.record_and_logs().await?;
        let now = OffsetDateTime::now_utc();
        let mut dead = dead_among(&entries, &events, now, Duration::days(window_days));
        let mut session = crate::lock(&self.session);
        dead.retain(|d| {
            admitted
                .get(&d.id)
                .is_some_and(|label| session.observe(*label))
        });
        Ok(dead)
    }

    /// Demote an entry behind a tombstone. The entry gets a new version with
    /// `status: demoted`. Retrieval does not serve it, [`Memory::get`] says that it is
    /// gone, and [`Memory::archived`] gives it with its full body. The log of the domain
    /// takes the `put` of the version and then a `demoted` event with the reason code.
    ///
    /// A person demotes each live entry that the person reads, for either reason. An
    /// agent demotes only on a task of a curator or of the auditor, only for the reason
    /// `dead`, never a verified entry, and only an entry that is dead inside
    /// [`DEAD_WINDOW_DAYS`]. A handoff and a policy entry are not demoted: a handoff
    /// ends with its task, and a rule is retired through its lifecycle.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for an entry that this session does not read. For an entry
    /// that it reads and that is not served, [`Error::Moved`] or [`Error::Gone`] as from
    /// [`Memory::get`]. [`Error::Denied`] for each refusal above, and for an entry with
    /// tap reach and a session that does not read all raw entries. A store or index
    /// error.
    pub async fn demote(&self, id: &str, reason: DemoteReason) -> Result<Demotion, Error> {
        let f = self.session_filter()?;
        if self.index.lock().await.meta(id, &f)?.is_none() {
            return Err(self.not_served(id, None).await);
        }
        let (domain, mut entry) = self.find(id).await?;
        // the index can lag the record: the record says whether the entry is live
        if let Some(status) = entry.front.status {
            return Err(not_live(id, status));
        }
        match entry.front.entry_type {
            EntryType::Handoff => {
                return Err(Error::Denied(format!(
                    "{id} is a handoff: a handoff ends with its task, and is not demoted"
                )));
            }
            EntryType::Policy => {
                return Err(Error::Denied(format!(
                    "{id} is a policy entry: a rule is retired through its lifecycle, and is not demoted"
                )));
            }
            _ => {}
        }
        self.may_demote(&entry, reason).await?;
        entry.front.status = Some(Status::Demoted);
        let put = self.put_logged(&entry).await?;
        self.feed_from(&domain, put).await?;
        self.refresh_projection().await;
        let demoted = Change::Demoted {
            id: id.to_string(),
            reason: reason.code().to_string(),
        };
        self.log_duty(&entry, demoted)?;
        Ok(Demotion {
            id: id.to_string(),
            domain,
            reason,
            by: self.author().to_string(),
            digest: dm_core::digest(&entry.render()),
        })
    }

    /// Whether this session may demote the live entry `entry` for `reason`.
    async fn may_demote(&self, entry: &Entry, reason: DemoteReason) -> Result<(), Error> {
        let id = &entry.front.id;
        self.may_curate(entry)?;
        if self.of_a_person() {
            return Ok(());
        }
        if !self.on_duty().await {
            return Err(Error::Denied(format!(
                "an agent demotes an entry only on a task of a curator or of the auditor, and {} holds none",
                self.author()
            )));
        }
        if reason == DemoteReason::Wrong {
            return Err(Error::Denied(format!(
                "a person judges that an entry is wrong. Write the correct entry with `supersedes: {id}`, or leave {id} to a person"
            )));
        }
        // the rule for a dead entry also refuses a verified entry: a person demotes that
        let (entries, events) = self.record_and_logs().await?;
        let since = OffsetDateTime::now_utc() - Duration::days(DEAD_WINDOW_DAYS);
        match alive(entry, &entries, &reads_of(&events, since), since) {
            None => Ok(()),
            Some(why) => Err(Error::Denied(format!(
                "{id} is not dead inside the window of {DEAD_WINDOW_DAYS} days: {}",
                why.why()
            ))),
        }
    }

    /// Restore a demoted entry: a person looked at it and it stands. Its status is
    /// cleared with a new version, through the log and the feed, and retrieval serves it
    /// again. The log of the domain takes a `restored` event after the `put`.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for a session that is not the session of a person, for an entry
    /// that is not demoted, and for one that an active entry replaced.
    /// [`Error::NotFound`] for an entry that this session does not read in the archive.
    /// A store or index error.
    pub async fn restore(&self, id: &str) -> Result<(), Error> {
        if !self.of_a_person() {
            return Err(Error::Denied(
                "only a person restores a demoted entry".into(),
            ));
        }
        let mut f = self.session_filter()?;
        f.include_inactive = true;
        if self.index.lock().await.meta(id, &f)?.is_none() {
            return Err(Error::NotFound(id.to_string()));
        }
        let (domain, mut entry) = self.find(id).await?;
        if entry.front.status != Some(Status::Demoted) {
            return Err(Error::Denied(format!("{id} is not demoted")));
        }
        if let Some(successor) = self.successors(&domain).await?.get(id) {
            return Err(Error::Denied(format!(
                "{id} was replaced by {successor} and stays demoted"
            )));
        }
        entry.front.status = None;
        let put = self.put_logged(&entry).await?;
        self.feed_from(&domain, put).await?;
        self.refresh_projection().await;
        if entry.front.entry_type == EntryType::Policy {
            self.reload_policy().await?;
        }
        self.log_duty(&entry, Change::Restored { id: id.to_string() })?;
        Ok(())
    }

    /// What the record says of a linked entry: `None` when no granted domain holds it,
    /// else its status.
    async fn linked(&self, id: &str) -> Result<Option<Option<Status>>, Error> {
        match self.find(id).await {
            Ok((_, e)) => Ok(Some(e.front.status)),
            Err(Error::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The failures of the link checks on one entry: a link that is broken, and a
    /// citation of an entry that another entry replaced.
    async fn link_failures(
        &self,
        entry: &Entry,
    ) -> Result<Vec<(Check, String, Option<String>)>, Error> {
        let id = &entry.front.id;
        let mut out = Vec::new();
        for (to, cited) in links(entry) {
            let broken = match self.linked(to).await? {
                None => Some("no granted domain holds"),
                Some(Some(Status::Demoted)) => Some("is demoted"),
                Some(Some(Status::Erased)) => Some("is erased"),
                Some(Some(Status::Withheld)) => Some("is withheld"),
                Some(Some(Status::Superseded)) if cited => {
                    let mut wide = self.session_filter()?;
                    wide.include_inactive = true;
                    let current = self.index.lock().await.replaced_by(to, &wide)?;
                    let why = format!("{id} cites {to}, which another entry replaced.");
                    out.push((Check::Superseded, why, current));
                    None
                }
                Some(_) => None,
            };
            if let Some(how) = broken {
                let why = format!("{id} names {to}, which {how}.");
                out.push((Check::CrossLink, why, None));
            }
        }
        Ok(out)
    }

    /// Run the convention checks on the active entries that the read filter of this
    /// session admits, in one domain or in each domain that the session reads. A
    /// handoff and a canary are not checked. Nothing is written, and the log takes no
    /// `read` event.
    ///
    /// - Freshness: a fact has a review date, and no entry has a review date more than
    ///   `overdue_days` before `today`.
    /// - Ownership: a convention and a method name an owner.
    /// - Cross-link: the entry that an entry replaces, and each citation that is an
    ///   entry id, is an entry that a granted domain holds and that is not demoted,
    ///   erased or withheld.
    /// - Superseded: no citation is an entry that another entry replaced. The failure
    ///   names the current version, when the session reads one.
    ///
    /// # Errors
    ///
    /// A domain that the session does not read, or a store or index error.
    pub async fn check_conventions(
        &self,
        domain: Option<&str>,
        today: Date,
        o: &CheckOptions,
    ) -> Result<Vec<Failure>, Error> {
        let f = self.filter_for(domain)?;
        let hits = self.index.lock().await.list(&f, None, o.limit)?;
        let mut out = Vec::new();
        for hit in hits {
            let Some(entry) = self.store.get(&hit.domain, &hit.id).await? else {
                continue;
            };
            if !entry.is_active()
                || entry.front.entry_type == EntryType::Handoff
                || is_canary(&entry)
            {
                continue;
            }
            let mut found: Vec<(Check, String, Option<String>)> = Vec::new();
            found.extend(
                freshness(&entry, today, o.overdue_days).map(|w| (Check::Freshness, w, None)),
            );
            found.extend(ownership(&entry).map(|w| (Check::Ownership, w, None)));
            found.extend(self.link_failures(&entry).await?);
            let owner = entry.front.owner.as_ref().unwrap_or(&self.grants().owner);
            out.extend(found.into_iter().map(|(check, why, names)| Failure {
                check,
                entry: entry.front.id.clone(),
                owner: owner.clone(),
                why,
                names,
                classification: entry.front.classification,
            }));
        }
        Ok(out)
    }

    /// File one task for each failure, in one domain. The same failure is filed once: a
    /// second run finds the task and files nothing. Returns the ids of the tasks filed
    /// now, and the number of failures that had a task already.
    ///
    /// A failure of the checks `freshness`, `ownership` and `cross-link` gets a task
    /// for a person, with the origin `convention:<check>` and priority 2. The body names
    /// the owner. A failure of the check `superseded` gets a task for a curator, with
    /// the origin `curator:rederive` and priority 3: an entry made from a replaced entry
    /// is derived again before each other curation.
    ///
    /// # Errors
    ///
    /// An ungranted domain, or as [`Memory::file_own`].
    pub async fn file_failures(
        &self,
        domain: Option<&str>,
        failures: &[Failure],
    ) -> Result<(Vec<String>, usize), Error> {
        let domain = self.resolve_domain(domain)?;
        let mut filed = Vec::new();
        let mut already = 0;
        for f in failures {
            let created = self.file_own(failure_task(&domain, f)).await?;
            if created.deduplicated {
                already += 1;
            } else {
                filed.push(created.task.id);
            }
        }
        Ok((filed, already))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WriteRequest;
    use std::path::Path;
    use time::macros::{date, datetime};

    const OWNER: &str = "human:test:me";

    fn owner(dir: &Path) -> Memory {
        Memory::init(dir, OWNER, "dom-me").unwrap()
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

    /// An entry as a text editor would write it into the record, written at `created`,
    /// with `fields` among its frontmatter.
    fn aged(id: &str, created: &str, kind: &str, fields: &str) -> Entry {
        Entry::parse(&format!(
            "---\nid: {id}\ntype: {kind}\ndomain: dom-me\nauthor: human:test:me\n\
             created: {created}T09:00:00Z\n{fields}classification: internal\n---\nAbout {id}.\n"
        ))
        .unwrap()
    }

    /// An entry from the year 2020: older than each window.
    fn old(id: &str, kind: &str, fields: &str) -> Entry {
        aged(id, "2020-01-01", kind, fields)
    }

    /// Put entries into the record by hand, and bring the index up to the record.
    async fn planted(m: &Memory, entries: &[Entry]) {
        for e in entries {
            m.store.put(e).await.unwrap();
        }
        m.reindex().await.unwrap();
    }

    /// A task with `origin`, filed by the owner.
    async fn task(o: &Memory, title: &str, origin: Option<&str>, priority: i64) -> String {
        let spec = TaskSpec {
            domain: "dom-me".into(),
            title: title.into(),
            body: "b".into(),
            origin: origin.map(str::to_string),
            upstream_event: origin.map(|_| title.to_string()),
            priority,
            ..TaskSpec::default()
        };
        o.file_own(spec).await.unwrap().task.id
    }

    /// An agent that holds a task with `origin`.
    async fn on_task(dir: &Path, o: &Memory, name: &str, origin: Option<&str>) -> Memory {
        let held = task(o, name, origin, 0).await;
        let a = agent(dir, name);
        a.hold_task(&held).await.unwrap();
        a
    }

    fn denied(r: Result<Demotion, Error>) -> String {
        match r {
            Err(Error::Denied(why)) => why,
            other => panic!("not refused: {other:?}"),
        }
    }

    fn read_at(seq: u64, at: &str, id: &str) -> log::Event {
        log::Event {
            seq,
            at: at.into(),
            by: "agent:r".into(),
            session: Some("sess-r".into()),
            label: Some(Label::Internal),
            quarantined: false,
            outside: false,
            change: Change::Read {
                ids: vec![id.to_string()],
            },
        }
    }

    #[tokio::test]
    async fn a_write_names_a_person_as_owner_and_another_name_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let o = owner(dir.path());
        for bad in ["agent:worker", "team-a", "", "Human:test:x"] {
            let named = WriteRequest {
                owner: Some(bad.into()),
                ..req("A convention.\n")
            };
            let refused = o.write(named).await;
            assert!(
                matches!(refused, Err(Error::Denied(_))),
                "{bad}: {refused:?}"
            );
        }
        let named = WriteRequest {
            owner: Some("human:test:alice".into()),
            ..req("A convention.\n")
        };
        let id = o.write(named).await.unwrap().id;
        let kept = o.get(&id).await.unwrap();
        assert_eq!(kept.front.owner.as_deref(), Some("human:test:alice"));
        let plain = o.write(req("No owner.\n")).await.unwrap().id;
        assert_eq!(o.get(&plain).await.unwrap().front.owner, None);
    }

    #[tokio::test]
    async fn a_write_that_replaces_an_entry_keeps_its_owner_unless_it_names_one() {
        let dir = tempfile::tempdir().unwrap();
        let o = owner(dir.path());
        let first = WriteRequest {
            owner: Some("human:test:alice".into()),
            ..req("First.\n")
        };
        let first = o.write(first).await.unwrap().id;
        let second = WriteRequest {
            supersedes: Some(first),
            ..req("Second.\n")
        };
        let second = o.write(second).await.unwrap().id;
        let kept = o.get(&second).await.unwrap();
        assert_eq!(kept.front.owner.as_deref(), Some("human:test:alice"));
        let third = WriteRequest {
            supersedes: Some(second),
            owner: Some("human:test:bob".into()),
            ..req("Third.\n")
        };
        let third = o.write(third).await.unwrap().id;
        let kept = o.get(&third).await.unwrap();
        assert_eq!(kept.front.owner.as_deref(), Some("human:test:bob"));
    }

    #[test]
    fn an_entry_is_dead_when_it_is_old_unread_unnamed_and_of_a_kind_that_a_demotion_ends() {
        let young = "2020-05-01";
        let entries = [
            old("mem-me-dead", "fact", ""),
            old("mem-me-longago", "fact", ""),
            aged("mem-me-young", young, "fact", ""),
            old("mem-me-read", "fact", ""),
            old("mem-me-badtime", "fact", ""),
            old("mem-me-cited", "fact", ""),
            old("mem-me-inbody", "fact", ""),
            old("mem-me-prev", "fact", ""),
            old("mem-me-ghost", "fact", ""),
            old("mem-me-handoff", "handoff", ""),
            old("mem-me-policy", "policy", ""),
            old("mem-me-verified", "fact", "trust_tier: verified\n"),
            old("mem-me-canary", "fact", "cites:\n- canary\n"),
            old(
                "mem-me-gonenamer",
                "fact",
                "status: demoted\ncites:\n- mem-me-ghost\n",
            ),
            aged("mem-me-citer", young, "fact", "cites:\n- mem-me-cited\n"),
            aged("mem-me-next", young, "fact", "supersedes: mem-me-prev\n"),
            aged(
                "mem-me-inbody-namer",
                young,
                "fact",
                "title: See mem-me-x\n",
            ),
        ];
        let mut entries = entries.to_vec();
        entries[16].body = "See mem-me-inbody for more.\n".into();
        let events = [
            read_at(1, "2020-02-01T00:00:00Z", "mem-me-longago"),
            read_at(2, "2020-01-15T00:00:00Z", "mem-me-longago"),
            read_at(3, "2020-05-20T00:00:00Z", "mem-me-read"),
            read_at(4, "yesterday", "mem-me-badtime"),
        ];
        let now = datetime!(2020-06-01 0:00 UTC);
        let dead = dead_among(&entries, &events, now, Duration::days(90));
        let ids: Vec<&str> = dead.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, ["mem-me-dead", "mem-me-longago", "mem-me-ghost"]);
        assert_eq!(dead[0].last_read, None);
        assert_eq!(dead[1].last_read.as_deref(), Some("2020-02-01T00:00:00Z"));
        assert_eq!(dead[0].created, "2020-01-01T09:00:00Z");
        assert_eq!(dead[0].title, "mem-me-dead");
        // a longer window takes the old read in, and a shorter one lets the young entry out
        let wide = dead_among(&entries, &events, now, Duration::days(130));
        assert!(wide.iter().all(|d| d.id != "mem-me-longago"), "{wide:?}");
        let later = datetime!(2020-12-01 0:00 UTC);
        let after = dead_among(&entries, &events, later, Duration::days(90));
        assert!(after.iter().any(|d| d.id == "mem-me-young"), "{after:?}");
        assert!(after.iter().any(|d| d.id == "mem-me-read"), "{after:?}");
        assert!(after.iter().all(|d| d.id != "mem-me-badtime"), "{after:?}");
    }

    #[tokio::test]
    async fn the_dead_list_marks_no_read_and_a_read_takes_an_entry_off_it() {
        let dir = tempfile::tempdir().unwrap();
        let o = owner(dir.path());
        let tap = "reach: tap\ntask: task-me-x\n";
        planted(
            &o,
            &[
                old("mem-me-dead001", "fact", ""),
                old("mem-me-tap0001", "fact", tap),
            ],
        )
        .await;
        o.write(req("Written now.\n")).await.unwrap();
        let ids = |dead: Vec<Dead>| dead.into_iter().map(|d| d.id).collect::<Vec<_>>();
        let both = ["mem-me-tap0001", "mem-me-dead001"];
        let first = ids(o.dead(None, DEAD_WINDOW_DAYS).await.unwrap());
        assert_eq!(first.len(), 2, "{first:?}");
        assert!(both.iter().all(|id| first.iter().any(|d| d == id)));
        assert_eq!(ids(o.dead(Some("dom-me"), 90).await.unwrap()), first);
        // a session that does not read the entry with tap reach does not get it
        let a = agent(dir.path(), "agent:a");
        let seen = ids(a.dead(None, DEAD_WINDOW_DAYS).await.unwrap());
        assert_eq!(seen, ["mem-me-dead001"]);
        a.get("mem-me-dead001").await.unwrap();
        let after = ids(o.dead(None, DEAD_WINDOW_DAYS).await.unwrap());
        assert_eq!(after, ["mem-me-tap0001"]);
    }

    #[tokio::test]
    async fn a_person_demotes_a_live_entry_for_either_reason_and_the_archive_keeps_it() {
        let dir = tempfile::tempdir().unwrap();
        let o = owner(dir.path());
        for reason in [DemoteReason::Wrong, DemoteReason::Dead] {
            let id = o
                .write(req("# Yak shaving\n\nThe yak is shaved.\n"))
                .await
                .unwrap()
                .id;
            o.get(&id).await.unwrap();
            let done = o.demote(&id, reason).await.unwrap();
            assert_eq!(
                (done.id.as_str(), done.domain.as_str()),
                (id.as_str(), "dom-me")
            );
            assert_eq!((done.reason, done.by.as_str()), (reason, OWNER));
            match o.get(&id).await {
                Err(Error::Gone(why)) => assert!(why.contains("demoted"), "{why}"),
                other => panic!("{other:?}"),
            }
            let found = o.search("yak", None, 10).await.unwrap();
            assert!(found.iter().all(|h| h.id != id), "search");
            let listed = o.list(None, None, 100).await.unwrap();
            assert!(listed.iter().all(|h| h.id != id), "list");
            let kept = o.archived(&id).await.unwrap();
            assert_eq!(kept.front.status, Some(Status::Demoted));
            assert_eq!(kept.body, "# Yak shaving\n\nThe yak is shaved.\n");
            assert_eq!(done.digest, dm_core::digest(&kept.render()));
            let events = log::read(dir.path(), "dom-me", 0).unwrap();
            let at = events
                .iter()
                .position(|e| matches!(&e.change, Change::Demoted { id: d, .. } if *d == id))
                .unwrap();
            let demoted = Change::Demoted {
                id: id.clone(),
                reason: reason.code().into(),
            };
            assert_eq!(events[at].change, demoted);
            assert_eq!(events[at].by, OWNER);
            assert_eq!(events[at].label, Some(kept.front.classification));
            let put = Change::Put {
                id: id.clone(),
                digest: done.digest.clone(),
                active: false,
            };
            assert_eq!(events[at - 1].change, put);
        }
        assert_eq!(
            serde_json::to_string(&DemoteReason::Wrong).unwrap(),
            "\"wrong\""
        );
    }

    #[tokio::test]
    async fn a_demotion_of_an_entry_that_the_session_does_not_read_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let o = owner(dir.path());
        let tap = "reach: tap\ntask: task-me-x\n";
        planted(&o, &[old("mem-me-tap0001", "fact", tap)]).await;
        let a = on_task(dir.path(), &o, "agent:plain", None).await;
        let refused = a.demote("mem-me-tap0001", DemoteReason::Dead).await;
        assert!(matches!(refused, Err(Error::NotFound(_))), "{refused:?}");
        let kept = o
            .store
            .get("dom-me", "mem-me-tap0001")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(kept.front.status, None);
    }

    #[tokio::test]
    async fn a_demotion_of_an_entry_that_is_not_live_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let o = owner(dir.path());
        let id = o.write(req("Demoted once.\n")).await.unwrap().id;
        o.demote(&id, DemoteReason::Wrong).await.unwrap();
        let again = o.demote(&id, DemoteReason::Wrong).await;
        assert!(matches!(again, Err(Error::Gone(_))), "{again:?}");
        // the record is ahead of the index: the index holds the entry as active
        planted(&o, &[old("mem-me-lag0001", "fact", "")]).await;
        let withheld = old("mem-me-lag0001", "fact", "status: withheld\n");
        o.store.put(&withheld).await.unwrap();
        let why = denied(o.demote("mem-me-lag0001", DemoteReason::Wrong).await);
        assert!(why.contains("withheld"), "{why}");
        let events = log::read(dir.path(), "dom-me", 0).unwrap();
        let demotions = events
            .iter()
            .filter(|e| matches!(e.change, Change::Demoted { .. }))
            .count();
        assert_eq!(demotions, 1);
    }

    #[tokio::test]
    async fn a_handoff_and_a_policy_entry_are_not_demoted() {
        let dir = tempfile::tempdir().unwrap();
        let o = owner(dir.path());
        planted(
            &o,
            &[
                old("mem-me-hand001", "handoff", ""),
                old("mem-me-rule001", "policy", ""),
            ],
        )
        .await;
        for (id, word) in [("mem-me-hand001", "handoff"), ("mem-me-rule001", "policy")] {
            let why = denied(o.demote(id, DemoteReason::Wrong).await);
            assert!(why.contains(word), "{why}");
            let kept = o.store.get("dom-me", id).await.unwrap().unwrap();
            assert_eq!(kept.front.status, None, "{id}");
        }
    }

    #[tokio::test]
    async fn an_agent_demotes_only_on_a_task_of_a_curator_or_of_the_auditor() {
        let dir = tempfile::tempdir().unwrap();
        let o = owner(dir.path());
        planted(
            &o,
            &[
                old("mem-me-dead001", "fact", ""),
                old("mem-me-dead002", "fact", ""),
            ],
        )
        .await;
        let idle = agent(dir.path(), "agent:idle");
        let why = denied(idle.demote("mem-me-dead001", DemoteReason::Dead).await);
        assert!(why.contains("holds none"), "{why}");
        let worker = on_task(dir.path(), &o, "agent:worker", None).await;
        let why = denied(worker.demote("mem-me-dead001", DemoteReason::Dead).await);
        assert!(why.contains("holds none"), "{why}");
        let tap = agent(dir.path(), "tap:github");
        assert!(tap
            .demote("mem-me-dead001", DemoteReason::Dead)
            .await
            .is_err());
        let curator = on_task(dir.path(), &o, "agent:curator", Some("curator:survey")).await;
        let done = curator
            .demote("mem-me-dead001", DemoteReason::Dead)
            .await
            .unwrap();
        assert_eq!(done.by, "agent:curator");
        let auditor = on_task(dir.path(), &o, "agent:auditor", Some("auditor:sample")).await;
        auditor
            .demote("mem-me-dead002", DemoteReason::Dead)
            .await
            .unwrap();
        for id in ["mem-me-dead001", "mem-me-dead002"] {
            assert!(matches!(o.get(id).await, Err(Error::Gone(_))), "{id}");
        }
    }

    #[tokio::test]
    async fn an_agent_on_a_curator_s_task_demotes_no_verified_entry_and_not_for_the_reason_wrong() {
        let dir = tempfile::tempdir().unwrap();
        let o = owner(dir.path());
        planted(
            &o,
            &[
                old("mem-me-sure001", "fact", "trust_tier: verified\n"),
                old("mem-me-dead001", "fact", ""),
            ],
        )
        .await;
        let curator = on_task(dir.path(), &o, "agent:curator", Some("curator:survey")).await;
        let why = denied(curator.demote("mem-me-sure001", DemoteReason::Dead).await);
        assert!(why.contains("verified"), "{why}");
        let why = denied(curator.demote("mem-me-dead001", DemoteReason::Wrong).await);
        assert!(why.contains("a person judges"), "{why}");
        // a person demotes both
        o.demote("mem-me-sure001", DemoteReason::Dead)
            .await
            .unwrap();
        o.demote("mem-me-dead001", DemoteReason::Wrong)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn an_agent_on_a_curator_s_task_demotes_no_entry_that_is_read_named_or_young() {
        let dir = tempfile::tempdir().unwrap();
        let o = owner(dir.path());
        planted(
            &o,
            &[
                old("mem-me-read001", "fact", ""),
                old("mem-me-named01", "fact", ""),
            ],
        )
        .await;
        agent(dir.path(), "agent:reader")
            .get("mem-me-read001")
            .await
            .unwrap();
        let citing = WriteRequest {
            cites: vec!["mem-me-named01".into()],
            ..req("It cites an old entry.\n")
        };
        let young = o.write(citing).await.unwrap().id;
        let curator = on_task(dir.path(), &o, "agent:curator", Some("curator:survey")).await;
        for (id, said) in [
            ("mem-me-read001", "it was read inside the window"),
            ("mem-me-named01", "an active entry names it"),
            (young.as_str(), "it was written inside the window"),
        ] {
            let why = denied(curator.demote(id, DemoteReason::Dead).await);
            assert!(why.contains(said), "{id}: {why}");
            let kept = o.store.get("dom-me", id).await.unwrap().unwrap();
            assert_eq!(kept.front.status, None, "{id}");
        }
        // the entry that names the other is demoted by a person: the other is dead then
        o.demote(&young, DemoteReason::Wrong).await.unwrap();
        curator
            .demote("mem-me-named01", DemoteReason::Dead)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn an_entry_with_tap_reach_is_demoted_only_by_a_reader_of_all_raw_entries() {
        let dir = tempfile::tempdir().unwrap();
        let o = owner(dir.path());
        let duty = task(&o, "Survey", Some("curator:survey"), 0).await;
        let tap = format!("reach: tap\ntask: {duty}\n");
        planted(&o, &[old("mem-me-tap0001", "fact", &tap)]).await;
        // a session that has the task in its chain reads the entry, and reads no other
        // raw entry with tap reach
        let member = agent(dir.path(), "agent:member");
        member.set_held_task(&duty);
        let why = denied(member.demote("mem-me-tap0001", DemoteReason::Dead).await);
        assert!(why.contains("tap reach"), "{why}");
        let curator = agent(dir.path(), "agent:curator");
        curator.hold_task(&duty).await.unwrap();
        curator
            .demote("mem-me-tap0001", DemoteReason::Dead)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_person_restores_a_demoted_entry_and_an_agent_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let o = owner(dir.path());
        let id = o.write(req("It stands.\n")).await.unwrap().id;
        let live = o.restore(&id).await;
        assert!(
            matches!(live, Err(Error::Denied(_))),
            "not demoted: {live:?}"
        );
        o.demote(&id, DemoteReason::Wrong).await.unwrap();
        let curator = on_task(dir.path(), &o, "agent:curator", Some("curator:survey")).await;
        let refused = curator.restore(&id).await;
        assert!(matches!(refused, Err(Error::Denied(_))), "{refused:?}");
        assert!(matches!(o.get(&id).await, Err(Error::Gone(_))));
        o.restore(&id).await.unwrap();
        let back = o.get(&id).await.unwrap();
        assert_eq!(
            (back.front.status, back.body.as_str()),
            (None, "It stands.\n")
        );
        let events = log::read(dir.path(), "dom-me", 0).unwrap();
        let at = events
            .iter()
            .position(|e| e.change == Change::Restored { id: id.clone() })
            .unwrap();
        assert_eq!(events[at].by, OWNER);
        assert!(
            matches!(&events[at - 1].change, Change::Put { id: put, active: true, .. } if *put == id)
        );
        // an entry that an active entry replaced stays demoted
        o.demote(&id, DemoteReason::Wrong).await.unwrap();
        let next = WriteRequest {
            supersedes: Some(id.clone()),
            ..req("It stands, corrected.\n")
        };
        let next = o.write(next).await.unwrap().id;
        match o.restore(&id).await {
            Err(Error::Denied(why)) => assert!(why.contains(&next), "{why}"),
            other => panic!("{other:?}"),
        }
    }

    /// The failures of one check, as the entry and what it names.
    fn of(failures: &[Failure], check: Check) -> Vec<(&str, Option<&str>)> {
        failures
            .iter()
            .filter(|f| f.check == check)
            .map(|f| (f.entry.as_str(), f.names.as_deref()))
            .collect()
    }

    const TODAY: Date = date!(2026 - 06 - 01);

    async fn checked(m: &Memory) -> Vec<Failure> {
        let mut found = m
            .check_conventions(None, TODAY, &CheckOptions::default())
            .await
            .unwrap();
        found.sort_by(|a, b| a.entry.cmp(&b.entry));
        found
    }

    #[tokio::test]
    async fn freshness_fails_for_a_fact_with_no_review_date_and_for_an_entry_far_overdue() {
        let dir = tempfile::tempdir().unwrap();
        let o = owner(dir.path());
        planted(
            &o,
            &[
                old("mem-me-f-nodate", "fact", ""),
                old("mem-me-d-nodate", "decision", ""),
                old("mem-me-f-overdue", "fact", "review_by: 2026-05-01\n"),
                old("mem-me-d-overdue", "decision", "review_by: 2026-04-01\n"),
                old("mem-me-f-edge", "fact", "review_by: 2026-05-02\n"),
                old("mem-me-f-due", "fact", "review_by: 2026-05-25\n"),
                old("mem-me-f-fresh", "fact", "review_by: 2026-09-01\n"),
                old("mem-me-h-overdue", "handoff", "review_by: 2026-01-01\n"),
                old(
                    "mem-me-c-overdue",
                    "fact",
                    "cites:\n- canary\nreview_by: 2026-01-01\n",
                ),
                old(
                    "mem-me-x-overdue",
                    "fact",
                    "status: demoted\nreview_by: 2026-01-01\n",
                ),
            ],
        )
        .await;
        let before = log::read(dir.path(), "dom-me", 0).unwrap().len();
        let found = checked(&o).await;
        assert_eq!(
            of(&found, Check::Freshness),
            [
                ("mem-me-d-overdue", None),
                ("mem-me-f-nodate", None),
                ("mem-me-f-overdue", None)
            ]
        );
        assert_eq!(found.len(), 3, "{found:?}");
        assert!(found.iter().all(|f| f.owner == OWNER));
        assert!(found.iter().all(|f| f.classification == Label::Internal));
        // the checks write nothing, and mark no read
        assert_eq!(log::read(dir.path(), "dom-me", 0).unwrap().len(), before);
    }

    #[tokio::test]
    async fn ownership_fails_for_a_convention_or_a_method_with_no_owner() {
        let dir = tempfile::tempdir().unwrap();
        let o = owner(dir.path());
        let alice = "owner: human:test:alice\n";
        planted(
            &o,
            &[
                old("mem-me-conv-none", "convention", ""),
                old("mem-me-meth-none", "method", ""),
                old("mem-me-conv-own", "convention", alice),
                old("mem-me-meth-own", "method", alice),
                old("mem-me-decision", "decision", ""),
                old(
                    "mem-me-far-due",
                    "decision",
                    &format!("{alice}review_by: 2026-01-01\n"),
                ),
            ],
        )
        .await;
        let found = checked(&o).await;
        assert_eq!(
            of(&found, Check::Ownership),
            [("mem-me-conv-none", None), ("mem-me-meth-none", None)]
        );
        assert!(found[0].why.contains("names no owner"), "{}", found[0].why);
        // the failure goes to the owner of the entry, or to the owner of the grants
        let to: Vec<(&str, &str)> = found
            .iter()
            .map(|f| (f.entry.as_str(), f.owner.as_str()))
            .collect();
        assert_eq!(
            to,
            [
                ("mem-me-conv-none", OWNER),
                ("mem-me-far-due", "human:test:alice"),
                ("mem-me-meth-none", OWNER)
            ]
        );
    }

    /// A record with one entry of each state, and entries that name them.
    async fn linked_record(o: &Memory) {
        let citing = |id: &str, to: &str| old(id, "decision", &format!("cites:\n- {to}\n"));
        let others = "cites:\n- https://example.test/mem-me-none\n- file:mem-none.md\n- task-me-none\n- transcript:sess-none#3\n- mem-me-live\n";
        planted(
            o,
            &[
                old("mem-me-live", "decision", ""),
                old("mem-me-demoted", "decision", "status: demoted\n"),
                old("mem-me-erased", "decision", "status: erased\n"),
                old("mem-me-withheld", "decision", "status: withheld\n"),
                old("mem-me-first", "decision", ""),
                old("mem-me-second", "decision", "supersedes: mem-me-first\n"),
                old("mem-me-third", "decision", "supersedes: mem-me-second\n"),
                citing("mem-me-to-none", "mem-me-nowhere"),
                citing("mem-me-to-demoted", "mem-me-demoted"),
                citing("mem-me-to-erased", "mem-me-erased"),
                citing("mem-me-to-withheld", "mem-me-withheld"),
                citing("mem-me-to-first", "mem-me-first"),
                old("mem-me-to-others", "decision", others),
                old(
                    "mem-me-after-none",
                    "decision",
                    "supersedes: mem-me-nowhere\n",
                ),
            ],
        )
        .await;
    }

    #[tokio::test]
    async fn a_link_to_an_entry_that_the_record_does_not_serve_fails_and_other_citations_pass() {
        let dir = tempfile::tempdir().unwrap();
        let o = owner(dir.path());
        linked_record(&o).await;
        let found = checked(&o).await;
        assert_eq!(
            of(&found, Check::CrossLink),
            [
                ("mem-me-after-none", None),
                ("mem-me-to-demoted", None),
                ("mem-me-to-erased", None),
                ("mem-me-to-none", None),
                ("mem-me-to-withheld", None)
            ]
        );
        let why = |entry: &str| &found.iter().find(|f| f.entry == entry).unwrap().why;
        assert!(why("mem-me-to-none").contains("mem-me-nowhere, which no granted domain holds"));
        assert!(why("mem-me-to-demoted").contains("mem-me-demoted, which is demoted"));
        // an entry that cites a replaced entry fails the check `superseded`, and the
        // failure names the end of the chain. The entry that replaced it does not fail
        assert_eq!(
            of(&found, Check::Superseded),
            [("mem-me-to-first", Some("mem-me-third"))]
        );
        assert_eq!(found.len(), 6, "{found:?}");
    }

    #[tokio::test]
    async fn each_failure_is_filed_once_and_a_task_to_derive_again_is_claimed_first() {
        let dir = tempfile::tempdir().unwrap();
        let o = owner(dir.path());
        let verify = task(&o, "Curator: verify", Some("curator:verify"), -1).await;
        let audit = task(&o, "Auditor: check", Some("auditor:summary"), 1).await;
        linked_record(&o).await;
        planted(&o, &[old("mem-me-conv-none", "convention", "")]).await;
        let found = checked(&o).await;
        assert_eq!(found.len(), 7, "{found:?}");
        let (filed, already) = o.file_failures(None, &found).await.unwrap();
        assert_eq!((filed.len(), already), (7, 0));
        let again = o
            .file_failures(Some("dom-me"), &checked(&o).await)
            .await
            .unwrap();
        assert_eq!(again, (Vec::new(), 7));
        let own = spec_of(&o, "convention:ownership").await;
        assert_eq!(own.title, "Convention, ownership: mem-me-conv-none");
        assert_eq!(
            (own.priority, own.requirements),
            (2, vec!["human".to_string()])
        );
        assert!(own
            .body
            .contains(&format!("The owner of `mem-me-conv-none` is {OWNER}.")));
        assert!(own.body.contains("names no owner"), "{}", own.body);
        assert_eq!(own.classification, Label::Internal);
        let again = spec_of(&o, "curator:rederive").await;
        assert_eq!(
            again.title,
            "Curator: derive mem-me-to-first again from mem-me-third"
        );
        assert_eq!((again.priority, again.requirements.len()), (3, 0));
        assert!(
            again.body.contains("`supersedes: mem-me-to-first`"),
            "{}",
            again.body
        );
        let links = tasks_of(&o, "convention:cross-link").await;
        assert_eq!(links.len(), 5);
        let curator = agent(dir.path(), "agent:curator");
        let rederive = tasks_of(&o, "curator:rederive").await.remove(0).id;
        let order = [rederive, audit, verify];
        for want in order {
            let claim = curator
                .claim_next(None, Duration::minutes(5))
                .await
                .unwrap();
            assert_eq!(claim.map(|c| c.task.id), Some(want));
        }
    }

    async fn tasks_of(o: &Memory, origin: &str) -> Vec<dm_ledger::Task> {
        let l = o.ledger.lock().await;
        l.with_origin("dom-me", origin).await.unwrap()
    }

    async fn spec_of(o: &Memory, origin: &str) -> TaskSpec {
        let mut tasks = tasks_of(o, origin).await;
        assert_eq!(tasks.len(), 1, "{origin}");
        tasks.remove(0).spec
    }
}
