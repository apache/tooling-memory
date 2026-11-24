//! Approvals: an action that cannot be taken back runs only under a person's approval of
//! the version that is there when it runs. The approval is an event on the ledger. It
//! names one person, one action, one entry, one version, the principal it is given to
//! and an expiry. The policy checks it each time the action runs.
//!
//! An action without such an approval is refused, and a sign-off task asks the approver
//! for one. The approver signs the task off at the command line, and the requester runs
//! the action again. An approver who runs the action in person signs off with that call:
//! the approval is recorded first and checked as any other. In person means in a session
//! that the memory server does not serve. What a served session asks, an agent may have
//! asked, so such a session signs nothing off, whatever name it carries.
//!
//! Two actions are gated here. An erasure is approved by the owner. A declassification
//! is approved by the guard of the domain: it writes a copy of one version of an entry
//! at a lower label, and the copy cites that version and names the guard. It is the one
//! write that lands below the label of the session that makes it.

use crate::rules::words_on_one_line;
use crate::{reads, Error, Memory, Promotion, PROPOSAL_ORIGIN};
use dm_core::{Entry, EntryType, Frontmatter, Label, TrustTier};
use dm_ledger::{ApprovalRecord, ApprovalSpec, Redaction, Task, TaskSpec};
use dm_policy::decide::{Action, ApprovalEvent, ApprovalInput, Class, Filing, Input, Review};
use dm_policy::Grants;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use time::{Duration, OffsetDateTime};

/// The prefix of the origin of sign-off tasks: `signoff:` and the entry's id.
pub const SIGN_OFF_ORIGIN: &str = "signoff:";

/// How long an approval given through a sign-off task holds, when the approver names no
/// other time.
const APPROVAL_LIFE: Duration = Duration::hours(24);

/// How long the approval holds that an approver gives by running the action in person:
/// long enough for the call that records it, and for nothing later.
const OWN_CALL_LIFE: Duration = Duration::minutes(1);

/// The action of the approval that a promotion to verified records.
const VERIFY: &str = "verify";

/// An action that runs only under an approval.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Gated {
    /// The erasure of an entry.
    Erase,
    /// A copy of an entry at a lower label.
    Declassify {
        /// The label of the copy.
        to: Label,
    },
}

impl Gated {
    /// The action as the approval names it: one token, with the label for a copy, so an
    /// approval of a copy at one label is no approval of a copy at another.
    pub(crate) fn token(self) -> String {
        match self {
            Gated::Erase => "erase".into(),
            Gated::Declassify { to } => format!("declassify:{to}"),
        }
    }

    /// The action as the policy names it.
    pub(crate) fn of(self) -> &'static str {
        match self {
            Gated::Erase => "erase",
            Gated::Declassify { .. } => "declassify",
        }
    }

    /// The action a token names, when it names one.
    pub(crate) fn from_token(token: &str) -> Option<Gated> {
        if token == "erase" {
            return Some(Gated::Erase);
        }
        let to = token.strip_prefix("declassify:")?.parse().ok()?;
        Some(Gated::Declassify { to })
    }

    /// Who approves the action in a domain: the owner for an erasure, the guard of the
    /// domain for a copy at a lower label.
    pub(crate) fn approver(self, grants: &Grants, domain: &str) -> String {
        let guard = match self {
            Gated::Erase => None,
            Gated::Declassify { .. } => grants.guard(domain),
        };
        guard.unwrap_or(&grants.owner).to_string()
    }

    fn action(self) -> Action {
        match self {
            Gated::Erase => Action::Erase,
            Gated::Declassify { .. } => Action::Declassify,
        }
    }

    fn to(self) -> Option<Label> {
        match self {
            Gated::Erase => None,
            Gated::Declassify { to } => Some(to),
        }
    }

    /// What the action does, for the title and the body of a sign-off task.
    fn phrase(self, id: &str) -> String {
        match self {
            Gated::Erase => format!("erasure of {id}"),
            Gated::Declassify { to } => format!("copy of {id} at {to}"),
        }
    }
}

/// What the perimeter said of a gated action.
pub(crate) enum Gate {
    /// An approval of this version is in force for the caller: the action runs under it.
    Open(ApprovalRecord),
    /// No approval is in force. A sign-off task asks the approver for one.
    Asked {
        /// The sign-off task.
        task: Box<Task>,
        /// Why the action did not run.
        reason: String,
    },
}

/// What signing a task off did.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "outcome", rename_all = "lowercase")]
pub enum SignOff {
    /// The task was a proposal to verify, and the entry is at the tier now.
    Promoted {
        /// The entry.
        id: String,
        /// Its tier.
        tier: TrustTier,
    },
    /// The task was a sign-off task, and the ledger holds the approval now.
    Approved {
        /// The approval.
        approval: ApprovalRecord,
    },
}

/// What asking for a copy of an entry at a lower label did.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "outcome", rename_all = "lowercase")]
pub enum Declassification {
    /// The copy is in the record, under an approval of the guard.
    Declassified {
        /// The copy.
        id: String,
        /// The label of the copy.
        classification: Label,
        /// The entry and the version that the copy cites: `<id>@<digest>`.
        cites: String,
        /// The approval that the copy landed under.
        approval: Box<ApprovalRecord>,
    },
    /// No approval is in force. A sign-off task asks the guard, and nothing exists at the
    /// lower label.
    Proposed {
        /// The sign-off task. It is classified as the entry.
        task: Box<Task>,
        /// Why the copy did not land.
        reason: String,
    },
}

/// An approval, and whether it still permits anything.
#[derive(Clone, Debug, Serialize)]
pub struct ApprovalView {
    /// The approval as the ledger holds it.
    #[serde(flatten)]
    pub record: ApprovalRecord,
    /// Whether it is before its expiry on the ledger's clock. An approval in force
    /// permits its action only on the version it names.
    pub in_force: bool,
}

/// One gated action on one version of one entry, as a caller asks for it.
struct Request<'a> {
    action: Gated,
    domain: &'a str,
    entry: &'a Entry,
    version: String,
    approver: String,
}

impl Request<'_> {
    fn target(&self) -> &str {
        &self.entry.front.id
    }

    /// The request as the policy reads it, with the approvals the ledger holds for it.
    fn input(&self, events: Vec<ApprovalEvent>) -> ApprovalInput {
        ApprovalInput {
            of: self.action.of().to_string(),
            target: self.target().to_string(),
            version: self.version.clone(),
            from: Some(self.entry.front.classification),
            to: self.action.to(),
            events,
        }
    }

    /// The approval of this request by `principal` for `scope`, until `expires`.
    fn approval(&self, principal: &str, scope: &str, expires: OffsetDateTime) -> ApprovalSpec {
        ApprovalSpec {
            domain: self.domain.to_string(),
            principal: principal.to_string(),
            action: self.action.token(),
            target: self.target().to_string(),
            version: self.version.clone(),
            scope: scope.to_string(),
            expires,
        }
    }
}

/// Where a copy at a lower label lands, and what it says of where it came from.
struct Landing<'a> {
    /// The domain of the source, and of the copy.
    domain: &'a str,
    /// The label of the copy.
    to: Label,
    /// The source and its version: `<id>@<digest>`.
    cites: String,
    /// The origin of the copy: the label, the source, its version and the guard.
    origin: String,
    /// The id of the copy. It follows from the origin, so each call for one version,
    /// one label and one guard names the same entry, and the record holds one copy.
    id: String,
}

impl<'a> Landing<'a> {
    /// The landing of the copy at `to` of the `version` of `source` that `guard`
    /// approves.
    fn of(domain: &'a str, source: &str, version: &str, to: Label, guard: &str) -> Self {
        let cites = format!("{source}@{version}");
        let origin = format!("declassify:{to}:{cites}:{guard}");
        Landing {
            domain,
            to,
            id: dm_core::id::entry_id_for(domain, &origin),
            cites,
            origin,
        }
    }
}

/// The upstream event of a sign-off task: the action, the version, how many approvals
/// of them the requester had when it asked, and the requester.
fn sign_off_event(action: Gated, version: &str, given: usize, requester: &str) -> String {
    format!("{}@{version}/{given}/{requester}", action.token())
}

/// What an erasure of `id` rewrites the sign-off tasks of the entry to: the id that a
/// stub keeps, and nothing of the reason that a requester gave, which can say what the
/// entry said.
fn sign_off_redaction(id: &str) -> Redaction {
    Redaction {
        title: format!("Approve: {id}"),
        body: format!(
            "`{id}` was erased, and this request is closed with it. Nothing of the entry, nor of why the approval was asked for, is kept here."
        ),
        reason: format!("{id} was erased"),
    }
}

/// The reach and the task of a copy. A copy is raw. The copy of a raw source keeps the
/// reach and the task of the source, so the sessions that read the source while it is
/// raw are the sessions that read the copy: the copy of an entry with tap reach has tap
/// reach. The copy of a curated or a verified source has fleet reach and names no task,
/// because the reach of such a source decides nothing.
fn reach_of_copy(source: &Frontmatter) -> (dm_core::Reach, Option<String>) {
    if source.trust_tier == TrustTier::Raw {
        (source.reach, source.task.clone())
    } else {
        (dm_core::Reach::Fleet, None)
    }
}

/// Why an entry that is not live gets no copy at a lower label.
fn no_copy_of(id: &str, status: dm_core::Status) -> Error {
    use dm_core::Status;
    Error::Denied(match status {
        Status::Erased => format!("{id} is erased; nothing is left to copy"),
        Status::Superseded => {
            format!("{id} was replaced; ask for a copy of its current version instead")
        }
        Status::Demoted => {
            format!("{id} was demoted by curation and gets no copy at a lower label")
        }
        Status::Withheld => {
            format!("{id} is withheld after an erasure and gets no copy at a lower label")
        }
    })
}

/// The action and the version that the upstream event of a sign-off task names.
fn asked_in(event: &str) -> Option<(Gated, &str)> {
    let (token, rest) = event.split_once('@')?;
    let version = rest.split('/').next().filter(|v| !v.is_empty())?;
    Some((Gated::from_token(token)?, version))
}

impl Memory {
    /// Ask the policy about one input of an approval.
    fn approval_question(&self, action: Action, domain: &str, approval: ApprovalInput) -> Input {
        Input {
            action,
            principal: self.principal(),
            domain: domain.to_string(),
            filing: None,
            task: None,
            parent: None,
            write: None,
            promote: None,
            held_task: None,
            spend: None,
            fetch: None,
            approval: Some(approval),
        }
    }

    /// Whether a gated action on `entry` may run now for this session. The approvals
    /// are read from the ledger and the policy checks them: one must be of this version,
    /// given by the approver to this principal, and not expired on the ledger's clock.
    /// When none is, a sign-off task asks the approver, once for each version and each
    /// approval that ran out. The approver's own call in person is recorded as an
    /// approval first.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] when the policy refuses for another cause than a missing
    /// approval, or when the approver declined this request for this version; a ledger
    /// or policy error.
    pub(crate) async fn approval_for(
        &self,
        action: Gated,
        domain: &str,
        entry: &Entry,
        justification: Option<&str>,
    ) -> Result<Gate, Error> {
        let request = Request {
            action,
            domain,
            entry,
            version: dm_core::digest(&entry.render()),
            approver: action.approver(&self.grants, domain),
        };
        let (events, held) = self.approvals_now(&request).await?;
        let d =
            self.decide(&self.approval_question(action.action(), domain, request.input(events)))?;
        if d.allow {
            // the policy of a root can allow more than the base rule; without an approval
            // on the ledger the action still does not run
            return held.map(Gate::Open).ok_or_else(|| {
                Error::Denied(format!(
                    "the ledger holds no approval of {} for this version of {}",
                    request.approver,
                    request.target()
                ))
            });
        }
        if d.review != Review::Approver || !d.denials.is_empty() {
            return Err(Error::Denied(d.reason));
        }
        let task = self.ask_for_approval(&request, justification).await?;
        Ok(Gate::Asked {
            task: Box::new(task),
            reason: d.reason,
        })
    }

    /// The approvals of the request's action on its entry, as the policy reads them, and
    /// the newest one that holds for this session now. The approver's own call in person
    /// is recorded first, so it is among them.
    async fn approvals_now(
        &self,
        request: &Request<'_>,
    ) -> Result<(Vec<ApprovalEvent>, Option<ApprovalRecord>), Error> {
        let mut l = self.ledger.lock().await;
        if self.author == request.approver && !self.served() {
            let expires = l.now() + OWN_CALL_LIFE;
            l.record_approval(&request.approval(&self.author, &self.author, expires))
                .await?;
        }
        let token = request.action.token();
        let now = l.now();
        let mut events = Vec::new();
        let mut held = None;
        for record in l.approvals(request.domain, Some(request.target())).await? {
            if record.spec.action != token {
                continue;
            }
            let expired = record.expired(now);
            events.push(ApprovalEvent {
                id: record.id.clone(),
                principal: record.spec.principal.clone(),
                class: Class::of(&record.spec.principal, &self.grants.owner),
                version: record.spec.version.clone(),
                scope: record.spec.scope.clone(),
                expired,
            });
            let holds = !expired
                && record.spec.version == request.version
                && record.spec.scope == self.author
                && record.spec.principal == request.approver;
            if holds {
                held = Some(record);
            }
        }
        Ok((events, held))
    }

    /// File the sign-off task of a request, or find the one already filed. The task
    /// names the version and how many approvals of it this requester had, so a request
    /// is made again when the entry changes or an approval runs out, and a request that
    /// the approver declined is not made again for the same version.
    async fn ask_for_approval(
        &self,
        request: &Request<'_>,
        justification: Option<&str>,
    ) -> Result<Task, Error> {
        let mut l = self.ledger.lock().await;
        let token = request.action.token();
        let given = l
            .approvals(request.domain, Some(request.target()))
            .await?
            .iter()
            .filter(|a| {
                a.spec.action == token
                    && a.spec.version == request.version
                    && a.spec.scope == self.author
            })
            .count();
        let spec = self.sign_off_task(request, given, justification);
        let task = self.file_in(&mut l, spec, Filing::Own, None).await?.task;
        if task.status.is_terminal() {
            let how = match task.status {
                dm_ledger::Status::Invalidated => "was declined",
                _ => "is closed",
            };
            return Err(Error::Denied(format!(
                "task {} asked {} for this approval and {how}. The request is not made again until the entry changes",
                task.id, request.approver
            )));
        }
        Ok(task)
    }

    /// The sign-off task of a request. Its title and its body name the entry by id. The
    /// task of a copy also holds the requester's reason, which an erasure of the entry
    /// takes out again ([`Memory::scrub_sign_offs`]).
    fn sign_off_task(
        &self,
        request: &Request<'_>,
        given: usize,
        justification: Option<&str>,
    ) -> TaskSpec {
        let id = request.target();
        let (who, version) = (&self.author, &request.version);
        let what = request.action.phrase(id);
        let weight = match request.action {
            Gated::Erase => "An erasure cannot be taken back.".to_string(),
            Gated::Declassify { to } => format!(
                "A copy at {to} can be read where `{id}` cannot, and what was read cannot be taken back.\n\nWhy: {}",
                justification.unwrap_or("no reason was given")
            ),
        };
        TaskSpec {
            domain: request.domain.to_string(),
            title: format!("Approve: {what}"),
            body: format!(
                "`{who}` asks for an approval: the {what}, version `{version}`.\n\nRead the entry with `amem get {id}` and judge it first. {weight}\n\n`amem approve <this task>` approves this version for `{who}` and for nobody else. The approval expires after 24 hours, or at the time that the command names. Invalidate this task to decline."
            ),
            requirements: vec!["human".into()],
            origin: Some(format!("{SIGN_OFF_ORIGIN}{id}")),
            upstream_event: Some(sign_off_event(request.action, version, given, who)),
            classification: request.entry.front.classification,
            priority: 3,
            ..TaskSpec::default()
        }
    }

    /// Whether an erasure of `entry` may run now for this session; see
    /// [`Memory::approval_for`]. Only the owner in person asks about an entry that the
    /// session's filter does not admit: every other session gets the answer that a
    /// missing entry gets.
    ///
    /// A ledger that does not answer keeps no approval. The owner's erasure in person
    /// then runs under the rule that an erasure is the owner's alone, so the text leaves
    /// the record and the index now. The last step of an erasure is the ledger's, so the
    /// erasure stays unfinished, and the run that finishes it records its approval.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for an entry the session may not read; [`Error::Denied`]
    /// without an approval in force, with the sign-off task that asks for one; a ledger
    /// error for every session but the owner's in person.
    pub(crate) async fn erasure_approved(&self, domain: &str, entry: &Entry) -> Result<(), Error> {
        let id = &entry.front.id;
        let in_person = self.author == self.grants.owner && !self.served();
        if !in_person && !self.may_read(domain, entry)? {
            return Err(Error::NotFound(id.clone()));
        }
        match self.approval_for(Gated::Erase, domain, entry, None).await {
            Ok(Gate::Open(approval)) => {
                self.ran_under(&approval);
                Ok(())
            }
            Ok(Gate::Asked { task, reason }) => Err(Error::Denied(format!(
                "{reason}. Task {} asks {} for it.",
                task.id,
                Gated::Erase.approver(&self.grants, domain)
            ))),
            Err(Error::Ledger(_)) if in_person => self.owner_erases_alone(domain),
            Err(e) => Err(e),
        }
    }

    /// The policy's answer to an erasure asked without an approval: the owner's alone.
    fn owner_erases_alone(&self, domain: &str) -> Result<(), Error> {
        let mut question = self.approval_question(Action::Erase, domain, ApprovalInput::default());
        question.approval = None;
        let d = self.decide(&question)?;
        if d.allow {
            Ok(())
        } else {
            Err(Error::Denied(d.reason))
        }
    }

    /// Whether the session's filter admits an entry, whatever its status. A raw entry is
    /// judged by its reach and its task, as each read judges it. The record is read
    /// here, not the index, so the snapshot of the session is asked too.
    fn may_read(&self, domain: &str, entry: &Entry) -> Result<bool, Error> {
        let front = &entry.front;
        let f = self.session_filter()?;
        let admitted = f.admits_entry(
            domain,
            front.classification,
            front.trust_tier,
            true,
            front.reach,
            front.task.as_deref(),
        );
        Ok(admitted && f.admits_at(front.created))
    }

    /// With a collector, a span that names the approval an action ran under.
    fn ran_under(&self, approval: &ApprovalRecord) {
        if dm_otel::enabled() {
            dm_otel::event(
                "approval.used",
                &[
                    ("memory.session", self.session_id.as_str().into()),
                    ("memory.domain", approval.spec.domain.as_str().into()),
                    ("approval.id", approval.id.as_str().into()),
                    ("approval.action", approval.spec.action.as_str().into()),
                    ("approval.target", approval.spec.target.as_str().into()),
                ],
            );
        }
    }

    /// Copy an entry to a lower label: a declassification. The copy is a new raw entry
    /// with the text and the author of the source. It cites the version it was made
    /// from, and its origin names the guard who approved it. The source stays as it is.
    ///
    /// The copy lands only under an approval of the guard of the domain for this version
    /// and this label, given to the caller and not expired. The guard's own call, in a
    /// session that the memory server does not serve, is that approval. For any other
    /// caller that may read the entry, a sign-off task asks the guard, and the answer is
    /// [`Declassification::Proposed`]. The task is classified as the entry and holds the
    /// justification. After the guard signs it off, the same call lands the copy. A call
    /// made again for a copy that landed gives that copy and writes nothing. Two calls at
    /// the same time land one copy, since the id of a copy follows from its origin. A
    /// copy that was erased does not land again, and its request is refused before the
    /// guard is asked.
    ///
    /// To ask is to read the entry: the session's label rises to the entry's label, and
    /// the log of the domain takes a read event. The
    /// copy is the one write that lands below the label of its session. The body is text
    /// that the record holds already, so the scan does not run again.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for a justification that is empty or is not words on one line,
    /// a label that is not lower than the entry's, an entry that is not live in the
    /// record, a copy of this version at this label that was erased, a request that the
    /// guard declined, or a refusal of the policy;
    /// [`Error::NotFound`] for an entry that this session cannot read; [`Error::Moved`]
    /// or [`Error::Gone`], as from [`Memory::get`], for one it could read that is no
    /// longer served; a store, index or ledger error.
    pub async fn declassify(
        &self,
        id: &str,
        to: Label,
        justification: &str,
    ) -> Result<Declassification, Error> {
        let why = justification.trim();
        if why.is_empty() || !words_on_one_line(why) {
            return Err(Error::Denied(
                "a declassification says why, in words on one line, with no code fence and no markup"
                    .into(),
            ));
        }
        let filter = self.session_filter()?;
        if self.index.lock().await.meta(id, &filter)?.is_none() {
            return Err(self.not_served(id, None, &filter).await);
        }
        let (domain, entry) = self.find(id).await?;
        // the index can lag the record: the record says whether the entry is live
        if let Some(status) = entry.front.status {
            return Err(no_copy_of(id, status));
        }
        let from = entry.front.classification;
        // before the approval: the guard's own call records one, and a copy that cannot
        // be must leave none
        if to >= from {
            return Err(Error::Denied(format!(
                "{id} is {from}, and {to} is not lower; a declassification copies an entry to a lower label"
            )));
        }
        // to ask is to read the entry: the log of its domain takes the read
        let outside = crate::reach::outside_content(&entry);
        if !self.observe_entry(&domain, id, from, outside)? {
            return Err(Error::NotFound(id.to_string()));
        }
        let action = Gated::Declassify { to };
        let version = dm_core::digest(&entry.render());
        let guard = action.approver(&self.grants, &domain);
        let landing = Landing::of(&domain, &entry.front.id, &version, to, &guard);
        // before the approval, as the label is: a copy that cannot land asks the guard
        // for nothing, and the guard's own call leaves no approval
        self.copy_was_not_erased(id, &landing).await?;
        match self
            .approval_for(action, &domain, &entry, Some(why))
            .await?
        {
            Gate::Asked { task, reason } => Ok(Declassification::Proposed { task, reason }),
            Gate::Open(approval) => {
                self.ran_under(&approval);
                self.land_copy(&entry, landing, approval).await
            }
        }
    }

    /// Refuse a copy that was erased, or whose erasure began: the receipt of an erasure
    /// is written first. The copy of one version at one label has one id, and an erased
    /// entry is not written again, so nothing can land.
    async fn copy_was_not_erased(&self, source: &str, at: &Landing<'_>) -> Result<(), Error> {
        if self.store.receipt(at.domain, &at.id).await?.is_none() {
            return Ok(());
        }
        Err(Error::Denied(format!(
            "{} is the copy of this version of {source} at {}, and it was erased. An erased entry is not written again: a copy lands at {} again when {source} has a new version",
            at.id, at.to, at.to
        )))
    }

    /// The copy of `source` that `approval` permits: the one that is in the record, or a
    /// new one. The copy of one version at one label under one guard has one id, so a
    /// call made again finds it in the record, whatever became of it since, and writes
    /// nothing. The record says whether the id is taken, and no text is read: a copy
    /// that another call is writing now is there too.
    async fn land_copy(
        &self,
        source: &Entry,
        at: Landing<'_>,
        approval: ApprovalRecord,
    ) -> Result<Declassification, Error> {
        if !self.store.exists(at.domain, &at.id).await? {
            self.write_copy(source, &at).await?;
        }
        Ok(Declassification::Declassified {
            id: at.id,
            classification: at.to,
            cites: at.cites,
            approval: Box::new(approval),
        })
    }

    /// Write the copy of `source`, as [`Memory::write`] writes an entry, with three
    /// differences. The classification is the label asked for and is not raised to the
    /// session's label. The author, the date, the type, the text and the review date are
    /// the source's. The copy cites the source's version and nothing else: what the
    /// source cites can be above the copy's label. The reach and the task are those of
    /// a raw source, and not what this session read.
    ///
    /// The third difference: the record takes the copy only when it holds no entry with
    /// this id. Every part of the copy follows from the source's version, the label and
    /// the guard, so two calls at the same time write the same text under the same id.
    /// One of them writes it, and the other one stops here: the copy is the first one's
    /// to index.
    async fn write_copy(&self, source: &Entry, at: &Landing<'_>) -> Result<(), Error> {
        let from = &source.front;
        let (reach, task) = reach_of_copy(from);
        let copy = Entry {
            front: Frontmatter {
                id: at.id.clone(),
                entry_type: from.entry_type,
                domain: at.domain.to_string(),
                author: from.author.clone(),
                created: from.created,
                cites: vec![at.cites.clone()],
                // only a write gives an entry its session: the text of a copy follows
                // from its origin alone, whichever session lands it
                session: None,
                // a copy is raw, so the reach of a raw source is the reach of its copy:
                // the copy of content from outside the fleet stays with the task chain
                // and the curators
                reach,
                task,
                confidence: from.confidence,
                trust_tier: TrustTier::Raw,
                review_by: from.review_by,
                supersedes: None,
                policy_context: Vec::new(),
                classification: at.to,
                status: None,
                origin: Some(at.origin.clone()),
                summary: from.summary.clone(),
                title: from.title.clone(),
            },
            body: source.body.clone(),
        };
        let Some(put) = self.create_logged(&copy).await? else {
            return Ok(());
        };
        self.feed_from(&copy.front.domain, put).await?;
        // the log of writes names who made the write: this session, not the author
        reads::wrote(
            &self.root,
            &self.session_id,
            &copy.front.id,
            &self.author,
            None,
        );
        self.refresh_projection().await;
        if copy.front.entry_type == EntryType::Policy {
            self.reload_policy().await?;
        }
        self.written.0.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// For an erasure of `id`: rewrite the sign-off tasks of the entry so that they keep
    /// its id and nothing a requester wrote, in their events too, and invalidate a
    /// request that is still open.
    ///
    /// # Errors
    ///
    /// A ledger error; [`dm_ledger::Error::Held`] when another connection holds the log
    /// that the old text is in.
    pub(crate) async fn scrub_sign_offs(&self, domain: &str, id: &str) -> Result<(), Error> {
        let origin = format!("{SIGN_OFF_ORIGIN}{id}");
        let mut l = self.ledger.lock().await;
        l.scrub_tasks(domain, &origin, &sign_off_redaction(id))
            .await?;
        Ok(())
    }

    /// A session that the memory server serves signs nothing off: a harness reaches it,
    /// so an agent may have asked.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for such a session.
    pub(crate) fn may_sign_off(&self) -> Result<(), Error> {
        if self.served() {
            return Err(Error::Denied(
                "a session of the memory server approves nothing: a person approves at the command line"
                    .into(),
            ));
        }
        Ok(())
    }

    /// Whether this session may invalidate `task`. To invalidate a sign-off task is to
    /// decline its request: the requester is told that the approver declined, and the
    /// request is not made again for that version. So only who may approve the request
    /// closes its task: the policy's approver, in a session that the memory server does
    /// not serve. Every other task passes, and so does a sign-off task that is closed,
    /// which the ledger refuses for all alike.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for a session that the memory server serves, and for a caller
    /// that the policy refuses as the approver of the request; a policy error.
    pub(crate) fn may_decline(&self, task: &Task) -> Result<(), Error> {
        let origin = task.spec.origin.as_deref().unwrap_or_default();
        let Some(target) = origin.strip_prefix(SIGN_OFF_ORIGIN) else {
            return Ok(());
        };
        if task.status.is_terminal() {
            return Ok(());
        }
        if self.served() {
            return Err(Error::Denied(
                "a session of the memory server declines nothing: the approver invalidates a sign-off task at the command line"
                    .into(),
            ));
        }
        let asked = task.spec.upstream_event.as_deref().and_then(asked_in);
        let Some((action, version)) = asked else {
            return Err(Error::Denied(format!(
                "{} names no action and version to decline",
                task.id
            )));
        };
        let input = ApprovalInput {
            of: action.of().to_string(),
            target: target.to_string(),
            version: version.to_string(),
            to: action.to(),
            ..ApprovalInput::default()
        };
        let question = self.approval_question(Action::Approve, &task.spec.domain, input);
        let d = self.decide(&question)?;
        if d.allow {
            return Ok(());
        }
        Err(Error::Denied(format!(
            "{} is a sign-off task, and only its approver declines it: {}",
            task.id, d.reason
        )))
    }

    /// Record the approval that a promotion to verified is: the approver approved this
    /// version, in the call that moves the tier.
    ///
    /// # Errors
    ///
    /// A ledger error.
    pub(crate) async fn record_verify_approval(&self, entry: &Entry) -> Result<(), Error> {
        let mut l = self.ledger.lock().await;
        let spec = ApprovalSpec {
            domain: entry.front.domain.clone(),
            principal: self.author.clone(),
            action: VERIFY.into(),
            target: entry.front.id.clone(),
            version: dm_core::digest(&entry.render()),
            scope: self.author.clone(),
            expires: l.now() + OWN_CALL_LIFE,
        };
        l.record_approval(&spec).await?;
        Ok(())
    }

    /// Sign a task off: approve what it asks a person for. A proposal to verify an entry
    /// is approved as [`Memory::approve`] does. A sign-off task records an approval of
    /// the action and the version that the task names, for the principal that asked,
    /// until `expires` or for 24 hours. The action does not run here: the requester runs
    /// it again, and the policy checks the approval then.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for a session that the memory server serves, a task that is
    /// neither, a task that is closed, an entry that changed since the request, a caller
    /// the policy refuses as approver, or an expiry that is not after now; a ledger error
    /// for a task another holds or one outside the grants; [`Error::NotFound`] for an
    /// entry that is gone.
    pub async fn sign_off(
        &self,
        task_id: &str,
        expires: Option<OffsetDateTime>,
    ) -> Result<SignOff, Error> {
        self.may_sign_off()?;
        let task = self.ledger.lock().await.get(task_id).await?;
        self.check_task(&task)?;
        let origin = task.spec.origin.as_deref().unwrap_or_default();
        if origin.starts_with(PROPOSAL_ORIGIN) {
            return match self.approve(task_id).await? {
                Promotion::Promoted { id, tier } => Ok(SignOff::Promoted { id, tier }),
                Promotion::Proposed { task } => Err(Error::Denied(format!(
                    "{} is proposed and not approved",
                    task.id
                ))),
            };
        }
        let Some(target) = origin.strip_prefix(SIGN_OFF_ORIGIN) else {
            return Err(Error::Denied(format!(
                "{task_id} is neither a proposal to verify nor a sign-off task"
            )));
        };
        let spec = self.approval_asked(&task, target, expires).await?;
        let by = self.claim_proposal(&task).await?;
        let mut l = self.ledger.lock().await;
        let approval = match l.record_approval(&spec).await {
            Ok(a) => a,
            Err(e) => {
                l.release(task_id, self.under(by), None).await?;
                return Err(e.into());
            }
        };
        l.complete(task_id, self.under(by), target).await?;
        Ok(SignOff::Approved { approval })
    }

    /// The approval that a sign-off task asks for, as this session may give it now: the
    /// task is open, the entry is the version the requester asked about, the policy
    /// takes this session as the approver, and the expiry is after now.
    async fn approval_asked(
        &self,
        task: &Task,
        target: &str,
        expires: Option<OffsetDateTime>,
    ) -> Result<ApprovalSpec, Error> {
        let asked = task.spec.upstream_event.as_deref().and_then(asked_in);
        let Some((action, version)) = asked else {
            return Err(Error::Denied(format!(
                "{} names no action and version to approve",
                task.id
            )));
        };
        if task.status == dm_ledger::Status::Done {
            return Err(Error::Denied(format!(
                "{} is Completed: the request was approved already",
                task.id
            )));
        }
        if task.status.is_terminal() {
            return Err(Error::Denied(format!("{} is {:?}", task.id, task.status)));
        }
        let (domain, entry) = self.find(target).await?;
        if domain != task.spec.domain {
            return Err(Error::Denied(format!(
                "{} was filed in {}, and {target} is in {domain}",
                task.id, task.spec.domain
            )));
        }
        let request = Request {
            action,
            domain: &domain,
            entry: &entry,
            version: dm_core::digest(&entry.render()),
            approver: action.approver(&self.grants, &domain),
        };
        if request.version != version {
            return Err(Error::Denied(format!(
                "{target} has changed since {} asked about it: the approval would be of a version nobody asked about. The requester asks again for the entry as it stands",
                task.id
            )));
        }
        let question = self.approval_question(Action::Approve, &domain, request.input(Vec::new()));
        let d = self.decide(&question)?;
        if !d.allow {
            return Err(Error::Denied(d.reason));
        }
        let now = self.ledger.lock().await.now();
        let expires = expires.unwrap_or(now + APPROVAL_LIFE);
        if expires <= now {
            return Err(Error::Denied(format!(
                "an approval that expires at {expires} permits nothing: name a time after now"
            )));
        }
        Ok(request.approval(&self.author, &task.spec.created_by, expires))
    }

    /// The approvals of a domain, or of one entry of it, oldest first, each with whether
    /// it is still before its expiry. Only the approvals of entries that the session's
    /// filter admits are given, replaced, demoted and erased entries included. An
    /// approval holds ids and a digest and nothing of the entry's text, so the session's
    /// label does not rise.
    ///
    /// # Errors
    ///
    /// An ungranted domain or one outside the session's read scope; a ledger or index
    /// error.
    pub async fn approvals(
        &self,
        domain: Option<&str>,
        target: Option<&str>,
    ) -> Result<Vec<ApprovalView>, Error> {
        let domain = self.resolve_domain(domain)?;
        let mut filter = self.filter_for(Some(&domain))?;
        filter.include_inactive = true;
        let (records, now) = {
            let l = self.ledger.lock().await;
            (l.approvals(&domain, target).await?, l.now())
        };
        let ix = self.index.lock().await;
        let mut admitted: HashMap<String, bool> = HashMap::new();
        let mut out = Vec::new();
        for record in records {
            let id = &record.spec.target;
            let readable = match admitted.get(id) {
                Some(known) => *known,
                None => {
                    let found = ix.meta(id, &filter)?.is_some();
                    admitted.insert(id.clone(), found);
                    found
                }
            };
            if readable {
                out.push(ApprovalView {
                    in_force: !record.expired(now),
                    record,
                });
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::McpServer;
    use crate::WriteRequest;
    use dm_core::erasure::Reason;
    use dm_ledger::Status;
    use std::path::Path;
    use std::sync::Arc;

    const OWNER: &str = "human:test:me";
    const AGENT: &str = "agent:x";
    const DOMAIN: &str = "dom-me";
    const TITLE: &str = "Door code";

    fn owner(dir: &Path) -> Memory {
        Memory::init(dir, OWNER, DOMAIN).unwrap()
    }

    /// A session of another principal on the same root, in person.
    fn session(dir: &Path, who: &str) -> Memory {
        Memory::open(dir).unwrap().with_author(who)
    }

    /// A session of the owner's name that the memory server serves.
    fn served(m: Memory) -> Arc<Memory> {
        let m = Arc::new(m);
        let _server = McpServer::new(Arc::clone(&m));
        m
    }

    /// An entry of the owner, internal, with a title that is not its id.
    async fn entry(m: &Memory, body: &str) -> String {
        let req = WriteRequest {
            title: Some(TITLE.into()),
            body: body.into(),
            ..WriteRequest::default()
        };
        m.write(req).await.unwrap().id
    }

    async fn stored(m: &Memory, id: &str) -> Entry {
        m.store.get(DOMAIN, id).await.unwrap().unwrap()
    }

    async fn version(m: &Memory, id: &str) -> String {
        dm_core::digest(&stored(m, id).await.render())
    }

    async fn at(m: &Memory, now: OffsetDateTime) {
        m.ledger.lock().await.set_clock(move || now);
    }

    /// Every sign-off task of an entry, oldest first, whatever its status.
    async fn sign_offs(m: &Memory, id: &str) -> Vec<Task> {
        let origin = format!("{SIGN_OFF_ORIGIN}{id}");
        let l = m.ledger.lock().await;
        l.with_origin(DOMAIN, &origin).await.unwrap()
    }

    /// Every approval on the ledger, as the ledger holds it.
    async fn on_ledger(m: &Memory) -> Vec<ApprovalRecord> {
        m.ledger.lock().await.approvals(DOMAIN, None).await.unwrap()
    }

    /// The refusal of an answer, which must say `says`.
    fn denied<T: std::fmt::Debug>(answer: Result<T, Error>, says: &str) -> String {
        match answer {
            Err(Error::Denied(why)) if why.contains(says) => why,
            other => panic!("expected a refusal that says {says:?}: {other:?}"),
        }
    }

    /// An erasure that is refused, and the sign-off task that its refusal names.
    async fn asked(m: &Memory, id: &str, says: &str) -> Task {
        let why = denied(m.erase(id, Reason::Mistaken).await, says);
        let named = why
            .split_once("Task ")
            .and_then(|(_, rest)| rest.split_once(" asks "))
            .map(|(task, _)| task.to_string());
        let Some(task) = named else {
            panic!("the refusal names no task: {why}")
        };
        assert!(why.ends_with(&format!("asks {OWNER} for it.")), "{why}");
        m.ledger.lock().await.get(&task).await.unwrap()
    }

    async fn approved(m: &Memory, task: &Task, expires: Option<OffsetDateTime>) -> ApprovalRecord {
        match m.sign_off(&task.id, expires).await.unwrap() {
            SignOff::Approved { approval } => approval,
            other @ SignOff::Promoted { .. } => panic!("approved: {other:?}"),
        }
    }

    /// Another principal promotes the entry to curated: the entry has a new version.
    async fn changed(dir: &Path, id: &str) {
        session(dir, "agent:curator")
            .promote(id, TrustTier::Curated, "checked against the door")
            .await
            .unwrap();
    }

    #[test]
    fn a_gated_action_is_one_token_and_reads_back() {
        let copy = Gated::Declassify {
            to: Label::Internal,
        };
        assert_eq!(Gated::Erase.token(), "erase");
        assert_eq!(copy.token(), "declassify:internal");
        assert_eq!((Gated::Erase.of(), copy.of()), ("erase", "declassify"));
        for action in [Gated::Erase, copy] {
            assert_eq!(Gated::from_token(&action.token()), Some(action));
        }
        for no_action in [
            "",
            "verify",
            "declassify",
            "declassify:",
            "declassify:secret",
        ] {
            assert_eq!(Gated::from_token(no_action), None, "{no_action}");
        }
    }

    #[test]
    fn the_event_of_a_sign_off_task_names_the_action_and_the_version() {
        let copy = Gated::Declassify { to: Label::Public };
        let event = sign_off_event(copy, "abc123", 2, "agent:x (model: a/b@c)");
        assert_eq!(event, "declassify:public@abc123/2/agent:x (model: a/b@c)");
        assert_eq!(asked_in(&event), Some((copy, "abc123")));
        for no_request in [
            "",
            "erase",
            "erase@",
            "erase@/0/agent:x",
            "verify@abc/0/agent:x",
        ] {
            assert_eq!(asked_in(no_request), None, "{no_request}");
        }
    }

    #[test]
    fn the_approver_of_an_erasure_is_the_owner_and_of_a_copy_the_guard() {
        let text = format!(
            "owner = \"{OWNER}\"\n[domains.dom-a]\nguard = \"human:test:guard\"\n[domains.dom-b]\n"
        );
        let g = Grants::parse(&text).unwrap();
        let copy = Gated::Declassify { to: Label::Public };
        assert_eq!(Gated::Erase.approver(&g, "dom-a"), OWNER);
        assert_eq!(copy.approver(&g, "dom-a"), "human:test:guard");
        assert_eq!(copy.approver(&g, "dom-b"), OWNER, "no guard is named");
    }

    #[test]
    fn signing_a_proposal_off_answers_as_approving_it_does() {
        let (id, tier) = ("mem-me-1".to_string(), TrustTier::Verified);
        let new = serde_json::to_value(SignOff::Promoted {
            id: id.clone(),
            tier,
        });
        let old = serde_json::to_value(Promotion::Promoted { id, tier });
        assert_eq!(new.unwrap(), old.unwrap());
    }

    /// The line of the decision log for the erasure of `id` names the approval asked
    /// about: the action, the entry and the version.
    fn the_decision_names_what_was_asked(dir: &Path, id: &str, version: &str) {
        let log = std::fs::read_to_string(dir.join("state/decisions.jsonl")).unwrap();
        let asked = serde_json::json!({ "of": "erase", "target": id, "version": version });
        let named = log
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .any(|line| line["action"] == "erase" && line["approval"] == asked);
        assert!(named, "{log}");
    }

    #[tokio::test]
    async fn the_owner_s_erasure_in_person_is_its_own_approval() {
        let dir = tempfile::tempdir().unwrap();
        let m = owner(dir.path());
        let id = entry(&m, "The door code is 4417.").await;
        let before = version(&m, &id).await;
        m.erase(&id, Reason::Mistaken).await.unwrap();
        let erased = stored(&m, &id).await.front.status;
        assert_eq!(erased, Some(dm_core::Status::Erased));
        // the entry is erased, and its approval is still listed
        let listed = m.approvals(None, Some(&id)).await.unwrap();
        assert_eq!(listed.len(), 1, "{listed:?}");
        let a = &listed[0].record.spec;
        assert_eq!((a.principal.as_str(), a.scope.as_str()), (OWNER, OWNER));
        assert_eq!(
            (a.action.as_str(), a.target.as_str()),
            ("erase", id.as_str())
        );
        assert_eq!(a.version, before, "the version before the erasure");
        assert!(listed[0].in_force, "it holds for one minute");
        the_decision_names_what_was_asked(dir.path(), &id, &before);
    }

    /// The sign-off task of an erasure: for a person, classified as the entry, filed by
    /// the requester, and with the entry's id and nothing else of the entry.
    fn is_the_sign_off_task(task: &Task, id: &str, classification: Label) {
        let spec = &task.spec;
        assert_eq!(
            spec.origin.as_deref(),
            Some(format!("signoff:{id}").as_str())
        );
        assert_eq!(spec.requirements, ["human"]);
        assert_eq!(spec.classification, classification);
        assert_eq!(spec.created_by, AGENT);
        assert_eq!(spec.title, format!("Approve: erasure of {id}"));
        assert!(
            !spec.body.contains(TITLE) && spec.body.contains(id),
            "{spec:?}"
        );
    }

    #[tokio::test]
    async fn an_erasure_by_another_principal_waits_for_the_owner_s_approval() {
        let dir = tempfile::tempdir().unwrap();
        let (m, agent) = (owner(dir.path()), session(dir.path(), AGENT));
        let id = entry(&m, "The door code is 4417.").await;
        let before = stored(&m, &id).await;
        let task = asked(&agent, &id, "an approval of human:test:me").await;
        assert_eq!(
            stored(&m, &id).await,
            before,
            "a refused erasure changes nothing"
        );
        is_the_sign_off_task(&task, &id, before.front.classification);
        // asked again, the same request is the same task
        assert_eq!(asked(&agent, &id, "an approval").await.id, task.id);
        assert_eq!(sign_offs(&m, &id).await.len(), 1);
        a_session_below_the_entry_asks_nothing(dir.path(), &id).await;
        assert_eq!(sign_offs(&m, &id).await.len(), 1);
        assert!(on_ledger(&m).await.is_empty(), "nobody approved yet");

        let now = OffsetDateTime::now_utc();
        at(&m, now).await;
        let approval = approved(&m, &task, None).await;
        let digest = dm_core::digest(&before.render());
        assert_eq!(approval.spec.scope, AGENT, "given to the requester");
        assert_eq!(
            (approval.spec.principal.as_str(), approval.at),
            (OWNER, now)
        );
        assert_eq!(
            (approval.spec.version, approval.spec.action.as_str()),
            (digest, "erase")
        );
        assert_eq!(approval.spec.expires, now + Duration::hours(24));
        assert_eq!(m.get_task(&task.id).await.unwrap().status, Status::Done);
        let receipt = agent.erase(&id, Reason::Mistaken).await.unwrap();
        assert_eq!(receipt.erased_by, AGENT);
    }

    /// An approval is in the event log of its domain: the copy of the ledger's events
    /// brings it, as the person who approved, at the highest label, and once.
    #[tokio::test]
    async fn the_log_of_the_domain_takes_an_approval_under_its_approver() {
        use crate::log::{read, Change};
        let dir = tempfile::tempdir().unwrap();
        let (m, agent) = (owner(dir.path()), session(dir.path(), AGENT));
        let id = entry(&m, "The door code is 4417.").await;
        let task = asked(&agent, &id, "an approval").await;
        let approval = approved(&m, &task, None).await;
        assert_eq!(approval.spec.scope, AGENT, "given to the requester");
        m.ledger_into_log(DOMAIN).await.unwrap();
        let in_log = || -> Vec<(String, Option<String>, String, Option<Label>)> {
            read(dir.path(), DOMAIN, 0)
                .unwrap()
                .into_iter()
                .filter_map(|e| match e.change {
                    Change::Ledger {
                        task,
                        op,
                        principal,
                        ..
                    } if op == "approved" => Some((task, principal, e.by, e.label)),
                    _ => None,
                })
                .collect()
        };
        let expected = vec![(
            format!("approval:{id}"),
            Some(OWNER.to_string()),
            OWNER.to_string(),
            Some(Label::Restricted),
        )];
        assert_eq!(in_log(), expected);
        assert_eq!(m.ledger_into_log(DOMAIN).await.unwrap(), 0);
        assert_eq!(in_log(), expected, "the copy run again adds nothing");
    }

    /// A session that may not read the entry gets the answer a missing entry gets,
    /// whatever name it carries when the memory server serves it.
    async fn a_session_below_the_entry_asks_nothing(dir: &Path, id: &str) {
        let low = session(dir, "agent:low").with_handling_cap(Label::Public);
        let answer = low.erase(id, Reason::Mistaken).await;
        assert!(matches!(answer, Err(Error::NotFound(_))), "{answer:?}");
        let named = served(Memory::open(dir).unwrap().with_handling_cap(Label::Public));
        let answer = named.erase(id, Reason::Mistaken).await;
        assert!(matches!(answer, Err(Error::NotFound(_))), "{answer:?}");
    }

    #[tokio::test]
    async fn an_approval_is_for_one_version_one_principal_and_a_time() {
        let dir = tempfile::tempdir().unwrap();
        let (m, agent) = (owner(dir.path()), session(dir.path(), AGENT));
        let a = entry(&m, "The door code is 4417.").await;
        let b = entry(&m, "The gate code is 2290.").await;
        let now = OffsetDateTime::now_utc();
        let until = now + Duration::hours(2);
        at(&m, now).await;
        at(&agent, now).await;
        let first = asked(&agent, &a, "an approval").await;
        let approval = approved(&m, &first, Some(until)).await;
        assert_eq!(approval.spec.expires, until, "the time given");

        another_principal_and_another_entry_stay_refused(&m, &agent, &a, &b).await;

        // at the expiry the approval permits nothing, and the request is made again
        // although the first task is done
        at(&agent, until).await;
        let second = asked(&agent, &a, "an approval of human:test:me").await;
        assert_ne!(second.id, first.id);
        assert!(!second.status.is_terminal(), "{second:?}");
        let listed = agent.approvals(None, Some(&a)).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert!(!listed[0].in_force, "expired on this ledger's clock");

        // a new approval holds, and stops when the entry has another version
        approved(&m, &second, None).await;
        changed(dir.path(), &a).await;
        let third = asked(&agent, &a, "another version").await;
        let event = third.spec.upstream_event.unwrap_or_default();
        let current = version(&m, &a).await;
        assert_eq!(event, format!("erase@{current}/0/{AGENT}"));
        assert!(stored(&m, &a).await.body.contains("4417"), "not erased");
    }

    /// An approval is given to one principal, for one entry and for one action.
    async fn another_principal_and_another_entry_stay_refused(
        m: &Memory,
        agent: &Memory,
        a: &str,
        b: &str,
    ) {
        let other = session(m.root(), "agent:y");
        let theirs = asked(&other, a, "an approval of human:test:me").await;
        assert_eq!(theirs.spec.created_by, "agent:y");
        asked(agent, b, "an approval of human:test:me").await;
        // an approval of another action on the entry, of this version and for this
        // principal, is no approval of its erasure
        let verify = ApprovalSpec {
            domain: DOMAIN.into(),
            principal: OWNER.into(),
            action: VERIFY.into(),
            target: b.into(),
            version: version(m, b).await,
            scope: AGENT.into(),
            expires: OffsetDateTime::now_utc() + Duration::hours(2),
        };
        m.ledger
            .lock()
            .await
            .record_approval(&verify)
            .await
            .unwrap();
        asked(agent, b, "an approval of human:test:me").await;
        assert!(stored(m, b).await.body.contains("2290"), "not erased");
    }

    #[tokio::test]
    async fn only_the_owner_in_person_signs_an_erasure_off() {
        let dir = tempfile::tempdir().unwrap();
        let (m, agent) = (owner(dir.path()), session(dir.path(), AGENT));
        let id = entry(&m, "The door code is 4417.").await;
        let task = asked(&agent, &id, "an approval").await;
        denied(agent.sign_off(&task.id, None).await, "is a person's");
        let person = session(dir.path(), "human:test:other");
        denied(
            person.sign_off(&task.id, None).await,
            "human:test:other does not",
        );
        let named = served(Memory::open(dir.path()).unwrap());
        denied(named.sign_off(&task.id, None).await, "approves nothing");
        // the entry changed since the request: the person would approve what nobody asked
        changed(dir.path(), &id).await;
        denied(m.sign_off(&task.id, None).await, "has changed since");
        let task = asked(&agent, &id, "an approval").await;
        let now = OffsetDateTime::now_utc();
        at(&m, now).await;
        denied(m.sign_off(&task.id, Some(now)).await, "permits nothing");
        let other = m.create_task(TaskSpec {
            title: "Tidy".into(),
            ..TaskSpec::default()
        });
        let other = other.await.unwrap().task;
        denied(m.sign_off(&other.id, None).await, "nor a sign-off task");
        assert!(on_ledger(&m).await.is_empty(), "a refusal records nothing");

        approved(&m, &task, None).await;
        denied(m.sign_off(&task.id, None).await, "approved already");
        assert_eq!(on_ledger(&m).await.len(), 1);
    }

    #[tokio::test]
    async fn a_served_session_named_as_the_owner_does_not_sign_off() {
        let dir = tempfile::tempdir().unwrap();
        let m = owner(dir.path());
        let id = entry(&m, "The door code is 4417.").await;
        let proposed = session(dir.path(), "agent:curator")
            .promote(&id, TrustTier::Verified, "checked against the door")
            .await
            .unwrap();
        let Promotion::Proposed { task: proposal } = proposed else {
            panic!("proposed: {proposed:?}")
        };
        let named = served(Memory::open(dir.path()).unwrap());
        assert_eq!(named.author(), OWNER);
        denied(named.approve(&proposal.id).await, "approves nothing");
        denied(named.sign_off(&proposal.id, None).await, "approves nothing");
        // its erasure is a request, as any other session's is
        let task = asked(&named, &id, "an approval of human:test:me").await;
        assert_eq!(task.spec.created_by, OWNER);
        denied(named.sign_off(&task.id, None).await, "approves nothing");
        assert!(on_ledger(&m).await.is_empty(), "nothing was approved");
        // the owner approves in a session that no harness reaches
        let approval = approved(&m, &task, None).await;
        assert_eq!(approval.spec.scope, OWNER);
        let receipt = named.erase(&id, Reason::Mistaken).await.unwrap();
        assert_eq!(receipt.erased_by, OWNER);
    }

    #[tokio::test]
    async fn a_sign_off_task_is_filed_by_the_service_alone() {
        let dir = tempfile::tempdir().unwrap();
        let (m, agent) = (owner(dir.path()), session(dir.path(), AGENT));
        let id = entry(&m, "The door code is 4417.").await;
        let event = sign_off_event(Gated::Erase, &version(&m, &id).await, 0, AGENT);
        for filer in [&m, &agent] {
            let forged = TaskSpec {
                title: format!("Approve: erasure of {id}"),
                requirements: vec!["human".into()],
                origin: Some(format!("signoff:{id}")),
                upstream_event: Some(event.clone()),
                ..TaskSpec::default()
            };
            denied(
                filer.create_task(forged).await,
                "only the service files one",
            );
        }
        assert!(sign_offs(&m, &id).await.is_empty());
    }

    #[tokio::test]
    async fn a_request_declined_is_not_made_again_until_the_entry_changes() {
        let dir = tempfile::tempdir().unwrap();
        let (m, agent) = (owner(dir.path()), session(dir.path(), AGENT));
        let id = entry(&m, "The door code is 4417.").await;
        let task = asked(&agent, &id, "an approval").await;
        m.invalidate(&task.id, "the code is right", None)
            .await
            .unwrap();
        let why = denied(agent.erase(&id, Reason::Mistaken).await, "was declined");
        assert!(why.contains(&task.id), "{why}");
        assert_eq!(sign_offs(&m, &id).await.len(), 1, "no second task");
        changed(dir.path(), &id).await;
        let again = asked(&agent, &id, "an approval").await;
        assert_ne!(again.id, task.id);
        assert!(!again.status.is_terminal(), "{again:?}");
    }

    #[tokio::test]
    async fn approving_a_proposal_to_verify_leaves_an_approval() {
        let dir = tempfile::tempdir().unwrap();
        let m = owner(dir.path());
        let id = entry(&m, "The door code is 4417.").await;
        let proposed = session(dir.path(), "agent:curator")
            .promote(&id, TrustTier::Verified, "checked against the door")
            .await
            .unwrap();
        let Promotion::Proposed { task } = proposed else {
            panic!("proposed: {proposed:?}")
        };
        let before = version(&m, &id).await;
        let signed = m.sign_off(&task.id, None).await.unwrap();
        assert!(matches!(signed, SignOff::Promoted { .. }), "{signed:?}");
        let listed = m.approvals(None, Some(&id)).await.unwrap();
        assert_eq!(listed.len(), 1, "{listed:?}");
        let a = &listed[0].record.spec;
        assert_eq!(
            (a.action.as_str(), a.target.as_str()),
            ("verify", id.as_str())
        );
        assert_eq!((a.principal.as_str(), a.scope.as_str()), (OWNER, OWNER));
        assert_eq!(a.version, before, "the version proposed");
    }

    /// Another connection takes the write lock of the ledger of `dir`.
    fn hold_the_ledger(dir: &Path) -> rusqlite::Connection {
        let holder = rusqlite::Connection::open(dir.join(crate::LEDGER_FILE)).unwrap();
        holder.execute_batch("BEGIN IMMEDIATE").unwrap();
        holder
    }

    #[tokio::test]
    async fn a_ledger_that_keeps_no_approval_stops_every_erasure_but_the_owner_s_in_person() {
        let dir = tempfile::tempdir().unwrap();
        let (m, agent) = (owner(dir.path()), session(dir.path(), AGENT));
        let id = entry(&m, "The door code is 4417.").await;
        let named = served(Memory::open(dir.path()).unwrap());
        let holder = hold_the_ledger(dir.path());
        // a session of the owner's name that the memory server serves erases nothing
        let answer = named.erase(&id, Reason::Mistaken).await;
        assert!(matches!(answer, Err(Error::Ledger(_))), "{answer:?}");
        let kept = stored(&m, &id).await.body;
        assert!(kept.contains("4417"), "a served session erased it");
        let answer = agent.erase(&id, Reason::Mistaken).await;
        assert!(matches!(answer, Err(Error::Ledger(_))), "{answer:?}");
        assert!(stored(&m, &id).await.body.contains("4417"), "not erased");
        // the owner in person: the text goes now, and the erasure stays unfinished
        let answer = m.erase(&id, Reason::Mistaken).await;
        assert!(matches!(answer, Err(Error::Ledger(_))), "{answer:?}");
        holder.execute_batch("ROLLBACK").unwrap();
        drop(holder);
        assert!(stored(&m, &id).await.body.is_empty(), "the text is gone");
        assert!(!m.receipt(&id).await.unwrap().unwrap().is_finished());
        assert!(on_ledger(&m).await.is_empty(), "the ledger kept nothing");
        // the run that finishes the erasure records its approval
        let receipt = m.erase(&id, Reason::Mistaken).await.unwrap();
        assert!(receipt.is_finished(), "{receipt:?}");
        assert_eq!(on_ledger(&m).await.len(), 1);
    }

    #[tokio::test]
    async fn approvals_are_listed_for_entries_the_session_may_read() {
        let dir = tempfile::tempdir().unwrap();
        let (m, agent) = (owner(dir.path()), session(dir.path(), AGENT));
        let id = entry(&m, "The door code is 4417.").await;
        let task = asked(&agent, &id, "an approval").await;
        approved(&m, &task, None).await;
        let low = session(dir.path(), "agent:low").with_handling_cap(Label::Public);
        assert!(low.approvals(None, None).await.unwrap().is_empty());
        assert!(low.approvals(None, Some(&id)).await.unwrap().is_empty());
        for reader in [&m, &agent] {
            let listed = reader.approvals(None, None).await.unwrap();
            assert_eq!(listed.len(), 1, "{listed:?}");
            assert_eq!(listed[0].record.spec.target, id);
            assert_eq!(reader.label(), Label::Public, "an approval is no read");
        }
    }

    const GUARD: &str = "human:test:guard";
    const WHY: &str = "Every visitor gets the code at the door.";
    const DOOR: &str = "The door code is 4417.";

    /// A restricted entry by the session `m`, with a source of its own.
    async fn restricted(m: &Memory, body: &str) -> String {
        let req = WriteRequest {
            title: Some(TITLE.into()),
            body: body.into(),
            cites: vec!["https://example.org/doors".into()],
            classification: Some(Label::Restricted),
            ..WriteRequest::default()
        };
        m.write(req).await.unwrap().id
    }

    /// A copy that landed: its id, and the approval it landed under.
    fn landed(answer: Result<Declassification, Error>) -> (String, ApprovalRecord) {
        match answer {
            Ok(Declassification::Declassified { id, approval, .. }) => (id, *approval),
            other => panic!("expected a copy: {other:?}"),
        }
    }

    /// A request that waits for the guard: its sign-off task, and why it waits.
    fn waits(answer: Result<Declassification, Error>) -> (Task, String) {
        match answer {
            Ok(Declassification::Proposed { task, reason }) => (*task, reason),
            other => panic!("expected a request: {other:?}"),
        }
    }

    /// How many entries the record holds, and how many events the ledger holds.
    async fn held(m: &Memory) -> (usize, u64) {
        let entries = m.store.list(DOMAIN).await.unwrap().len();
        let events = m.ledger.lock().await.event_count().await.unwrap();
        (entries, events)
    }

    async fn entries(m: &Memory) -> usize {
        held(m).await.0
    }

    /// The copy is a raw entry at internal with the source's author and text. It cites
    /// the version of the source and nothing that the source cites, and its origin names
    /// the label, the version and the guard.
    fn is_the_copy_of(copy: &Entry, source: &Entry) {
        let (c, s) = (&copy.front, &source.front);
        let version = format!("{}@{}", s.id, dm_core::digest(&source.render()));
        assert_eq!(c.classification, Label::Internal);
        let origin = format!("declassify:internal:{version}:{OWNER}");
        assert_eq!(c.origin.as_deref(), Some(origin.as_str()));
        assert_eq!(c.cites, [version]);
        assert_eq!(
            (c.trust_tier, s.trust_tier),
            (TrustTier::Raw, TrustTier::Curated)
        );
        assert_eq!((c.author.as_str(), &c.title), ("agent:writer", &s.title));
        assert_eq!(c.created, s.created, "the date of the text");
        assert_eq!((c.status, &c.supersedes), (None, &None));
        assert_eq!(copy.body, source.body);
    }

    /// A session that handles internal entries reads the copy, by id and by search, and
    /// the source stays not found for it.
    async fn a_session_at_internal_reads_the_copy_alone(dir: &Path, source: &str, copy: &str) {
        let low = session(dir, "agent:low").with_handling_cap(Label::Internal);
        assert!(low.get(copy).await.unwrap().body.contains("4417"));
        let hits = low.search("door code", None, 10).await.unwrap();
        let found: Vec<&str> = hits.iter().map(|h| h.id.as_str()).collect();
        assert_eq!(found, [copy]);
        let answer = low.get(source).await;
        assert!(matches!(answer, Err(Error::NotFound(_))), "{answer:?}");
    }

    #[tokio::test]
    async fn the_guard_s_own_declassification_lands_a_lower_copy_that_cites_the_source() {
        let dir = tempfile::tempdir().unwrap();
        let m = owner(dir.path());
        let id = restricted(&session(dir.path(), "agent:writer"), DOOR).await;
        changed(dir.path(), &id).await;
        let source = stored(&m, &id).await;
        let answer = m.declassify(&id, Label::Internal, WHY).await.unwrap();
        let Declassification::Declassified {
            id: copy,
            classification,
            cites,
            approval,
        } = answer
        else {
            panic!("expected a copy: {answer:?}")
        };
        assert_eq!(classification, Label::Internal);
        assert_eq!(cites, format!("{id}@{}", dm_core::digest(&source.render())));
        is_the_copy_of(&stored(&m, &copy).await, &source);
        assert_eq!(stored(&m, &id).await, source, "the source is as it was");
        a_session_at_internal_reads_the_copy_alone(dir.path(), &id, &copy).await;
        let listed = m.approvals(None, Some(&id)).await.unwrap();
        assert_eq!(listed.len(), 1, "{listed:?}");
        assert_eq!(listed[0].record, *approval);
        let a = &listed[0].record.spec;
        assert_eq!(a.action, "declassify:internal");
        assert_eq!((a.principal.as_str(), a.scope.as_str()), (OWNER, OWNER));
        // the same call again gives the copy that landed, and writes nothing
        let (again, _) = landed(m.declassify(&id, Label::Internal, WHY).await);
        assert_eq!(again, copy);
        assert_eq!(entries(&m).await, 2, "the source and one copy");
    }

    /// The task that the writer of [`restricted_with_tap_reach`] holds. The ledger does
    /// not hold it.
    const PLANTED: &str = "task-planted-1";

    /// A restricted raw entry with tap reach that names [`PLANTED`]: its writer read
    /// content from outside the fleet.
    async fn restricted_with_tap_reach(dir: &Path) -> String {
        let writer = session(dir, "agent:writer");
        writer.set_held_task(PLANTED);
        crate::lock(&writer.session).read_outside();
        restricted(&writer, DOOR).await
    }

    /// The reach of a stored entry, and the task that it names.
    async fn reach_of(m: &Memory, id: &str) -> (dm_core::Reach, Option<String>) {
        let front = stored(m, id).await.front;
        (front.reach, front.task)
    }

    #[tokio::test]
    async fn the_copy_of_a_raw_entry_keeps_its_reach_and_its_task() {
        use dm_core::Reach;
        let dir = tempfile::tempdir().unwrap();
        let m = owner(dir.path());
        let id = restricted_with_tap_reach(dir.path()).await;
        let planted = (Reach::Tap, Some(PLANTED.to_string()));
        assert_eq!(reach_of(&m, &id).await, planted);
        assert!(!m.read_outside(), "before the request");
        let (copy, _) = landed(m.declassify(&id, Label::Internal, WHY).await);
        assert!(m.read_outside(), "to ask for the copy is to read the entry");
        assert_eq!(reach_of(&m, &copy).await, planted);
        // the copy has the readers of the source: the task chain, and no other agent
        let reader = session(dir.path(), "agent:reader").with_handling_cap(Label::Internal);
        let answer = reader.get(&copy).await;
        assert!(matches!(answer, Err(Error::NotFound(_))), "{answer:?}");
        let member = session(dir.path(), "agent:member").with_handling_cap(Label::Internal);
        member.set_held_task(PLANTED);
        assert!(member.get(&copy).await.unwrap().body.contains("4417"));
        // a curated source keeps the field, and its reach decides nothing: its copy has
        // fleet reach and names no task
        let why = "checked against the door";
        m.promote(&id, TrustTier::Curated, why).await.unwrap();
        let curated = stored(&m, &id).await.front;
        assert_eq!(
            (curated.trust_tier, curated.reach),
            (TrustTier::Curated, Reach::Tap)
        );
        let (second, _) = landed(m.declassify(&id, Label::Internal, WHY).await);
        assert_ne!(second, copy, "a new version has a new copy");
        assert_eq!(reach_of(&m, &second).await, (Reach::Fleet, None));
        assert!(reader.get(&second).await.unwrap().body.contains("4417"));
    }

    #[tokio::test]
    async fn a_session_that_does_not_read_a_raw_entry_asks_for_no_erasure_of_it() {
        let dir = tempfile::tempdir().unwrap();
        let m = owner(dir.path());
        let id = restricted_with_tap_reach(dir.path()).await;
        // an agent outside the task chain gets the answer for an entry that does not
        // exist, and no task asks the owner
        let outsider = session(dir.path(), AGENT);
        let answer = outsider.erase(&id, Reason::Mistaken).await;
        assert!(matches!(answer, Err(Error::NotFound(_))), "{answer:?}");
        assert!(sign_offs(&m, &id).await.is_empty());
        // an agent in the task chain reads the entry, so it asks
        let member = session(dir.path(), AGENT);
        member.set_held_task(PLANTED);
        asked(&member, &id, "an approval of human:test:me").await;
        assert_eq!(sign_offs(&m, &id).await.len(), 1);
    }

    /// A session that does not handle the label of a task does not get the task.
    async fn a_session_below_the_task_does_not_see_it(dir: &Path, task: &Task) {
        let low = session(dir, "agent:low").with_handling_cap(Label::Internal);
        let hidden = low.get_task(&task.id).await;
        assert!(matches!(hidden, Err(Error::Ledger(_))), "{hidden:?}");
    }

    /// What the session writes next is classified restricted: it read a restricted entry.
    async fn writes_at_restricted(m: &Memory) {
        assert_eq!(m.label(), Label::Restricted);
        let note = WriteRequest {
            body: "A note.".into(),
            ..WriteRequest::default()
        };
        let written = m.write(note).await.unwrap();
        assert_eq!(written.classification, Label::Restricted);
    }

    #[tokio::test]
    async fn a_declassification_asked_by_another_principal_waits_for_the_guard() {
        let dir = tempfile::tempdir().unwrap();
        let (m, agent) = (owner(dir.path()), session(dir.path(), AGENT));
        let id = restricted(&m, DOOR).await;
        let (task, reason) = waits(agent.declassify(&id, Label::Internal, WHY).await);
        assert!(
            reason.contains("an approval of human:test:me, the guard of dom-me"),
            "{reason}"
        );
        assert_eq!(task.spec.classification, Label::Restricted);
        assert_eq!(task.spec.created_by, AGENT);
        assert!(task.spec.body.contains(WHY), "{:?}", task.spec);
        a_session_below_the_task_does_not_see_it(dir.path(), &task).await;
        assert_eq!(entries(&m).await, 1, "nothing exists at the lower label");
        // to ask for a lower copy of an entry is to read the entry
        writes_at_restricted(&agent).await;
        approved(&m, &task, None).await;
        // the requester runs the same call again, here in a new session
        let later = session(dir.path(), AGENT);
        let (copy, approval) = landed(later.declassify(&id, Label::Internal, WHY).await);
        assert_eq!(approval.spec.scope, AGENT);
        let copy = stored(&m, &copy).await.front;
        assert_eq!(copy.classification, Label::Internal);
        writes_at_restricted(&later).await;
        only_the_guard_that_the_grants_name_signs_a_copy_off().await;
    }

    /// A root whose domain names a guard that is not the owner.
    fn guarded(dir: &Path) -> Memory {
        drop(owner(dir));
        let path = dir.join(crate::GRANTS_FILE);
        let mut grants = Grants::load(&path).unwrap();
        grants.domains.get_mut(DOMAIN).unwrap().guard = Some(GUARD.into());
        std::fs::write(&path, grants.render().unwrap()).unwrap();
        Memory::open(dir).unwrap()
    }

    /// With a guard in the grants, the owner approves no copy, and the owner's own call
    /// is a request as any other. The guard approves, and the copy names the guard.
    async fn only_the_guard_that_the_grants_name_signs_a_copy_off() {
        let dir = tempfile::tempdir().unwrap();
        let m = guarded(dir.path());
        let id = restricted(&m, "The gate code is 2290.").await;
        let (task, reason) = waits(m.declassify(&id, Label::Internal, WHY).await);
        assert!(
            reason.contains("an approval of human:test:guard"),
            "{reason}"
        );
        denied(
            m.sign_off(&task.id, None).await,
            "human:test:guard approves declassify in dom-me; human:test:me does not",
        );
        let guard = session(dir.path(), GUARD);
        let approval = approved(&guard, &task, None).await;
        let spec = &approval.spec;
        assert_eq!(
            (spec.principal.as_str(), spec.scope.as_str()),
            (GUARD, OWNER)
        );
        let (copy, _) = landed(m.declassify(&id, Label::Internal, WHY).await);
        let origin = stored(&m, &copy).await.front.origin.unwrap_or_default();
        assert!(origin.ends_with(":human:test:guard"), "{origin}");
    }

    #[tokio::test]
    async fn a_declassification_is_for_one_version_and_one_label() {
        let dir = tempfile::tempdir().unwrap();
        let (m, agent) = (owner(dir.path()), session(dir.path(), AGENT));
        let id = restricted(&m, DOOR).await;
        let (first, _) = waits(agent.declassify(&id, Label::Internal, WHY).await);
        approved(&m, &first, None).await;
        let (copy, _) = landed(agent.declassify(&id, Label::Internal, WHY).await);
        // the source has a new version: the approval and the copy are of the old one
        changed(dir.path(), &id).await;
        let (second, reason) = waits(agent.declassify(&id, Label::Internal, WHY).await);
        assert_ne!(second.id, first.id);
        assert!(reason.contains("another version"), "{reason}");
        assert_eq!(entries(&m).await, 2, "no copy of the new version yet");
        // an approval of a copy at internal is no approval of a copy at public
        approved(&m, &second, None).await;
        let (third, _) = waits(agent.declassify(&id, Label::Public, WHY).await);
        let event = third.spec.upstream_event.clone().unwrap_or_default();
        assert!(event.starts_with("declassify:public@"), "{event}");
        assert_eq!(entries(&m).await, 2, "no copy at public");
        // each version and each label has its own copy
        let (newer, _) = landed(agent.declassify(&id, Label::Internal, WHY).await);
        approved(&m, &third, None).await;
        let (public, _) = landed(agent.declassify(&id, Label::Public, WHY).await);
        assert!(copy != newer && newer != public && public != copy);
        let public = stored(&m, &public).await.front;
        assert_eq!(public.classification, Label::Public);
        assert_eq!(entries(&m).await, 4);
    }

    /// An entry that the record holds as replaced while the index still serves it.
    async fn replaced_in_the_record_alone(m: &Memory) -> String {
        let id = entry(m, "The side door has no code.").await;
        let mut successor = stored(m, &id).await;
        successor.front.id = dm_core::id::new_entry_id(DOMAIN);
        successor.front.supersedes = Some(id.clone());
        m.store.put(&successor).await.unwrap();
        assert_eq!(
            stored(m, &id).await.front.status,
            Some(dm_core::Status::Superseded),
            "the record marks it"
        );
        id
    }

    /// No label that is not lower than the entry's, and no justification that is not
    /// words on one line.
    async fn refuses_what_is_no_declassification(m: &Memory, restricted: &str, internal: &str) {
        let before = held(m).await;
        let mut whys = Vec::new();
        for (id, to) in [
            (restricted, Label::Restricted),
            (internal, Label::Restricted),
            (internal, Label::Internal),
        ] {
            let why = denied(m.declassify(id, to, WHY).await, "is not lower");
            whys.push((id, why));
        }
        // the guard's own call is refused before its approval is recorded
        assert_eq!(held(m).await, before, "a refused label records nothing");
        assert!(whys.iter().all(|(id, why)| why.starts_with(id)), "{whys:?}");
        for no_words in ["", " \n", "Two\nlines.", "See <b>this</b>.", "```"] {
            let answer = m.declassify(restricted, Label::Internal, no_words).await;
            denied(answer, "words on one line");
        }
    }

    /// Three entries that are not live: one replaced, one erased, and one that the
    /// record holds as replaced while the index still serves it.
    async fn not_live(m: &Memory) -> [String; 3] {
        let old = entry(m, "The gate code was 1180.").await;
        let new = WriteRequest {
            body: "The gate code is 2290.".into(),
            supersedes: Some(old.clone()),
            ..WriteRequest::default()
        };
        m.write(new).await.unwrap();
        let erased = entry(m, "The key is under the mat.").await;
        m.erase(&erased, Reason::Mistaken).await.unwrap();
        [old, erased, replaced_in_the_record_alone(m).await]
    }

    /// The kind of an answer that is no refusal of the policy and no copy.
    fn kind(answer: Result<Declassification, Error>) -> &'static str {
        match answer {
            Err(Error::NotFound(_)) => "not found",
            Err(Error::Moved { .. }) => "moved",
            Err(Error::Gone(_)) => "gone",
            other => panic!("expected an entry that is not served: {other:?}"),
        }
    }

    /// An entry that the session may not read is not found. One that it could read and
    /// that is not live is refused: as a read of it is, or by the record when the index
    /// still serves it.
    async fn refuses_what_is_not_there(m: &Memory, restricted: &str) {
        let low = session(m.root(), "agent:low").with_handling_cap(Label::Internal);
        let answer = low.declassify(restricted, Label::Public, WHY).await;
        assert_eq!(kind(answer), "not found");
        let [old, erased, lagging] = not_live(m).await;
        let before = held(m).await;
        assert_eq!(kind(m.declassify(&old, Label::Public, WHY).await), "moved");
        let answer = m.declassify(&erased, Label::Public, WHY).await;
        assert_eq!(kind(answer), "gone");
        let answer = m.declassify(&lagging, Label::Public, WHY).await;
        denied(
            answer,
            "was replaced; ask for a copy of its current version",
        );
        assert_eq!(held(m).await, before, "a refusal records nothing");
    }

    #[tokio::test]
    async fn a_declassification_that_cannot_be_is_refused_before_anything_is_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let m = owner(dir.path());
        let id = restricted(&m, DOOR).await;
        let internal = entry(&m, "The gate code is 2290.").await;
        let before = held(&m).await;
        refuses_what_is_no_declassification(&m, &id, &internal).await;
        assert_eq!(held(&m).await, before, "a refusal records nothing");
        assert_eq!(m.label(), Label::Public, "and reads nothing");
        refuses_what_is_not_there(&m, &id).await;
        // a session of the owner's name that the memory server serves lands no copy: its
        // call is a request
        let before = entries(&m).await;
        let named = served(Memory::open(dir.path()).unwrap());
        let (task, _) = waits(named.declassify(&id, Label::Internal, WHY).await);
        assert_eq!(task.spec.created_by, OWNER);
        assert!(on_ledger(&m).await.iter().all(|a| a.spec.action == "erase"));
        assert_eq!(entries(&m).await, before, "no copy");
    }

    /// Every sign-off task of the entry is closed, and holds the id of the entry and
    /// nothing that a requester wrote. Gives how many there are.
    async fn closed_with_the_id_alone(m: &Memory, id: &str) -> usize {
        let tasks = sign_offs(m, id).await;
        for task in &tasks {
            let all = format!("{task:?}");
            assert!(!all.contains("4417") && !all.contains("visitor"), "{all}");
            assert_eq!(task.spec.title, format!("Approve: {id}"));
            assert_eq!(task.status, Status::Invalidated, "{all}");
        }
        tasks.len()
    }

    #[tokio::test]
    async fn an_erasure_of_the_source_takes_the_request_and_flags_the_copy() {
        let dir = tempfile::tempdir().unwrap();
        let (m, agent) = (owner(dir.path()), session(dir.path(), AGENT));
        let id = restricted(&m, DOOR).await;
        let (copy, _) = landed(m.declassify(&id, Label::Internal, WHY).await);
        let said = "The door code 4417 is for every visitor.";
        let (task, _) = waits(agent.declassify(&id, Label::Internal, said).await);
        assert!(task.spec.body.contains(said), "{:?}", task.spec);
        let receipt = m.erase(&id, Reason::Mistaken).await.unwrap();
        assert!(receipt.is_finished(), "{receipt:?}");
        assert_eq!(receipt.withheld, [copy], "the copy is withheld");
        assert_eq!(closed_with_the_id_alone(&m, &id).await, 1);
        // the events hold nothing of it either: a replay brings nothing back
        m.ledger.lock().await.rebuild().await.unwrap();
        assert_eq!(closed_with_the_id_alone(&m, &id).await, 1);
        // a request that reaches the ledger after the erasure goes with the next run
        let late = TaskSpec {
            domain: DOMAIN.into(),
            title: format!("Approve: copy of {id} at internal"),
            body: format!("Why: {said}"),
            origin: Some(format!("{SIGN_OFF_ORIGIN}{id}")),
            upstream_event: Some(format!("declassify:internal@late/0/{AGENT}")),
            classification: Label::Restricted,
            ..TaskSpec::default()
        };
        m.ledger.lock().await.create(late).await.unwrap();
        denied(m.erase(&id, Reason::Mistaken).await, "already erased");
        assert_eq!(closed_with_the_id_alone(&m, &id).await, 2);
    }

    /// Two sessions of the requester make the same call at the same time. Gives the
    /// copy that each call answers.
    async fn both_at_once(dir: &Path, id: &str) -> (String, String) {
        let (a, b) = (session(dir, AGENT), session(dir, AGENT));
        let (first, second) = tokio::join!(
            a.declassify(id, Label::Internal, WHY),
            b.declassify(id, Label::Internal, WHY)
        );
        (landed(first).0, landed(second).0)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_calls_at_the_same_time_land_one_copy() {
        let dir = tempfile::tempdir().unwrap();
        let (m, agent) = (owner(dir.path()), session(dir.path(), AGENT));
        for round in 1..=6usize {
            let body = format!("The door code of floor {round} is 4417.");
            let id = restricted(&m, &body).await;
            let (task, _) = waits(agent.declassify(&id, Label::Internal, WHY).await);
            approved(&m, &task, None).await;
            let (first, second) = both_at_once(dir.path(), &id).await;
            assert_eq!(first, second, "one copy of one version");
            let copies = entries(&m).await - round;
            assert_eq!(copies, round, "one copy for each source");
        }
        // the log names each copy at the version that the record holds
        let built = m.materialize(None).await.unwrap();
        assert!(built.behind.is_empty(), "{built:?}");
    }

    /// A call that found no copy can reach the write after another call landed the copy,
    /// and after the copy changed. The record takes nothing from it.
    #[tokio::test]
    async fn a_copy_that_is_there_is_not_written_again() {
        let dir = tempfile::tempdir().unwrap();
        let m = owner(dir.path());
        let id = restricted(&m, DOOR).await;
        let (copy, approval) = landed(m.declassify(&id, Label::Internal, WHY).await);
        changed(dir.path(), &copy).await;
        let promoted = stored(&m, &copy).await;
        assert_eq!(promoted.front.trust_tier, TrustTier::Curated);
        let source = stored(&m, &id).await;
        let version = &approval.spec.version;
        let at = Landing::of(DOMAIN, &id, version, Label::Internal, OWNER);
        assert_eq!(at.id, copy, "the same landing");
        m.write_copy(&source, &at).await.unwrap();
        assert_eq!(stored(&m, &copy).await, promoted, "the copy is as it was");
        let history = m.store.history(DOMAIN, &copy).await.unwrap();
        assert_eq!(history.len(), 1, "the raw version, from the promotion");
    }

    #[tokio::test]
    async fn the_answer_names_the_approval_that_was_given_to_the_caller() {
        let dir = tempfile::tempdir().unwrap();
        let m = owner(dir.path());
        let (x, y) = (session(dir.path(), AGENT), session(dir.path(), "agent:y"));
        let id = restricted(&m, DOOR).await;
        let (for_x, _) = waits(x.declassify(&id, Label::Internal, WHY).await);
        let (for_y, _) = waits(y.declassify(&id, Label::Internal, WHY).await);
        assert_ne!(for_x.id, for_y.id, "each requester has its own request");
        // the approval for agent:y is the newer one of the two
        let given_x = approved(&m, &for_x, None).await;
        let given_y = approved(&m, &for_y, None).await;
        let (copy, under) = landed(x.declassify(&id, Label::Internal, WHY).await);
        assert_eq!(under, given_x, "agent:x ran under its own approval");
        let (same, under) = landed(y.declassify(&id, Label::Internal, WHY).await);
        assert_eq!(under, given_y, "agent:y ran under its own approval");
        assert_eq!(same, copy, "one copy for the two requesters");
        assert_eq!(entries(&m).await, 2, "the source and one copy");
    }

    #[tokio::test]
    async fn a_copy_that_was_erased_is_not_answered_and_does_not_land_again() {
        let dir = tempfile::tempdir().unwrap();
        let (m, agent) = (owner(dir.path()), session(dir.path(), AGENT));
        let id = restricted(&m, DOOR).await;
        let (copy, _) = landed(m.declassify(&id, Label::Internal, WHY).await);
        m.erase(&copy, Reason::Mistaken).await.unwrap();
        let before = held(&m).await;
        // the guard in person, and a requester that has no approval
        for asker in [&m, &agent] {
            let answer = asker.declassify(&id, Label::Internal, WHY).await;
            let why = denied(answer, "was erased");
            assert!(why.contains(&copy), "{why}");
        }
        let after = held(&m).await;
        assert_eq!(after, before, "no entry, no approval and no task");
        // a copy at another label is another copy
        let (public, _) = landed(m.declassify(&id, Label::Public, WHY).await);
        assert_ne!(public, copy);
    }

    #[tokio::test]
    async fn only_the_approver_in_person_declines_a_request() {
        let dir = tempfile::tempdir().unwrap();
        let (m, agent) = (owner(dir.path()), session(dir.path(), AGENT));
        let id = entry(&m, DOOR).await;
        let task = asked(&agent, &id, "an approval").await;
        let other = session(dir.path(), "agent:y");
        let person = session(dir.path(), "human:test:other");
        for no_approver in [&other, &agent, &person] {
            let answer = no_approver.invalidate(&task.id, "invalid: no", None).await;
            denied(answer, "only its approver declines it");
        }
        let named = served(Memory::open(dir.path()).unwrap());
        let answer = named.invalidate(&task.id, "invalid: no", None).await;
        denied(answer, "declines nothing");
        // nobody declined: the request is open, and the owner can approve it
        let again = asked(&agent, &id, "an approval of human:test:me").await;
        assert_eq!(again.id, task.id);
        assert!(!again.status.is_terminal(), "{again:?}");
        approved(&m, &task, None).await;
        agent.erase(&id, Reason::Mistaken).await.unwrap();
    }
}
