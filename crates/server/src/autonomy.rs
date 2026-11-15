//! The autonomy level of a task definition: what a completion of a task needs before
//! it counts.
//!
//! A task that names a task definition is done only when its completion has what the
//! level of that version of the definition asks:
//!
//! - `person`: a person's verdict, given in a session that the memory server does not
//!   serve;
//! - `review`: a verdict from another principal than the one that handed the work in;
//! - `checks`: nothing more. The completion closes the task.
//!
//! The level of a version is state on the ledger, in the platform's domain, under the
//! reference `<name>@<entry>` ([`dm_ledger::DefinitionRecord`]). It is not text in the
//! entry: the `autonomy` that the definition declares is the highest level it may earn.
//! A version with no record is at `person`, so a changed definition starts again there.
//! A task whose definition left force is completed at `person` too.
//!
//! Below `checks`, a completion parks the task: the task is blocked on the event
//! `verdict:<task>` with the handoff on it, so the rules of a waiting task hold it.
//! Nobody claims it, what depends on it stays blocked, and the task that it continues
//! stays open. [`Memory::verdict`] accepts the work, which closes the task, or rejects
//! it, which puts the task back on offer and lowers the level one step. Nobody gives
//! the verdict on its own work at any level.
//!
//! An outcome that reverts or rejects the work of a task lowers the level one step
//! ([`Memory::record_outcome`]), once for each task and kind of outcome.

use crate::{lock, Error, Memory};
use dm_ledger::{
    Autonomy, DefinitionChange, DefinitionRecord, Held, Ledger, Status, Task, WorkOutcome,
};
use dm_policy::decide::Class;
use serde::Serialize;

/// The start of the name of the event that a task waits on for its verdict. The event
/// of a task is `verdict:<task id>`.
pub const VERDICT_EVENT: &str = "verdict:";

/// The event that `task` waits on while its completion needs a verdict.
#[must_use]
pub fn verdict_event(task: &str) -> String {
    format!("{VERDICT_EVENT}{task}")
}

/// Whether a task waits for the verdict on its own work: it is not closed, and the
/// event it waits on is its own.
#[must_use]
pub fn awaits_verdict(task: &Task) -> bool {
    !task.status.is_terminal()
        && task.waiting_on.as_deref() == Some(verdict_event(&task.id).as_str())
}

/// What a verdict did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Verdict {
    /// The task after the verdict: done, or on offer again with its handoff.
    pub task: Task,
    /// Whether the work was accepted.
    pub accepted: bool,
    /// The level that the verdict was given at.
    pub level: Autonomy,
    /// The outcome that a rejection recorded on the definition, or the one that an
    /// earlier rejection of the same task recorded. None for an accepted task, and
    /// where the grants name no platform domain.
    pub outcome: Option<DefinitionRecord>,
}

/// What one version of a task definition has earned.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AutonomyView {
    /// The version, as a task names it: `<name>@<entry>`.
    pub definition: String,
    /// The level in force for it.
    pub level: Autonomy,
    /// The highest level that the definition's text lets it earn.
    pub ceiling: Autonomy,
    /// Its records on the ledger, oldest first.
    pub records: Vec<DefinitionRecord>,
}

/// The level that a version's records leave it at: where the last raise or outcome put
/// it, and the lowest level when there is none. A graded run alone moves nothing.
fn level_after(records: &[DefinitionRecord]) -> Autonomy {
    records
        .iter()
        .rev()
        .find_map(|r| match &r.change {
            DefinitionChange::Raised { to, .. } | DefinitionChange::Outcome { to, .. } => Some(*to),
            DefinitionChange::Graded { .. } => None,
        })
        .unwrap_or_default()
}

/// The refusal for a task that does not wait for a verdict.
fn no_verdict_awaited(id: &str) -> Error {
    Error::Denied(format!(
        "{id} does not wait for a verdict: a verdict is given on work that was handed in under a task definition and is not yet accepted"
    ))
}

impl Memory {
    /// The autonomy level of one version of a task definition, named as
    /// `<name>@<entry>`: the level that its last raise or outcome on the ledger of the
    /// platform's domain left it at. A version with no such record is at `person`, and
    /// so is every version where the grants name no platform domain.
    ///
    /// This reads the ledger and nothing of the entry, and it is not held to what the
    /// session may read. To answer a session, use [`Memory::autonomy`].
    ///
    /// # Errors
    ///
    /// A ledger error.
    pub async fn level_of(&self, reference: &str) -> Result<Autonomy, Error> {
        let l = self.ledger.lock().await;
        self.level_in(&l, reference).await
    }

    /// [`Memory::level_of`], through a ledger that the caller holds locked.
    async fn level_in(&self, l: &Ledger, reference: &str) -> Result<Autonomy, Error> {
        let Some(domain) = self.platform_domain() else {
            return Ok(Autonomy::Person);
        };
        let records = l.definition_records(&domain, Some(reference)).await?;
        Ok(level_after(&records))
    }

    /// The level that a completion of `task` is held to. None for a task that names no
    /// definition. `person` for a task whose definition left force: nothing that the
    /// version earned applies to work under a definition that nobody stands behind now.
    async fn task_level(&self, l: &Ledger, task: &Task) -> Result<Option<Autonomy>, Error> {
        let Some(named) = &task.spec.definition else {
            return Ok(None);
        };
        // the service pins every task to `<name>@<entry>` when it is filed, so the name
        // on the task is the reference that the records are under
        match self.pinned_definition(named).await {
            Ok(Some(_)) => Ok(Some(self.level_in(l, named).await?)),
            Ok(None) | Err(Error::Denied(_)) => Ok(Some(Autonomy::Person)),
            Err(e) => Err(e),
        }
    }

    /// Whether a completion of `task` does not close it: the task names a definition,
    /// and the level of that definition is below `checks`.
    pub(crate) async fn needs_verdict(&self, l: &Ledger, task: &Task) -> Result<bool, Error> {
        let level = self.task_level(l, task).await?;
        Ok(level.is_some_and(|level| level < Autonomy::Checks))
    }

    /// Whether a completion with `handoff` is one that this session's principal made
    /// before: the task waits for its verdict with that handoff on it, and the ledger
    /// gave this principal the claim that the work was handed in under.
    pub(crate) async fn handed_in_before(
        &self,
        l: &Ledger,
        task: &Task,
        handoff: &str,
    ) -> Result<bool, Error> {
        if !awaits_verdict(task) || task.handoff.as_deref() != Some(handoff) {
            return Ok(false);
        }
        let worker = l.claimant(&task.id, task.generation).await?;
        Ok(worker.as_deref() == Some(self.author.as_str()))
    }

    /// Hand the work of a held task in for its verdict: the task is blocked on its
    /// verdict event with the handoff on it, and this session's claim is over. Nothing
    /// that a completion closes is closed: not the task, and not the task it continues.
    ///
    /// # Errors
    ///
    /// The ledger's refusals of a completion: [`dm_ledger::Error::WrongState`] for a
    /// task that is not held, or that has an effect that nobody has settled;
    /// [`dm_ledger::Error::NotHolder`] or [`dm_ledger::Error::Fenced`] for a claim that
    /// is not in force.
    pub(crate) async fn park_for_verdict(
        &self,
        l: &mut Ledger,
        task: &Task,
        by: Held<'_>,
        handoff: &str,
    ) -> Result<Task, Error> {
        let refused = |op| {
            Error::Ledger(dm_ledger::Error::WrongState {
                id: task.id.clone(),
                status: task.status,
                op,
            })
        };
        if task.status != Status::Claimed {
            return Err(refused("complete"));
        }
        // whether the destination holds the effect is part of what done means, at
        // every level: work with an effect that is not settled is not handed in
        if !task.effects.is_empty() {
            return Err(refused("complete while an effect of it is not settled"));
        }
        let parked = l
            .wait_for(&task.id, by, &verdict_event(&task.id), Some(handoff))
            .await?;
        lock(&self.claims).remove(&task.id);
        self.observe_task(&parked);
        Ok(parked)
    }

    /// Whether this session is a person in person: the owner or another person, in a
    /// session that the memory server does not serve. A harness reaches a served
    /// session, so an agent may have asked, whatever the session's name.
    fn in_person(&self, what: &str) -> Result<(), Error> {
        let person = matches!(
            Class::of(&self.author, &self.grants.owner),
            Class::Owner | Class::Human
        );
        if person && !self.served() {
            return Ok(());
        }
        Err(Error::Denied(format!(
            "only a person {what}, in a session that the memory server does not serve"
        )))
    }

    /// The level that a verdict on `task` is given at, when this session may give it.
    /// Nobody gives the verdict on its own work. At `person`, only a person in person
    /// gives it. Above that, every other principal than the worker does.
    async fn may_give_verdict(&self, l: &Ledger, task: &Task) -> Result<Autonomy, Error> {
        let worker = l.claimant(&task.id, task.generation).await?;
        if worker.as_deref() == Some(self.author.as_str()) {
            return Err(Error::Denied(format!(
                "{} handed the work of {} in: nobody gives the verdict on its own work",
                self.author, task.id
            )));
        }
        let level = self.task_level(l, task).await?.unwrap_or_default();
        if level == Autonomy::Person {
            self.in_person("gives the verdict on work at the level person")?;
        }
        Ok(level)
    }

    /// The handoff of a task that waits for its verdict, when this session may see the
    /// task and may give the verdict.
    async fn handoff_to_judge(&self, id: &str) -> Result<String, Error> {
        let l = self.ledger.lock().await;
        let task = l.get(id).await?;
        self.check_task(&task)?;
        let handoff = task.handoff.clone().filter(|_| awaits_verdict(&task));
        let handoff = handoff.ok_or_else(|| no_verdict_awaited(id))?;
        self.may_give_verdict(&l, &task).await?;
        Ok(handoff)
    }

    /// Give the verdict on the work of a task that waits for one. To accept closes the
    /// task with the handoff that was handed in, opens what depended on it, and closes
    /// the task that it continues. To reject records a `rejected` outcome on the
    /// task's definition, which lowers the level one step, and puts the task back on
    /// offer with the handoff on it.
    ///
    /// The session must be one that may read the handoff, and it has then read it. At
    /// the level `person`, the session is a person's and the memory server does not
    /// serve it. At every level, it is not the principal that handed the work in.
    ///
    /// # Errors
    ///
    /// The ledger's not-found for a task outside the grants or above the session's
    /// handling; [`Error::Denied`] for a task that does not wait for a verdict, from
    /// the principal that handed the work in, and at `person` from anyone but a person
    /// in person; [`Error::NotFound`] for a handoff that the session may not read; a
    /// ledger or store error.
    pub async fn verdict(&self, task_id: &str, accept: bool) -> Result<Verdict, Error> {
        let handoff = self.handoff_to_judge(task_id).await?;
        // the ledger is not held here: the read is under the session's own filter
        self.get(&handoff).await?;
        let mut l = self.ledger.lock().await;
        let task = l.get(task_id).await?;
        // the task as it is now: the verdict is on the handoff that was read
        if !awaits_verdict(&task) || task.handoff.as_deref() != Some(handoff.as_str()) {
            return Err(no_verdict_awaited(task_id));
        }
        let level = self.may_give_verdict(&l, &task).await?;
        let event = verdict_event(task_id);
        if accept {
            let done = l.complete_awaited(task_id, &event, &handoff).await?;
            l.close_resumed(&done).await;
            self.observe_task(&done);
            return Ok(Verdict {
                task: done,
                accepted: true,
                level,
                outcome: None,
            });
        }
        // the outcome first: a stop after it leaves a task that still waits, and the
        // same verdict asked again records nothing more and puts the task back
        let outcome = self
            .outcome_in(&mut l, &task, WorkOutcome::Rejected)
            .await?;
        l.arrived(&task.spec.domain, &event).await?;
        let back = l.get(task_id).await?;
        self.observe_task(&back);
        Ok(Verdict {
            task: back,
            accepted: false,
            level,
            outcome,
        })
    }

    /// Record an outcome of `task` on its definition, once for each task and kind of
    /// outcome: the record of that outcome that the ledger holds already, or a new one
    /// that lowers the level one step. None where the task names no definition or the
    /// grants name no platform domain.
    async fn outcome_in(
        &self,
        l: &mut Ledger,
        task: &Task,
        outcome: WorkOutcome,
    ) -> Result<Option<DefinitionRecord>, Error> {
        let (Some(domain), Some(reference)) = (self.platform_domain(), &task.spec.definition)
        else {
            return Ok(None);
        };
        let records = l.definition_records(&domain, Some(reference)).await?;
        let same = records.iter().find(|r| {
            matches!(&r.change, DefinitionChange::Outcome { task: t, outcome: o, .. }
                if *t == task.id && *o == outcome)
        });
        if let Some(recorded) = same {
            return Ok(Some(recorded.clone()));
        }
        let change = DefinitionChange::Outcome {
            task: task.id.clone(),
            outcome,
            by: self.author.clone(),
            to: level_after(&records).lower(),
        };
        let record = l.record_definition(&domain, reference, change).await?;
        Ok(Some(record))
    }

    /// Record that the work of a task did not stand: it was accepted and later
    /// reverted, or it was rejected. The outcome is recorded on the version of the
    /// definition that the task names, and lowers the level of that version one step;
    /// the lowest level stays where it is. The same outcome of the same task is
    /// recorded once: asked again, the first record comes back and nothing is appended.
    /// The task itself does not change.
    ///
    /// A person records an outcome, in a session that the memory server does not
    /// serve: an agent would otherwise lower or keep a level by its own say.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] from anyone but a person in person, for a task that names no
    /// definition, for `reverted` on a task that is not done, for `rejected` on a task
    /// that is not done and does not wait for its verdict, and where the grants name
    /// no platform domain; the ledger's not-found for a task outside the grants or
    /// above the session's handling; a ledger error.
    pub async fn record_outcome(
        &self,
        task_id: &str,
        outcome: WorkOutcome,
    ) -> Result<DefinitionRecord, Error> {
        self.in_person("records an outcome of a task's work")?;
        let mut l = self.ledger.lock().await;
        let task = l.get(task_id).await?;
        self.check_task(&task)?;
        if task.spec.definition.is_none() {
            return Err(Error::Denied(format!(
                "{task_id} names no task definition: an outcome is recorded on the definition that a task names"
            )));
        }
        let done = task.status == Status::Done;
        let (applies, needs) = match outcome {
            WorkOutcome::Reverted => (done, "work that is done"),
            WorkOutcome::Rejected => (
                done || awaits_verdict(&task),
                "work that is done or that waits for its verdict",
            ),
        };
        if !applies {
            return Err(Error::Denied(format!(
                "{task_id} is {}: {} is recorded only for {needs}",
                task.status.as_str(),
                outcome.as_str()
            )));
        }
        let recorded = self.outcome_in(&mut l, &task, outcome).await?;
        self.observe_task(&task);
        recorded.ok_or_else(|| {
            Error::Denied(
                "the grants name no platform domain, so no ledger keeps a record of a task definition"
                    .into(),
            )
        })
    }

    /// What one version of a task definition has earned: its level, the highest level
    /// that its text lets it earn, and its records. `definition` is a bare name, for
    /// the version in force, or `<name>@<entry>`. The session must be one that may
    /// read the definition's entry, and it has then read it.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] when no definition of that name is in force for this session,
    /// in the words a task that names it is refused in; a ledger or store error.
    pub async fn autonomy(&self, definition: &str) -> Result<AutonomyView, Error> {
        let def = self.readable_definition(definition).await?;
        let reference = def.reference();
        let records = match self.platform_domain() {
            Some(domain) => {
                let l = self.ledger.lock().await;
                l.definition_records(&domain, Some(&reference)).await?
            }
            None => Vec::new(),
        };
        Ok(AutonomyView {
            level: level_after(&records),
            ceiling: def.autonomy,
            definition: reference,
            records,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::tests::{root, written};
    use crate::WriteRequest;
    use dm_core::{EntryType, Label};
    use dm_ledger::TaskSpec;
    use std::path::Path;
    use time::Duration;

    /// A definition whose text lets it earn the highest level. What it has earned is
    /// on the ledger, and a new root's ledger holds nothing.
    const TIDY: &str = "# Tidy a module\n\n```platform task tidy\npurpose = \"Tidy one module.\"\nacceptance = [\"The handoff names each file that changed.\"]\noutputs = [\"handoff\"]\nautonomy = \"checks\"\n```\n";

    /// A root with the definition `tidy` verified in its platform domain `dom-plat`:
    /// the owner's session, and the reference of the version in force.
    async fn platform(dir: &Path) -> (Memory, String) {
        let owner = root(dir);
        let entry = written(&owner, TIDY, true).await;
        (owner, format!("tidy@{entry}"))
    }

    fn as_author(dir: &Path, author: &str) -> Memory {
        Memory::open(dir).unwrap().with_author(author)
    }

    fn spec(title: &str, definition: Option<&str>) -> TaskSpec {
        TaskSpec {
            domain: "dom-me".into(),
            title: title.into(),
            body: "Do it.\n".into(),
            definition: definition.map(str::to_string),
            ..TaskSpec::default()
        }
    }

    /// A task under `tidy` that the owner filed and admitted.
    async fn tidy_task(owner: &Memory, title: &str) -> Task {
        admitted(owner, spec(title, Some("tidy"))).await
    }

    async fn admitted(owner: &Memory, s: TaskSpec) -> Task {
        let t = owner.create_task(s).await.unwrap().task;
        if t.status == Status::Proposed {
            owner.admit(&t.id).await.unwrap()
        } else {
            t
        }
    }

    /// A handoff entry in `dom-me`, written in a session of the owner of its own.
    async fn handoff(dir: &Path, text: &str, classification: Option<Label>) -> String {
        let h = WriteRequest {
            body: text.into(),
            domain: Some("dom-me".into()),
            entry_type: EntryType::Handoff,
            classification,
            ..WriteRequest::default()
        };
        Memory::open(dir).unwrap().write(h).await.unwrap().id
    }

    /// A new session of `worker` that holds `task`.
    async fn holding(dir: &Path, task: &Task, worker: &str) -> Memory {
        let session = as_author(dir, worker);
        session.claim(&task.id, Duration::minutes(5)).await.unwrap();
        session
    }

    /// `task` claimed by a new session of `worker` and completed with a new handoff:
    /// the handoff, and the task as the completion left it.
    async fn handed_in(dir: &Path, task: &Task, worker: &str) -> (String, Task) {
        let session = holding(dir, task, worker).await;
        let h = handoff(dir, "Tidied.\n", None).await;
        let after = session.complete(&task.id, None, &h).await.unwrap();
        (h, after)
    }

    /// Put the level of `reference` at `to`, as a raise would: a record on the ledger.
    async fn set_level(owner: &Memory, reference: &str, to: Autonomy) {
        let raised = DefinitionChange::Raised {
            to,
            run: "def-plat-test".into(),
            signed_by: None,
        };
        let mut l = owner.ledger.lock().await;
        l.record_definition("dom-plat", reference, raised)
            .await
            .unwrap();
    }

    async fn events(m: &Memory) -> u64 {
        m.ledger.lock().await.event_count().await.unwrap()
    }

    fn denied<T>(r: &Result<T, Error>, saying: &str) -> bool {
        matches!(r, Err(Error::Denied(why)) if why.contains(saying))
    }

    fn wrong_state<T>(r: &Result<T, Error>, saying: &str) -> bool {
        matches!(
            r,
            Err(Error::Ledger(dm_ledger::Error::WrongState { op, .. })) if op.contains(saying)
        )
    }

    fn is_done(task: &Task, handoff: &str) -> bool {
        task.status == Status::Done
            && task.handoff.as_deref() == Some(handoff)
            && task.waiting_on.is_none()
    }

    async fn state(m: &Memory, id: &str) -> Status {
        m.get_task(id).await.unwrap().status
    }

    /// A task that names no definition and depends on `on`.
    fn depends_on(title: &str, on: &Task) -> TaskSpec {
        let mut after = spec(title, None);
        after.depends_on = vec![on.id.clone()];
        after
    }

    fn is_parked(task: &Task, handoff: &str) -> bool {
        task.status == Status::Blocked
            && task.waiting_on == Some(format!("verdict:{}", task.id))
            && task.handoff.as_deref() == Some(handoff)
            && task.holder.is_none()
    }

    #[test]
    fn the_level_is_where_the_last_raise_or_outcome_left_it() {
        let record = |change| DefinitionRecord {
            id: "def-plat-1".into(),
            domain: "dom-plat".into(),
            definition: "tidy@mem-plat-1".into(),
            change,
            at: time::OffsetDateTime::UNIX_EPOCH,
        };
        let raised = |to| DefinitionChange::Raised {
            to,
            run: "def-plat-run".into(),
            signed_by: None,
        };
        let graded = DefinitionChange::Graded {
            suite: "mem-held-1".into(),
            level: Autonomy::Checks,
            passed: 5,
            of: 5,
            needed: 4,
            by: "agent:evaluator".into(),
        };
        let lowered = DefinitionChange::Outcome {
            task: "task-me-1".into(),
            outcome: WorkOutcome::Reverted,
            by: "human:test:me".into(),
            to: Autonomy::Person,
        };
        assert_eq!(level_after(&[]), Autonomy::Person);
        // a graded run alone moves nothing
        let mut records = vec![record(graded.clone())];
        assert_eq!(level_after(&records), Autonomy::Person);
        records.push(record(raised(Autonomy::Review)));
        records.push(record(graded));
        assert_eq!(level_after(&records), Autonomy::Review);
        records.push(record(lowered));
        assert_eq!(level_after(&records), Autonomy::Person);
    }

    /// Rule 1. The text of the definition says `checks`; nothing on the ledger says so.
    #[tokio::test]
    async fn a_completion_at_the_level_person_parks_the_task_for_a_verdict() {
        let dir = tempfile::tempdir().unwrap();
        let (owner, reference) = platform(dir.path()).await;
        let view = owner.autonomy("tidy").await.unwrap();
        assert_eq!(
            (view.definition.as_str(), view.level, view.ceiling),
            (reference.as_str(), Autonomy::Person, Autonomy::Checks)
        );
        assert_eq!(owner.level_of(&reference).await.unwrap(), Autonomy::Person);
        let task = tidy_task(&owner, "Tidy the importer").await;
        assert_eq!(task.spec.definition.as_deref(), Some(reference.as_str()));
        let after = admitted(&owner, depends_on("Then the exporter", &task)).await;
        let worker = holding(dir.path(), &task, "agent:a").await;
        let h = handoff(dir.path(), "Tidied.\n", None).await;
        let parked = worker.complete(&task.id, None, &h).await.unwrap();
        assert!(is_parked(&parked, &h), "{parked:?}");
        assert_eq!(owner.get_task(&task.id).await.unwrap(), parked);
        // the worker's claim is over
        let lease = Duration::minutes(5);
        assert!(worker.heartbeat(&task.id, None, lease).await.is_err());
        the_parked_work_is_nobody_s_and_opens_nothing(&owner, dir.path(), &task, &after).await;
    }

    async fn the_parked_work_is_nobody_s_and_opens_nothing(
        owner: &Memory,
        dir: &Path,
        task: &Task,
        after: &Task,
    ) {
        let lease = Duration::minutes(5);
        let other = as_author(dir, "agent:b");
        assert!(other.claim(&task.id, lease).await.is_err());
        // what depends on it is not open: the work does not count yet
        assert_eq!(state(owner, &after.id).await, Status::Blocked);
        assert!(other.claim(&after.id, lease).await.is_err());
        let offered = other.claim_next(Some("dom-me"), lease).await.unwrap();
        assert!(offered.is_none(), "{offered:?}");
    }

    /// Rule 2.
    #[tokio::test]
    async fn a_completion_of_a_task_that_names_no_definition_closes_it() {
        let dir = tempfile::tempdir().unwrap();
        let (owner, _) = platform(dir.path()).await;
        let task = admitted(&owner, spec("Sweep the floor", None)).await;
        let (h, done) = handed_in(dir.path(), &task, "agent:a").await;
        assert!(is_done(&done, &h), "{done:?}");
    }

    /// Rule 3.
    #[tokio::test]
    async fn a_completion_at_the_level_checks_closes_the_task_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let (owner, reference) = platform(dir.path()).await;
        set_level(&owner, &reference, Autonomy::Checks).await;
        assert_eq!(owner.level_of(&reference).await.unwrap(), Autonomy::Checks);
        let task = tidy_task(&owner, "Tidy the importer").await;
        let after = admitted(&owner, depends_on("Then the exporter", &task)).await;
        let (h, done) = handed_in(dir.path(), &task, "agent:a").await;
        assert!(is_done(&done, &h), "{done:?}");
        assert_eq!(state(&owner, &after.id).await, Status::Open);
    }

    /// Rule 4.
    #[tokio::test]
    async fn a_completion_with_an_unsettled_effect_is_refused_at_every_level() {
        let dir = tempfile::tempdir().unwrap();
        let (owner, reference) = platform(dir.path()).await;
        for level in [Autonomy::Person, Autonomy::Review, Autonomy::Checks] {
            set_level(&owner, &reference, level).await;
            let held = refused_for_its_effect(&owner, dir.path()).await;
            assert_eq!(
                (held.status, held.waiting_on, held.handoff),
                (Status::Claimed, None, None),
                "{level}: nothing is parked"
            );
        }
    }

    /// A task under `tidy` with an effect begun, whose completion is refused; the task
    /// after the refusal.
    async fn refused_for_its_effect(owner: &Memory, dir: &Path) -> Task {
        let task = tidy_task(owner, "Push the release").await;
        let worker = holding(dir, &task, "agent:a").await;
        worker.effect_begun(&task.id, None, "push-1").await.unwrap();
        let h = handoff(dir, "Pushed.\n", None).await;
        let before = events(owner).await;
        let refused = worker.complete(&task.id, None, &h).await;
        assert!(
            wrong_state(&refused, "effect of it is not settled"),
            "{refused:?}"
        );
        assert_eq!(events(owner).await, before, "nothing appended");
        owner.get_task(&task.id).await.unwrap()
    }

    /// Rule 5.
    #[tokio::test]
    async fn a_completion_asked_again_while_the_task_awaits_its_verdict_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (owner, _) = platform(dir.path()).await;
        let task = tidy_task(&owner, "Tidy the importer").await;
        let worker = holding(dir.path(), &task, "agent:a").await;
        let h = handoff(dir.path(), "Tidied.\n", None).await;
        let key = Some("done-1");
        let parked = worker.complete_once(&task.id, None, &h, key).await.unwrap();
        assert!(is_parked(&parked, &h), "{parked:?}");
        let before = events(&owner).await;
        // with the key, without it, and from another session of the worker
        let again = worker.complete_once(&task.id, None, &h, key).await;
        assert_eq!(again.unwrap(), parked);
        let again = worker.complete(&task.id, None, &h).await;
        assert_eq!(again.unwrap(), parked);
        let later = as_author(dir.path(), "agent:a");
        let again = later.complete_once(&task.id, None, &h, key).await;
        assert_eq!(again.unwrap(), parked);
        another_completion_is_refused(dir.path(), &worker, &parked).await;
        assert_eq!(events(&owner).await, before, "nothing appended");
        assert_eq!(owner.get_task(&task.id).await.unwrap(), parked);
    }

    /// Another handoff is another completion, and so is another principal's.
    async fn another_completion_is_refused(dir: &Path, worker: &Memory, parked: &Task) {
        let h = parked.handoff.clone().unwrap();
        let other = handoff(dir, "Something else.\n", None).await;
        let id = parked.id.as_str();
        let changed = worker
            .complete_once(id, Some(1), &other, Some("done-1"))
            .await;
        assert!(wrong_state(&changed, "complete"), "{changed:?}");
        let changed = worker.complete(id, None, &other).await;
        assert!(changed.is_err(), "{changed:?}");
        let onlooker = as_author(dir, "agent:b");
        let theirs = onlooker.complete(id, Some(1), &h).await;
        assert!(wrong_state(&theirs, "complete"), "{theirs:?}");
    }

    /// Rule 6.
    #[tokio::test]
    async fn a_person_s_accept_closes_the_task_its_dependent_opens_and_what_it_continues_closes() {
        let dir = tempfile::tempdir().unwrap();
        let (owner, _) = platform(dir.path()).await;
        let resumed = admitted(&owner, spec("Tidy the importer", None)).await;
        let mut continuation = spec("Resume: tidy the importer", Some("tidy"));
        continuation.origin = Some(dm_ledger::continues(&resumed.id));
        let continuation = admitted(&owner, continuation).await;
        let after = admitted(&owner, depends_on("Then the exporter", &continuation)).await;
        let (h, parked) = handed_in(dir.path(), &continuation, "agent:a").await;
        assert!(is_parked(&parked, &h), "{parked:?}");
        // the completion closed nothing up the chain
        assert_eq!(state(&owner, &resumed.id).await, Status::Open);
        let verdict = owner.verdict(&continuation.id, true).await.unwrap();
        assert_eq!(
            (verdict.accepted, verdict.level, &verdict.outcome),
            (true, Autonomy::Person, &None)
        );
        assert!(is_done(&verdict.task, &h), "{:?}", verdict.task);
        the_accept_reached_the_ledger(&owner, &verdict.task, &after, &resumed).await;
    }

    async fn the_accept_reached_the_ledger(
        owner: &Memory,
        done: &Task,
        after: &Task,
        resumed: &Task,
    ) {
        assert_eq!(owner.get_task(&done.id).await.unwrap(), *done);
        assert_eq!(state(owner, &after.id).await, Status::Open);
        let closed = owner.get_task(&resumed.id).await.unwrap();
        assert!(
            is_done(&closed, done.handoff.as_deref().unwrap()),
            "{closed:?}"
        );
        // a verdict is given once
        let again = owner.verdict(&done.id, true).await;
        assert!(denied(&again, "does not wait for a verdict"), "{again:?}");
    }

    /// Rule 7.
    #[tokio::test]
    async fn at_the_level_person_only_a_person_in_person_gives_the_verdict() {
        let dir = tempfile::tempdir().unwrap();
        let (owner, _) = platform(dir.path()).await;
        let task = tidy_task(&owner, "Tidy the importer").await;
        let (h, parked) = handed_in(dir.path(), &task, "agent:a").await;
        let before = events(&owner).await;
        let agent = as_author(dir.path(), "agent:b");
        let served_owner = Memory::open(dir.path()).unwrap();
        served_owner.mark_served();
        let served_person = as_author(dir.path(), "human:test:reviewer");
        served_person.mark_served();
        for session in [&agent, &served_owner, &served_person] {
            for accept in [true, false] {
                let refused = session.verdict(&task.id, accept).await;
                let who = session.author();
                assert!(denied(&refused, "only a person"), "{who}: {refused:?}");
            }
        }
        nobody_reports_the_event_of_the_verdict(&owner, dir.path(), &task).await;
        assert_eq!(events(&owner).await, before, "nothing appended");
        assert_eq!(owner.get_task(&task.id).await.unwrap(), parked);
        // a person who is not the owner, in a session that no harness reaches
        let reviewer = as_author(dir.path(), "human:test:reviewer");
        let verdict = reviewer.verdict(&task.id, true).await.unwrap();
        assert!(is_done(&verdict.task, &h), "{:?}", verdict.task);
    }

    /// The event that the task waits on is no way around the verdict.
    async fn nobody_reports_the_event_of_the_verdict(owner: &Memory, dir: &Path, task: &Task) {
        let event = format!("verdict:{}", task.id);
        let tap = as_author(dir, "tap:checks");
        for session in [&tap, owner] {
            let reported = session.event_arrived(Some("dom-me"), &event).await;
            assert!(denied(&reported, "verdict on the work"), "{reported:?}");
        }
    }

    /// Rule 8.
    #[tokio::test]
    async fn at_the_level_review_another_principal_gives_the_verdict_and_nobody_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let (owner, reference) = platform(dir.path()).await;
        // at the level person, a person does not give the verdict on its own work
        let own = tidy_task(&owner, "Tidy the exporter").await;
        let (_, parked) = handed_in(dir.path(), &own, "human:test:me").await;
        let refused = owner.verdict(&own.id, true).await;
        assert!(denied(&refused, "its own work"), "{refused:?}");
        assert_eq!(owner.get_task(&own.id).await.unwrap(), parked);
        set_level(&owner, &reference, Autonomy::Review).await;
        let task = tidy_task(&owner, "Tidy the importer").await;
        let (h, parked) = handed_in(dir.path(), &task, "agent:a").await;
        assert!(is_parked(&parked, &h), "{parked:?}");
        // another session of the worker is the worker
        let worker = as_author(dir.path(), "agent:a");
        for accept in [true, false] {
            let refused = worker.verdict(&task.id, accept).await;
            assert!(denied(&refused, "its own work"), "{refused:?}");
        }
        assert_eq!(owner.get_task(&task.id).await.unwrap(), parked);
        // another agent reviews, in a session that the memory server serves
        let reviewer = as_author(dir.path(), "agent:b");
        reviewer.mark_served();
        let verdict = reviewer.verdict(&task.id, true).await.unwrap();
        assert_eq!((verdict.accepted, verdict.level), (true, Autonomy::Review));
        assert!(is_done(&verdict.task, &h), "{:?}", verdict.task);
    }

    /// Rule 9, the task.
    #[tokio::test]
    async fn a_verdict_is_given_only_on_a_task_that_awaits_one() {
        let dir = tempfile::tempdir().unwrap();
        let (owner, _) = platform(dir.path()).await;
        let open = tidy_task(&owner, "Not handed in").await;
        let bare = admitted(&owner, spec("Sweep the floor", None)).await;
        handed_in(dir.path(), &bare, "agent:a").await;
        // a task that waits on another event, with a handoff left on it
        let waits = tidy_task(&owner, "Waits on a check").await;
        let waiter = holding(dir.path(), &waits, "agent:a").await;
        let left = handoff(dir.path(), "The change is open.\n", None).await;
        waiter
            .wait_for(&waits.id, None, "check:42", Some(&left))
            .await
            .unwrap();
        for task in [&open, &bare, &waits] {
            for accept in [true, false] {
                let refused = owner.verdict(&task.id, accept).await;
                assert!(
                    denied(&refused, "does not wait for a verdict"),
                    "{}: {refused:?}",
                    task.spec.title
                );
            }
        }
        assert_eq!(state(&owner, &waits.id).await, Status::Blocked);
        let missing = owner.verdict("task-me-nowhere", true).await;
        assert!(
            matches!(missing, Err(Error::Ledger(dm_ledger::Error::NotFound(_)))),
            "{missing:?}"
        );
    }

    /// Rule 9, the handoff: one above what the session may read.
    #[tokio::test]
    async fn a_verdict_is_given_only_by_a_session_that_may_read_the_handoff() {
        let dir = tempfile::tempdir().unwrap();
        let (owner, _) = platform(dir.path()).await;
        let task = tidy_task(&owner, "Tidy the importer").await;
        let worker = holding(dir.path(), &task, "agent:a").await;
        let h = handoff(dir.path(), "Tidied.\n", Some(Label::Restricted)).await;
        let parked = worker.complete(&task.id, None, &h).await.unwrap();
        let capped =
            as_author(dir.path(), "human:test:reviewer").with_handling_cap(Label::Internal);
        assert_eq!(capped.get_task(&task.id).await.unwrap(), parked);
        let before = events(&owner).await;
        for accept in [true, false] {
            let unread = capped.verdict(&task.id, accept).await;
            assert!(matches!(unread, Err(Error::NotFound(_))), "{unread:?}");
        }
        assert_eq!(events(&owner).await, before, "nothing appended");
        assert_eq!(owner.get_task(&task.id).await.unwrap(), parked);
        let reviewer = as_author(dir.path(), "human:test:reviewer");
        let verdict = reviewer.verdict(&task.id, true).await.unwrap();
        assert!(is_done(&verdict.task, &h), "{:?}", verdict.task);
        assert_eq!(reviewer.label(), Label::Restricted, "it read the handoff");
    }

    /// Rule 10.
    #[tokio::test]
    async fn a_reject_puts_the_task_back_on_offer_and_lowers_the_level_one_step() {
        let dir = tempfile::tempdir().unwrap();
        let (owner, reference) = platform(dir.path()).await;
        set_level(&owner, &reference, Autonomy::Review).await;
        let task = tidy_task(&owner, "Tidy the importer").await;
        let (h, _) = handed_in(dir.path(), &task, "agent:a").await;
        let reviewer = as_author(dir.path(), "agent:b");
        let verdict = reviewer.verdict(&task.id, false).await.unwrap();
        assert_eq!((verdict.accepted, verdict.level), (false, Autonomy::Review));
        let back = &verdict.task;
        assert_eq!(
            (back.status, &back.waiting_on, back.handoff.as_deref()),
            (Status::Open, &None, Some(h.as_str())),
            "on offer again, with the handoff"
        );
        let outcome = verdict.outcome.clone().unwrap();
        let lowered = DefinitionChange::Outcome {
            task: task.id.clone(),
            outcome: WorkOutcome::Rejected,
            by: "agent:b".into(),
            to: Autonomy::Person,
        };
        assert_eq!(
            (&outcome.definition, &outcome.change),
            (&reference, &lowered)
        );
        assert_eq!(owner.level_of(&reference).await.unwrap(), Autonomy::Person);
        let view = owner.autonomy(&reference).await.unwrap();
        assert_eq!((view.level, view.records.len()), (Autonomy::Person, 2));
        assert_eq!(view.records[1], outcome, "after the raise");
        a_second_reject_of_the_task_records_nothing_more(&owner, dir.path(), &task, &outcome).await;
    }

    /// The task is worked again, by the agent that rejected it, and rejected again.
    async fn a_second_reject_of_the_task_records_nothing_more(
        owner: &Memory,
        dir: &Path,
        task: &Task,
        first: &DefinitionRecord,
    ) {
        let (h, parked) = handed_in(dir, task, "agent:b").await;
        assert!(is_parked(&parked, &h), "{parked:?}");
        assert_eq!(parked.generation, 2);
        // the level is person now: the first worker is an agent, and gives no verdict
        let agent = as_author(dir, "agent:a");
        let refused = agent.verdict(&task.id, false).await;
        assert!(denied(&refused, "only a person"), "{refused:?}");
        let verdict = owner.verdict(&task.id, false).await.unwrap();
        assert_eq!(
            (verdict.level, verdict.task.status),
            (Autonomy::Person, Status::Open)
        );
        assert_eq!(verdict.outcome.as_ref(), Some(first), "once for each task");
        let view = owner.autonomy("tidy").await.unwrap();
        assert_eq!((view.level, view.records.len()), (Autonomy::Person, 2));
    }

    fn reverted(task: &Task, to: Autonomy) -> DefinitionChange {
        DefinitionChange::Outcome {
            task: task.id.clone(),
            outcome: WorkOutcome::Reverted,
            by: "human:test:me".into(),
            to,
        }
    }

    /// Rule 11.
    #[tokio::test]
    async fn a_reverted_outcome_lowers_the_level_one_step_once_for_each_task() {
        let dir = tempfile::tempdir().unwrap();
        let (owner, reference) = platform(dir.path()).await;
        set_level(&owner, &reference, Autonomy::Checks).await;
        let mut done = Vec::new();
        for title in ["Tidy the importer", "Tidy the exporter", "Tidy the index"] {
            let task = tidy_task(&owner, title).await;
            done.push(handed_in(dir.path(), &task, "agent:a").await.1);
        }
        let first = owner
            .record_outcome(&done[0].id, WorkOutcome::Reverted)
            .await
            .unwrap();
        assert_eq!(first.change, reverted(&done[0], Autonomy::Review));
        assert_eq!(owner.level_of(&reference).await.unwrap(), Autonomy::Review);
        the_same_outcome_again_appends_nothing(&owner, &done[0], &first).await;
        assert_eq!(owner.level_of(&reference).await.unwrap(), Autonomy::Review);
        // another task's outcome is another step, and the lowest level stays
        for task in &done[1..] {
            let lowered = owner
                .record_outcome(&task.id, WorkOutcome::Reverted)
                .await
                .unwrap();
            assert_eq!(lowered.change, reverted(task, Autonomy::Person));
        }
        let view = owner.autonomy(&reference).await.unwrap();
        assert_eq!((view.level, view.records.len()), (Autonomy::Person, 4));
    }

    async fn the_same_outcome_again_appends_nothing(
        owner: &Memory,
        task: &Task,
        first: &DefinitionRecord,
    ) {
        let before = events(owner).await;
        let again = owner
            .record_outcome(&task.id, WorkOutcome::Reverted)
            .await
            .unwrap();
        assert_eq!(&again, first);
        assert_eq!(events(owner).await, before, "nothing appended");
        // the task itself is as it was: done
        assert_eq!(owner.get_task(&task.id).await.unwrap(), *task);
        assert_eq!(task.status, Status::Done);
    }

    /// Rule 12.
    #[tokio::test]
    async fn an_outcome_is_a_person_s_to_record_and_only_on_work_under_a_definition() {
        let dir = tempfile::tempdir().unwrap();
        let (owner, reference) = platform(dir.path()).await;
        set_level(&owner, &reference, Autonomy::Checks).await;
        let task = tidy_task(&owner, "Tidy the importer").await;
        handed_in(dir.path(), &task, "agent:a").await;
        // a task that names no definition has no level to lower
        let bare = admitted(&owner, spec("Sweep the floor", None)).await;
        handed_in(dir.path(), &bare, "agent:a").await;
        let open = tidy_task(&owner, "Not handed in").await;
        let before = events(&owner).await;
        let agent = as_author(dir.path(), "agent:b");
        let served = Memory::open(dir.path()).unwrap();
        served.mark_served();
        for outcome in [WorkOutcome::Reverted, WorkOutcome::Rejected] {
            for session in [&agent, &served] {
                let refused = session.record_outcome(&task.id, outcome).await;
                assert!(denied(&refused, "only a person"), "{refused:?}");
            }
            // work that is not done is not reverted, and work not handed in is not rejected
            let refused = owner.record_outcome(&open.id, outcome).await;
            assert!(denied(&refused, "is recorded only for"), "{refused:?}");
        }
        let refused = owner.record_outcome(&bare.id, WorkOutcome::Reverted).await;
        assert!(denied(&refused, "names no task definition"), "{refused:?}");
        assert_eq!(events(&owner).await, before, "nothing appended");
        assert_eq!(owner.level_of(&reference).await.unwrap(), Autonomy::Checks);
    }

    /// Rule 13: the level that the version earned is not the level of a task whose
    /// definition left force. Rule 12, for work that waits for its verdict: it is not
    /// done, so it is not reverted.
    #[tokio::test]
    async fn a_task_whose_definition_left_force_is_completed_at_the_level_person() {
        let dir = tempfile::tempdir().unwrap();
        let (owner, reference) = platform(dir.path()).await;
        set_level(&owner, &reference, Autonomy::Checks).await;
        let task = tidy_task(&owner, "Tidy the importer").await;
        let worker = holding(dir.path(), &task, "agent:a").await;
        let entry = reference.trim_start_matches("tidy@");
        owner.store.demote("dom-plat", entry).await.unwrap();
        assert_eq!(owner.level_of(&reference).await.unwrap(), Autonomy::Checks);
        let h = handoff(dir.path(), "Tidied.\n", None).await;
        let parked = worker.complete(&task.id, None, &h).await.unwrap();
        assert!(is_parked(&parked, &h), "{parked:?}");
        let refused = owner.record_outcome(&task.id, WorkOutcome::Reverted).await;
        assert!(denied(&refused, "is recorded only for"), "{refused:?}");
        // and the verdict is a person's
        let agent = as_author(dir.path(), "agent:b");
        let refused = agent.verdict(&task.id, true).await;
        assert!(denied(&refused, "only a person"), "{refused:?}");
        let verdict = owner.verdict(&task.id, true).await.unwrap();
        assert_eq!(verdict.level, Autonomy::Person);
        assert!(is_done(&verdict.task, &h), "{:?}", verdict.task);
    }

    /// A definition that a session may not read is, to that session, one that does not
    /// exist: what it has earned is not told either.
    #[tokio::test]
    async fn what_a_definition_has_earned_is_read_under_the_session_s_filter() {
        let dir = tempfile::tempdir().unwrap();
        let owner = root(dir.path());
        let restricted = WriteRequest {
            body: TIDY.into(),
            domain: Some("dom-plat".into()),
            entry_type: EntryType::Convention,
            classification: Some(Label::Restricted),
            ..WriteRequest::default()
        };
        let entry = owner.write(restricted).await.unwrap().id;
        let mut e = owner.store.get("dom-plat", &entry).await.unwrap().unwrap();
        e.front.trust_tier = dm_core::TrustTier::Verified;
        owner.store.put(&e).await.unwrap();
        let reference = format!("tidy@{entry}");
        set_level(&owner, &reference, Autonomy::Review).await;
        let low = Memory::open(dir.path())
            .unwrap()
            .with_handling_cap(Label::Public);
        let words = |r: Result<AutonomyView, Error>| match r {
            Err(Error::Denied(why)) => why.replace("no-such", "tidy"),
            other => format!("{other:?}"),
        };
        let hidden = words(low.autonomy(&reference).await);
        assert!(hidden.contains("no task definition tidy is in force"));
        assert_eq!(hidden, words(low.autonomy("tidy").await));
        assert_eq!(hidden, words(low.autonomy("no-such").await));
        assert_eq!(low.label(), Label::Public);
        let reader = Memory::open(dir.path()).unwrap();
        let view = reader.autonomy("tidy").await.unwrap();
        assert_eq!(
            (view.definition, view.level, view.records.len()),
            (reference, Autonomy::Review, 1)
        );
        assert_eq!(reader.label(), Label::Restricted, "it read the entry");
    }
}
