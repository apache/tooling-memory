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

impl Memory {
    /// Give a held task up until an outside event comes: a check that runs, a reply, a
    /// change that has not landed. Nobody may claim it until a tap or a person reports
    /// the event with [`Memory::event_arrived`]. The handoff, when given, must exist
    /// and be readable here; the next holder starts from it.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for an event name that is not one token; a ledger error,
    /// including a lease already lost; a task outside the grants.
    pub async fn wait_for(
        &self,
        id: &str,
        generation: Option<u64>,
        on: &str,
        handoff: Option<&str>,
    ) -> Result<Task, Error> {
        token("an event name", on, 200)?;
        if let Some(h) = handoff {
            self.get(h).await?;
        }
        let mut l = self.ledger.lock().await;
        self.check_task(&l.get(id).await?)?;
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
    /// [`Error::Denied`] for an agent, or an event name that is not one token; an
    /// ungranted domain; a ledger error.
    pub async fn event_arrived(
        &self,
        domain: Option<&str>,
        event: &str,
    ) -> Result<Vec<Task>, Error> {
        token("an event name", event, 200)?;
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
    /// [`Error::Denied`] for an action id that is not one token; a ledger error,
    /// including a lease already lost or a claim made since; a task outside the grants.
    pub async fn effect_begun(
        &self,
        id: &str,
        generation: Option<u64>,
        action: &str,
    ) -> Result<Task, Error> {
        token("an action id", action, 64)?;
        let mut l = self.ledger.lock().await;
        self.check_task(&l.get(id).await?)?;
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
