//! What a tap reads and records on the way back out: the domain's completed tasks that
//! came from upstream, with their handoffs, and where each handoff was posted. The one
//! read a tap has is bounded by its ceiling here, so a handoff above it is never handed
//! out and the tap's label stays where its grant allows it to send.

use crate::{Error, Memory};
use dm_core::{Entry, Label};
use dm_ledger::Task;

impl Memory {
    /// Completed tasks in a granted domain that came from upstream and whose handoff has
    /// not been posted back, oldest first, each with its handoff. A task above `ceiling`,
    /// or one whose handoff is, is left out rather than read; so is a task whose handoff
    /// is gone. Each handoff handed out is a read, logged and counted like any other.
    ///
    /// # Errors
    ///
    /// An ungranted domain, or a ledger, index or store error.
    pub async fn unposted(
        &self,
        domain: &str,
        ceiling: Label,
    ) -> Result<Vec<(Task, Entry)>, Error> {
        let domain = self.resolve_domain(Some(domain))?;
        let tasks = self.ledger.lock().await.unposted(&domain).await?;
        let mut filter = self.grants.read_filter();
        filter.handling = filter.handling.min(ceiling);
        let mut out = Vec::new();
        for task in tasks {
            if task.spec.classification > ceiling {
                continue;
            }
            let Some(handoff) = task.handoff.as_deref() else {
                continue;
            };
            // the index says whether the handoff is under the ceiling before the record
            // serves it, so nothing above the ceiling raises the session's label
            let under = self.index.lock().await.meta(handoff, &filter)?;
            if under.is_none() {
                continue;
            }
            let entry = match self.get(handoff).await {
                Ok(e) => e,
                Err(Error::NotFound(_)) => continue,
                Err(e) => return Err(e),
            };
            // the record's word, should the index be behind it
            if entry.front.classification > ceiling {
                continue;
            }
            out.push((task, entry));
        }
        Ok(out)
    }

    /// Record where a completed task's handoff was posted back, so it is posted once.
    /// Only a task of `domain` at or under `ceiling`, the tap's, since a tap may say
    /// where it posted what it was handed and nothing about the rest; `to` is one line.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for a task of another domain, above the ceiling, or a `to`
    /// that is not one line of at most 2048 characters; a ledger error, including a
    /// task that is not completed or was posted already; or a task outside the grants.
    pub async fn post(
        &self,
        id: &str,
        to: &str,
        domain: &str,
        ceiling: Label,
    ) -> Result<Task, Error> {
        if to.is_empty() || to.chars().count() > 2048 || to.chars().any(char::is_control) {
            return Err(Error::Denied(
                "where it was posted is one line of at most 2048 characters".into(),
            ));
        }
        let mut l = self.ledger.lock().await;
        let task = l.get(id).await?;
        self.task_in_grants(&task)?;
        if task.spec.domain != domain {
            return Err(Error::Denied(format!("{id} is not a task of {domain}")));
        }
        if task.spec.classification > ceiling {
            return Err(Error::Denied(format!(
                "{id} is {} which is above the ceiling {ceiling}",
                task.spec.classification
            )));
        }
        Ok(l.post(id, to).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WriteRequest;
    use dm_core::EntryType;
    use dm_ledger::TaskSpec;
    use std::path::Path;

    async fn done(m: &Memory, origin: Option<&str>, label: Label, text: Option<&str>) -> String {
        let task = m
            .create_task(TaskSpec {
                domain: "dom-me".into(),
                title: "A task".into(),
                body: "Do it.\n".into(),
                requirements: vec![],
                depends_on: vec![],
                origin: origin.map(str::to_string),
                upstream_event: origin.map(|_| "open:2026-10-01T09:00:00Z".to_string()),
                created_by: String::new(),
                classification: label,
                priority: 0,
            })
            .await
            .unwrap()
            .task;
        m.admit(&task.id).await.unwrap();
        m.claim(&task.id, time::Duration::minutes(5)).await.unwrap();
        if let Some(text) = text {
            let h = m
                .write(WriteRequest {
                    entry_type: EntryType::Handoff,
                    body: text.into(),
                    classification: Some(label),
                    ..WriteRequest::default()
                })
                .await
                .unwrap();
            m.complete(&task.id, &h.id).await.unwrap();
        }
        task.id
    }

    fn mem(dir: &Path) -> Memory {
        Memory::init(dir, "human:test:me", "dom-me")
            .unwrap()
            .without_read_log()
    }

    #[tokio::test]
    async fn unposted_hands_out_handoffs_under_the_ceiling_and_post_records_once() {
        let dir = tempfile::tempdir().unwrap();
        let m = mem(dir.path());
        let internal = done(
            &m,
            Some("github:acme/x#1"),
            Label::Internal,
            Some("Done, internally.\n"),
        )
        .await;
        // claimed, not done; before the restricted one, since a claim raises the
        // session's label and a later task would be filed at it
        let open = done(&m, Some("github:acme/x#3"), Label::Internal, None).await;
        let restricted = done(
            &m,
            Some("github:acme/x#2"),
            Label::Restricted,
            Some("Done, and the key is in the vault.\n"),
        )
        .await;
        let local = done(&m, None, Label::Internal, Some("Done here.\n")).await;

        let tap = Memory::open(dir.path())
            .unwrap()
            .with_author("tap:comments")
            .without_read_log();
        let due = tap.unposted("dom-me", Label::Internal).await.unwrap();
        let ids: Vec<&str> = due.iter().map(|(t, _)| t.id.as_str()).collect();
        assert_eq!(ids, vec![internal.as_str()], "{ids:?}");
        assert_eq!(due[0].1.body, "Done, internally.\n");
        assert_eq!(
            tap.label(),
            Label::Internal,
            "reading what is under the ceiling leaves the label under it"
        );
        let all = tap.unposted("dom-me", Label::Restricted).await.unwrap();
        assert_eq!(all.len(), 2, "at a restricted ceiling both come out");
        assert!(all.iter().any(|(t, _)| t.id == restricted));
        assert!(tap.unposted("dom-else", Label::Restricted).await.is_err());

        assert!(matches!(
            tap.post(&open, "https://x/1", "dom-me", Label::Internal)
                .await,
            Err(Error::Ledger(_))
        ));
        let posted = tap
            .post(&internal, "https://x/1", "dom-me", Label::Internal)
            .await
            .unwrap();
        assert_eq!(posted.posted_to.as_deref(), Some("https://x/1"));
        assert!(tap
            .unposted("dom-me", Label::Internal)
            .await
            .unwrap()
            .is_empty());
        assert!(matches!(
            tap.post(&internal, "https://x/2", "dom-me", Label::Internal)
                .await,
            Err(Error::Ledger(_))
        ));
        assert_eq!(m.get_task(&local).await.unwrap().posted_to, None);
    }

    /// A second domain granted, and a task from upstream completed in it by the owner;
    /// the owner's session and the task's id.
    async fn done_elsewhere(root: &Path) -> (Memory, String) {
        let grants = root.join(crate::GRANTS_FILE);
        let mut text = std::fs::read_to_string(&grants).unwrap();
        text.push_str("\n[domains.dom-other]\ndefault_classification = \"internal\"\n");
        std::fs::write(&grants, text).unwrap();
        let owner = Memory::open(root).unwrap().without_read_log();
        let other = owner
            .create_task(dm_ledger::TaskSpec {
                domain: "dom-other".into(),
                title: "Elsewhere".into(),
                body: "Do it.\n".into(),
                requirements: vec![],
                depends_on: vec![],
                origin: Some("github:acme/x#3".into()),
                upstream_event: Some("open:2026-10-01T09:00:00Z".into()),
                created_by: String::new(),
                classification: Label::Internal,
                priority: 0,
            })
            .await
            .unwrap()
            .task
            .id;
        owner.admit(&other).await.unwrap();
        owner
            .claim(&other, time::Duration::minutes(5))
            .await
            .unwrap();
        let h = owner
            .write(WriteRequest {
                domain: Some("dom-other".into()),
                entry_type: EntryType::Handoff,
                body: "Done elsewhere.\n".into(),
                ..WriteRequest::default()
            })
            .await
            .unwrap();
        owner.complete(&other, &h.id).await.unwrap();
        (owner, other)
    }

    #[tokio::test]
    async fn post_refuses_a_task_outside_the_tap_s_domain_or_above_its_ceiling() {
        let dir = tempfile::tempdir().unwrap();
        let m = mem(dir.path());
        let restricted = done(
            &m,
            Some("github:acme/x#2"),
            Label::Restricted,
            Some("The key is in the vault.\n"),
        )
        .await;
        let (owner, other) = done_elsewhere(dir.path()).await;
        let tap = Memory::open(dir.path())
            .unwrap()
            .with_author("tap:comments")
            .without_read_log();
        assert!(tap
            .unposted("dom-me", Label::Internal)
            .await
            .unwrap()
            .is_empty());
        for (id, why) in [
            (&restricted, "above the ceiling"),
            (&other, "another domain"),
        ] {
            assert!(
                matches!(
                    tap.post(id, "https://x/1", "dom-me", Label::Internal).await,
                    Err(Error::Denied(_))
                ),
                "{why}"
            );
            assert_eq!(owner.get_task(id).await.unwrap().posted_to, None, "{why}");
        }
        assert!(matches!(
            tap.post(&other, "https://x/\u{7}", "dom-other", Label::Internal)
                .await,
            Err(Error::Denied(_))
        ));
        assert!(matches!(
            tap.post(&other, &"x".repeat(3000), "dom-other", Label::Internal)
                .await,
            Err(Error::Denied(_))
        ));
        assert!(tap
            .post(&other, "https://x/1", "dom-other", Label::Internal)
            .await
            .is_ok());
    }
}
