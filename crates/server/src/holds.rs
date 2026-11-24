//! What holds a task back from a claim though nobody holds it: an outside event it
//! waits on, and an external effect whose outcome nobody has recorded. A session does
//! not wait for an event, and a lease that ran out does not say what a destination holds.

use crate::{lock, Error, Memory};
use dm_ledger::{Outcome, Task};
use dm_policy::decide::Class;

/// A name a caller gives an event or an action: one token, so it reads the same in a
/// log line, a command and a tool call.
fn token(what: &str, name: &str, most: usize) -> Result<(), Error> {
    let plain = !name.is_empty()
        && name.chars().count() <= most
        && name.chars().all(|c| c.is_ascii_graphic());
    if plain {
        Ok(())
    } else {
        Err(Error::Denied(format!(
            "{what} is 1 to {most} printable ASCII characters without a space"
        )))
    }
}

/// The event that a task under a task definition waits on is the verdict on its work. A
/// session does not wait on that name, and no session reports it: [`Memory::verdict`]
/// gives the verdict, and checks who gives it.
fn not_a_verdict(event: &str) -> Result<(), Error> {
    if event.starts_with(crate::autonomy::VERDICT_EVENT) {
        return Err(Error::Denied(format!(
            "an event named {}<task> is the verdict on the work of a task: no session waits on that name or reports it",
            crate::autonomy::VERDICT_EVENT
        )));
    }
    Ok(())
}

/// The event that a task in a landing queue waits on is the landing of its change. A
/// holder queues its change with [`Memory::queue_landing`], which checks the queue. Only
/// [`Memory::land`] reports the landing, and it checks who lands and the fleet's share.
fn not_a_landing(event: &str) -> Result<(), Error> {
    if event.starts_with(crate::measure::LANDING_EVENT) {
        return Err(Error::Denied(format!(
            "an event named {}<queue> is the landing of a change: queue_landing waits on that name, and only land reports it",
            crate::measure::LANDING_EVENT
        )));
    }
    Ok(())
}

/// Why a task is not closed, folded or handed on: an effect of it that nobody has settled.
pub(crate) fn unsettled(task: &Task) -> Option<String> {
    task.effects
        .first()
        .map(|a| format!("effect {a} of {} is not settled", task.id))
}

impl Memory {
    /// What a session writes on a task in its own words, every session that may read
    /// the task reads. A session whose label is above the task's classification has
    /// read what those sessions may not, so it writes no such words there. A filing
    /// rises to the session's label instead; a task that is filed stays where it is.
    pub(crate) fn writes_within(&self, task: &Task, what: &str) -> Result<(), Error> {
        if self.label() <= task.spec.classification {
            return Ok(());
        }
        Err(Error::Denied(format!(
            "this session has read above the classification of {}: it does not write {what} on that task",
            task.id
        )))
    }

    /// Give a held task up until an outside event comes: a check that runs, a reply, a
    /// change that has not landed. Nobody may claim it until a tap or a person reports
    /// the event with [`Memory::event_arrived`]. The handoff, when given, must exist
    /// and be readable here; the next holder starts from it.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for an event name that is not one token or that names a
    /// verdict or a landing, or from a session whose label is above the task's classification; a
    /// ledger error, including a lease already lost; a task outside the grants.
    pub async fn wait_for(
        &self,
        id: &str,
        generation: Option<u64>,
        on: &str,
        handoff: Option<&str>,
    ) -> Result<Task, Error> {
        token("an event name", on, 200)?;
        not_a_verdict(on)?;
        not_a_landing(on)?;
        if let Some(h) = handoff {
            self.get(h).await?;
        }
        let mut l = self.ledger.lock().await;
        let task = l.get(id).await?;
        self.check_task(&task)?;
        self.writes_within(&task, "an event name")?;
        let by = self.under(self.claim_of(id, generation)?);
        let t = l.wait_for(id, by, on, handoff).await?;
        lock(&self.claims).remove(id);
        self.observe_task(&t);
        Ok(t)
    }

    /// Report that an outside event came: each task of the domain that waited on it is
    /// open again, for the service to offer. A tap reports it, or a person. An agent
    /// does not: it would be the agent that says its own wait is over. The tasks this
    /// session may see are returned, each read.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for an agent, or an event name that is not one token or that
    /// names a verdict or a landing; an ungranted domain; a ledger error.
    pub async fn event_arrived(
        &self,
        domain: Option<&str>,
        event: &str,
    ) -> Result<Vec<Task>, Error> {
        token("an event name", event, 200)?;
        not_a_verdict(event)?;
        not_a_landing(event)?;
        if Class::of(&self.author, &self.grants.owner) == Class::Agent {
            return Err(Error::Denied(
                "an agent does not report the event a task waits on: a tap or a person does".into(),
            ));
        }
        let domain = self.resolve_domain(domain)?;
        let woken = self.ledger.lock().await.arrived(&domain, event).await?;
        self.read_tasks(woken)
    }

    /// Record that this session is about to make an external action under its claim,
    /// before it makes it. Until the action is settled, the task goes to nobody else,
    /// whatever its lease says. A retry of the action names the same id.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for an action id that is not one token, or from a session
    /// whose label is above the task's classification; a ledger error, including a
    /// lease already lost or a claim made since; a task outside the grants.
    pub async fn effect_begun(
        &self,
        id: &str,
        generation: Option<u64>,
        action: &str,
    ) -> Result<Task, Error> {
        token("an action id", action, 64)?;
        let mut l = self.ledger.lock().await;
        let task = l.get(id).await?;
        self.check_task(&task)?;
        self.writes_within(&task, "an action id")?;
        let by = self.under(self.claim_of(id, generation)?);
        let t = l.effect_begun(id, by, action).await?;
        self.observe_task(&t);
        Ok(t)
    }

    /// Record what became of an external action. The holder says so while its claim is
    /// in force: it had the destination's answer. After that, a person or a tap that
    /// looked at the destination says so; an agent that lost its claim does not, since
    /// it cannot tell a lost answer from an action that was not made.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for an action id that is not one token; a ledger error: for an
    /// agent without the claim in force, the task's holder check; for an action the
    /// task did not begin, a wrong state; a task outside the grants.
    pub async fn effect_settled(
        &self,
        id: &str,
        generation: Option<u64>,
        action: &str,
        outcome: Outcome,
    ) -> Result<Task, Error> {
        token("an action id", action, 64)?;
        let mut l = self.ledger.lock().await;
        self.check_task(&l.get(id).await?)?;
        let reconciles = Class::of(&self.author, &self.grants.owner) != Class::Agent;
        let own = lock(&self.claims).get(id).copied();
        // a person or a tap that made no claim here reconciles; everyone else speaks
        // under a claim, and the ledger checks it
        let held = if reconciles && own.is_none() {
            None
        } else {
            Some(self.under(self.claim_of(id, generation)?))
        };
        let t = l
            .effect_settled(id, action, outcome, (&self.author, held))
            .await?;
        self.observe_task(&t);
        Ok(t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dm_core::Label;
    use dm_ledger::{Status, TaskSpec};
    use std::path::Path;

    fn mem(dir: &Path) -> Memory {
        Memory::init(dir, "human:test:me", "dom-me")
            .unwrap()
            .without_read_log()
    }

    fn as_author(dir: &Path, author: &str) -> Memory {
        Memory::open(dir)
            .unwrap()
            .with_author(author)
            .without_read_log()
    }

    /// A task the owner filed and admitted, claimed by `agent:a`; that session and the id.
    async fn held(owner: &Memory, dir: &Path, title: &str) -> (Memory, String) {
        let task = owner
            .create_task(TaskSpec {
                title: title.into(),
                body: "Do it.\n".into(),
                ..TaskSpec::default()
            })
            .await
            .unwrap()
            .task;
        owner.admit(&task.id).await.unwrap();
        let agent = as_author(dir, "agent:a");
        agent
            .claim(&task.id, time::Duration::minutes(5))
            .await
            .unwrap();
        (agent, task.id)
    }

    fn denied<T>(r: &Result<T, Error>, saying: &str) -> bool {
        matches!(r, Err(Error::Denied(why)) if why.contains(saying))
    }

    #[tokio::test]
    async fn a_task_that_waits_is_offered_again_when_a_tap_reports_the_event() {
        let dir = tempfile::tempdir().unwrap();
        let owner = mem(dir.path());
        let (agent, id) = held(&owner, dir.path(), "Open the change and wait").await;
        let spaced = agent.wait_for(&id, None, "check 42", None).await;
        assert!(denied(&spaced, "an event name"));
        let waiting = agent.wait_for(&id, None, "check:42", None).await.unwrap();
        assert_eq!(waiting.status, Status::Blocked);
        assert_eq!(waiting.waiting_on.as_deref(), Some("check:42"));
        // the session that gave it up holds it no more, and nobody else gets it
        assert!(agent
            .heartbeat(&id, None, time::Duration::minutes(5))
            .await
            .is_err());
        let other = as_author(dir.path(), "agent:b");
        assert!(other.claim(&id, time::Duration::minutes(5)).await.is_err());
        // an agent does not say that its own wait is over
        let by_agent = other.event_arrived(None, "check:42").await;
        assert!(denied(&by_agent, "a tap or a person"));
        assert_eq!(owner.get_task(&id).await.unwrap().status, Status::Blocked);
        let tap = as_author(dir.path(), "tap:checks");
        assert!(tap
            .event_arrived(None, "check:41")
            .await
            .unwrap()
            .is_empty());
        let woken = tap.event_arrived(None, "check:42").await.unwrap();
        assert_eq!(woken.len(), 1);
        assert_eq!(
            (woken[0].id.as_str(), woken[0].status),
            (id.as_str(), Status::Open)
        );
        let again = other.claim(&id, time::Duration::minutes(5)).await.unwrap();
        assert_eq!(again.generation, 2);
    }

    #[tokio::test]
    async fn an_effect_is_settled_by_its_holder_or_by_a_person_and_by_no_other_agent() {
        let dir = tempfile::tempdir().unwrap();
        let owner = mem(dir.path());
        let (agent, id) = held(&owner, dir.path(), "Post the release note").await;
        let spaced = agent.effect_begun(&id, None, "act 1").await;
        assert!(denied(&spaced, "an action id"));
        let begun = agent.effect_begun(&id, None, "act-1").await.unwrap();
        assert_eq!(begun.effects, vec!["act-1".to_string()]);
        // another agent's session made no claim, and names the holder's generation
        let other = as_author(dir.path(), "agent:b");
        let unheld = other
            .effect_settled(&id, None, "act-1", Outcome::Applied)
            .await;
        assert!(matches!(unheld, Err(Error::Ledger(_))), "{unheld:?}");
        let named = other
            .effect_settled(&id, Some(begun.generation), "act-1", Outcome::Applied)
            .await;
        assert!(matches!(named, Err(Error::Ledger(_))), "{named:?}");
        assert_eq!(owner.get_task(&id).await.unwrap().effects.len(), 1);
        // the holder had the destination's answer
        let own = agent
            .effect_settled(&id, None, "act-1", Outcome::Applied)
            .await
            .unwrap();
        assert!(own.effects.is_empty());
        // and a person reconciles one whose holder is gone
        agent.effect_begun(&id, None, "act-2").await.unwrap();
        let person = as_author(dir.path(), "human:test:me");
        let reconciled = person
            .effect_settled(&id, None, "act-2", Outcome::NotApplied)
            .await
            .unwrap();
        assert!(reconciled.effects.is_empty());
    }

    /// Rule 14 of the autonomy levels: the event of a verdict is the service's name.
    #[tokio::test]
    async fn no_session_waits_on_the_event_of_a_verdict_or_reports_it() {
        let dir = tempfile::tempdir().unwrap();
        let owner = mem(dir.path());
        let (agent, id) = held(&owner, dir.path(), "Tidy the importer").await;
        let event = format!("verdict:{id}");
        let waits = agent.wait_for(&id, None, &event, None).await;
        assert!(denied(&waits, "verdict on the work"), "{waits:?}");
        let held = owner.get_task(&id).await.unwrap();
        assert_eq!((held.status, held.waiting_on), (Status::Claimed, None));
        let tap = as_author(dir.path(), "tap:checks");
        for session in [&tap, &owner] {
            for name in [event.as_str(), "verdict:task-me-other", "verdict:"] {
                let reported = session.event_arrived(None, name).await;
                assert!(denied(&reported, "verdict on the work"), "{reported:?}");
            }
        }
        // a name that only holds the word is an event like any other
        let other = "check:verdict:1";
        agent.wait_for(&id, None, other, None).await.unwrap();
        let woken = tap.event_arrived(None, other).await.unwrap();
        assert_eq!(woken.len(), 1);
    }

    fn not_handed_on<T: std::fmt::Debug>(r: &Result<T, Error>, id: &str) -> bool {
        matches!(
            r,
            Err(Error::Ledger(dm_ledger::Error::WrongState { id: held, op, .. }))
                if held == id && op.contains("hand its work on")
        )
    }

    fn part() -> Vec<(String, String)> {
        vec![("A part".to_string(), "Do the part.\n".to_string())]
    }

    #[tokio::test]
    async fn a_task_with_an_unsettled_effect_is_not_retried_fanned_out_or_closed() {
        let dir = tempfile::tempdir().unwrap();
        let owner = mem(dir.path());
        let (agent, id) = held(&owner, dir.path(), "Push the release").await;
        agent.effect_begun(&id, None, "push-1").await.unwrap();
        agent.release(&id, None, None).await.unwrap();
        // another agent does not get the work under the task's name, or under another
        let other = as_author(dir.path(), "agent:b");
        assert!(other.claim(&id, time::Duration::minutes(5)).await.is_err());
        let retried = other.retry_task(&id).await;
        assert!(not_handed_on(&retried, &id), "{retried:?}");
        let fanned = other.fan_out(&id, &part()).await;
        assert!(not_handed_on(&fanned, &id), "{fanned:?}");
        // and nobody closes it while nobody knows what the destination holds
        for who in [&other, &owner] {
            let closed = who.invalidate(&id, "obsolete", None).await;
            assert!(denied(&closed, "effect push-1"), "{closed:?}");
        }
        let tree = owner.task_tree(&id).await.unwrap();
        assert_eq!(tree.task.status, Status::Open);
        assert_eq!(tree.task.effects, vec!["push-1".to_string()]);
        assert!(tree.children.is_empty());
        // a person looked at the destination: the work is on offer again
        owner
            .effect_settled(&id, None, "push-1", Outcome::NotApplied)
            .await
            .unwrap();
        the_retry_holds_its_task_open_in_turn(&owner, &other, &id).await;
    }

    /// The retry of `id` begins an effect and gives up: `id` is not closed over it.
    async fn the_retry_holds_its_task_open_in_turn(owner: &Memory, other: &Memory, id: &str) {
        let retry = retried_with_an_effect(other, id).await;
        let closed = owner.invalidate(id, "obsolete", None).await;
        assert!(denied(&closed, "effect push-2"), "{closed:?}");
        assert_eq!(owner.get_task(id).await.unwrap().status, Status::Open);
        owner
            .effect_settled(&retry, None, "push-2", Outcome::Applied)
            .await
            .unwrap();
        let closed = owner.invalidate(id, "obsolete", None).await.unwrap();
        assert_eq!(closed.status, Status::Invalidated);
        let below = owner.get_task(&retry).await.unwrap();
        assert_eq!(below.status, Status::Invalidated);
    }

    /// A retry of `id` that `other` filed, claimed and gave up with `push-2` begun.
    async fn retried_with_an_effect(other: &Memory, id: &str) -> String {
        let retry = other.retry_task(id).await.unwrap();
        assert_eq!(retry.status, Status::Open);
        other
            .claim(&retry.id, time::Duration::minutes(5))
            .await
            .unwrap();
        other.effect_begun(&retry.id, None, "push-2").await.unwrap();
        other.release(&retry.id, None, None).await.unwrap();
        retry.id
    }

    #[tokio::test]
    async fn a_continuation_with_an_unsettled_effect_is_not_folded_into_a_new_one() {
        let dir = tempfile::tempdir().unwrap();
        let owner = mem(dir.path());
        let (agent, id) = held(&owner, dir.path(), "Push the release").await;
        let resumes = owner
            .create_task(TaskSpec {
                title: "Resume: Push the release".into(),
                body: "Do it.\n".into(),
                origin: Some(dm_ledger::continues(&id)),
                ..TaskSpec::default()
            })
            .await
            .unwrap()
            .task;
        owner.admit(&resumes.id).await.unwrap();
        let other = as_author(dir.path(), "agent:b");
        other
            .claim(&resumes.id, time::Duration::minutes(5))
            .await
            .unwrap();
        other
            .effect_begun(&resumes.id, None, "push-1")
            .await
            .unwrap();
        other.release(&resumes.id, None, None).await.unwrap();
        // a new fan-out would close the continuation and give its work to another
        let fanned = agent.fan_out(&id, &part()).await;
        assert!(denied(&fanned, "effect push-1"), "{fanned:?}");
        let tree = owner.task_tree(&id).await.unwrap();
        assert_eq!(tree.children.len(), 1);
        assert_eq!(tree.children[0].task.status, Status::Open);
        owner
            .effect_settled(&resumes.id, None, "push-1", Outcome::Applied)
            .await
            .unwrap();
        let fanned = agent.fan_out(&id, &part()).await.unwrap();
        assert_eq!(fanned.children.len(), 1);
        let folded = owner.get_task(&resumes.id).await.unwrap();
        assert_eq!(folded.status, Status::Invalidated);
    }

    /// A session writes its own words on a task only at or below the task's
    /// classification: a session that reads the task later reads those words.
    #[tokio::test]
    async fn a_session_that_read_above_a_task_writes_no_words_of_its_own_on_it() {
        let dir = tempfile::tempdir().unwrap();
        let owner = mem(dir.path());
        let (agent, id) = held(&owner, dir.path(), "Announce the launch").await;
        let secret = owner
            .write(crate::WriteRequest {
                body: "The launch is CODENAME-BLUEFIN, in May.\n".into(),
                classification: Some(Label::Restricted),
                ..crate::WriteRequest::default()
            })
            .await
            .unwrap()
            .id;
        agent.get(&secret).await.unwrap();
        assert_eq!(agent.label(), Label::Restricted);
        let begun = agent.effect_begun(&id, None, "bluefin-may").await;
        assert!(denied(&begun, "read above"), "{begun:?}");
        let waits = agent
            .wait_for(&id, None, "launch:CODENAME-BLUEFIN", None)
            .await;
        assert!(denied(&waits, "read above"), "{waits:?}");
        let closed = agent.invalidate(&id, "BLUEFIN moved", None).await;
        assert!(denied(&closed, "read above"), "{closed:?}");
        // a session below reads the task, and none of those words
        let reader = as_author(dir.path(), "agent:c").with_handling_cap(Label::Internal);
        let seen = reader.get_task(&id).await.unwrap();
        assert_eq!(seen.spec.classification, Label::Public);
        assert_eq!((seen.status, seen.reason), (Status::Claimed, None));
        assert_eq!((seen.waiting_on, seen.effects), (None, Vec::new()));
        // on a task at its label, the session writes them
        let high = owner
            .create_task(TaskSpec {
                title: "Brief the launch team".into(),
                body: "Do it.\n".into(),
                classification: Label::Restricted,
                ..TaskSpec::default()
            })
            .await
            .unwrap()
            .task;
        owner.admit(&high.id).await.unwrap();
        agent
            .claim(&high.id, time::Duration::minutes(5))
            .await
            .unwrap();
        let begun = agent.effect_begun(&high.id, None, "bluefin-may").await;
        assert_eq!(begun.unwrap().effects, vec!["bluefin-may".to_string()]);
    }

    /// A parent that the session may not read is refused as one that does not exist.
    #[tokio::test]
    async fn a_filing_names_no_parent_that_the_session_may_not_read() {
        let dir = tempfile::tempdir().unwrap();
        let owner = mem(dir.path());
        let hidden = owner
            .create_task(TaskSpec {
                title: "Brief the launch team".into(),
                body: "Do it.\n".into(),
                classification: Label::Restricted,
                ..TaskSpec::default()
            })
            .await
            .unwrap()
            .task;
        let capped = as_author(dir.path(), "agent:c").with_handling_cap(Label::Internal);
        let mut answers = Vec::new();
        for parent in [hidden.id.as_str(), "task-me-nowhere"] {
            let filed = capped
                .create_task(TaskSpec {
                    title: "A part".into(),
                    body: "Do it.\n".into(),
                    parent: Some(parent.to_string()),
                    ..TaskSpec::default()
                })
                .await;
            match filed {
                Err(Error::Ledger(dm_ledger::Error::NotFound(named))) => {
                    assert_eq!(named, parent);
                    answers.push("not found");
                }
                other => panic!("{parent}: {other:?}"),
            }
        }
        assert_eq!(answers.len(), 2);
        assert_eq!(capped.label(), Label::Public);
        for st in [Status::Proposed, Status::Open] {
            let listed = owner.tasks(None, st).await.unwrap();
            assert!(listed.iter().all(|t| t.spec.parent.is_none()), "{listed:?}");
        }
    }

    #[tokio::test]
    async fn the_service_says_which_policy_a_task_was_filed_under_and_checks_its_parent() {
        let dir = tempfile::tempdir().unwrap();
        let owner = mem(dir.path());
        let filed = owner
            .create_task(TaskSpec {
                title: "Carry out the decision".into(),
                body: "Do it.\n".into(),
                policy_context: vec!["pol-made-up".into()],
                implements: vec!["mem-me-0847".into()],
                ..TaskSpec::default()
            })
            .await
            .unwrap()
            .task;
        assert_eq!(filed.spec.policy_context, Vec::<String>::new());
        assert_eq!(filed.spec.implements, vec!["mem-me-0847".to_string()]);
        let orphan = owner
            .create_task(TaskSpec {
                title: "A part of nothing".into(),
                body: "Do it.\n".into(),
                parent: Some("task-me-nowhere".into()),
                ..TaskSpec::default()
            })
            .await;
        assert!(orphan.is_err());
        let part = owner
            .create_task(TaskSpec {
                title: "A part".into(),
                body: "Do it.\n".into(),
                parent: Some(filed.id.clone()),
                ..TaskSpec::default()
            })
            .await
            .unwrap()
            .task;
        assert_eq!(part.spec.parent, Some(filed.id));
    }
}
