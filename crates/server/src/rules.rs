//! The rule lifecycle as a session meets it: the rules in force where it is about to
//! act, an exception asked for, an amendment proposed to a rule's owner, and the reset
//! by which a domain's owner retires what no person stands behind.
//!
//! A rule is a policy entry ([`dm_policy::record`]). Where it is in its life is its
//! block's `status` held back by its entry's tier, so every step up is a reviewed
//! promotion and every step is a version of an entry. Nothing here is a second store
//! of rules.

use crate::{Error, Memory, WriteRequest};
use dm_core::{EntryType, TrustTier};
use dm_ledger::{Task, TaskSpec};
use dm_policy::decide::Class;
use dm_policy::record::{self, Meta, Status};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// The origin of a task that proposes an amendment to a rule, before the rule's id.
pub const AMENDMENT_ORIGIN: &str = "amend:";

/// One rule in force, as [`Memory::check_policy`] gives it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RuleInForce {
    /// The rule's stable id.
    pub policy_id: String,
    /// 0 values, 1 strategy, 2 practice, 3 domain convention.
    pub layer: u8,
    /// The rule, one sentence.
    pub rule: String,
    /// Why: what an edge case is resolved from.
    pub intent: String,
    /// `enforced`, which refuses, or `advisory`, which reports.
    pub status: Status,
    /// Whether a module checks it. A rule without one binds by being read.
    pub checked: bool,
    /// Who answers for it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
}

/// What a session asks an exception for.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExceptionRequest {
    /// The rule to breach, by its policy id.
    pub breaches: String,
    /// The one task the exception holds for; the task the session runs on when omitted.
    #[serde(default)]
    pub task: Option<String>,
    /// What it covers, in words.
    pub scope: String,
    /// When it lapses, RFC 3339.
    pub expires: String,
    /// Why it is needed.
    pub justification: String,
    /// The domain to write it in; the only granted domain when omitted.
    #[serde(default)]
    pub domain: Option<String>,
}

/// An exception as filed: written, in force for nothing until a person approves it.
#[derive(Clone, Debug, Serialize)]
pub struct FiledException {
    /// The entry that holds it.
    pub id: String,
    /// Its policy id, which a decision that uses it cites.
    pub policy_id: String,
    /// Who approves: the higher the layer breached, the more senior.
    pub approver: String,
    /// The approval task.
    pub task: Box<Task>,
}

/// What a reset did.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Reset {
    /// The domain.
    pub domain: String,
    /// The rules kept, each with the person in its lineage.
    pub kept: Vec<Kept>,
    /// The rules retired, by policy id.
    pub retired: Vec<String>,
}

/// A rule a reset kept.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Kept {
    /// The rule.
    pub policy_id: String,
    /// The person who wrote it or promoted it.
    pub person: String,
}

fn is_person(author: &str, owner: &str) -> bool {
    matches!(Class::of(author, owner), Class::Owner | Class::Human)
}

impl Memory {
    /// The rules in force for an action in a domain: every enforced and advisory rule
    /// of layers 0 to 2, and the domain's own conventions. A session asks before an
    /// action it cannot take back. The rules are not chosen by the action: the action
    /// is what the read log keeps, so an audit finds that the session asked.
    ///
    /// # Errors
    ///
    /// A domain outside the grants.
    pub fn check_policy(
        &self,
        action: &str,
        domain: Option<&str>,
    ) -> Result<Vec<RuleInForce>, Error> {
        let domain = self.resolve_domain(domain)?;
        let p = self.policy();
        let mut rules: Vec<RuleInForce> = p
            .entries()
            .iter()
            .filter(|e| !e.meta.is_exception())
            .filter(|e| e.meta.layer < 3 || e.domain == domain)
            .filter_map(|e| {
                let status = p.state_of(e);
                matches!(status, Status::Enforced | Status::Advisory).then(|| RuleInForce {
                    policy_id: e.meta.id.clone(),
                    layer: e.meta.layer,
                    rule: e.meta.rule.clone(),
                    intent: e.meta.intent.clone(),
                    status,
                    checked: e.rego.is_some(),
                    owner: e.meta.owner.clone(),
                })
            })
            .collect();
        rules.sort_by(|a, b| (a.layer, &a.policy_id).cmp(&(b.layer, &b.policy_id)));
        rules.dedup_by(|a, b| a.policy_id == b.policy_id);
        self.reads
            .text("check_policy", &format!("{action} in {domain}"));
        Ok(rules)
    }

    /// The rule of a policy id, among the record's rules.
    fn rule(&self, policy_id: &str) -> Result<record::Entry, Error> {
        self.policy()
            .entries()
            .iter()
            .find(|e| e.meta.id == policy_id && !e.meta.is_exception())
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("policy {policy_id}")))
    }

    /// Who approves an exception to a rule: the rule's owner for a domain convention or
    /// a practice, and the owner of the record for strategy and values.
    fn approver_of(&self, rule: &record::Entry) -> String {
        match (&rule.meta.owner, rule.meta.layer) {
            (Some(owner), 2..) => owner.clone(),
            _ => self.grants.owner.clone(),
        }
    }

    /// Ask for an exception to a rule: for one task, until a time, with a reason. The
    /// exception is written as a policy entry and proposed for approval; it lets
    /// nothing through until a person approves it, and after its expiry it lets
    /// nothing through again.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for a rule or a task that is not there; [`Error::Denied`]
    /// for a rule of the L0 core, an expiry that is not a time to come, or no task to
    /// scope it to; and as [`Memory::write`] and [`Memory::promote`].
    pub async fn file_exception(&self, ask: ExceptionRequest) -> Result<FiledException, Error> {
        let rule = self.rule(&ask.breaches)?;
        if rule.meta.is_core() {
            return Err(Error::Denied(format!(
                "{} is a rule of the L0 core, which takes no exception",
                rule.meta.id
            )));
        }
        let now = OffsetDateTime::now_utc();
        let rfc3339 = &time::format_description::well_known::Rfc3339;
        if !OffsetDateTime::parse(&ask.expires, rfc3339).is_ok_and(|t| t > now) {
            return Err(Error::Denied(format!(
                "an exception expires at a time to come, as RFC 3339: {} is not one",
                ask.expires
            )));
        }
        let Some(task) = ask.task.clone().or_else(|| self.held_task()) else {
            return Err(Error::Denied(
                "an exception holds for one task: name the task".into(),
            ));
        };
        self.get_task(&task).await?;
        let approver = self.approver_of(&rule);
        let key = dm_core::digest(&format!("{}\n{task}\n{}", rule.meta.id, ask.expires));
        let hex = key.rsplit(':').next().unwrap_or_default();
        let policy_id = format!("exc-{}-{}", now.year(), hex.get(..8).unwrap_or(hex));
        let meta = Meta {
            id: policy_id.clone(),
            layer: rule.meta.layer,
            intent: ask.justification.clone(),
            breaches: Some(rule.meta.id.clone()),
            scope_task: Some(task.clone()),
            scope: Some(ask.scope.clone()),
            expires: Some(ask.expires.clone()),
            approver: Some(approver.clone()),
            justification: Some(ask.justification.clone()),
            ..Meta::default()
        };
        let title = format!("Exception to {} for {task}", rule.meta.id);
        let body = record::render(&title, &ask.justification, &meta).map_err(Error::Denied)?;
        let written = self
            .write(WriteRequest {
                domain: ask.domain.clone(),
                entry_type: EntryType::Policy,
                title: Some(title),
                body,
                cites: vec![rule.entry_id.clone()],
                ..WriteRequest::default()
            })
            .await?;
        let why = format!(
            "{approver} approves an exception to {}: {}",
            rule.meta.id, ask.justification
        );
        match self.promote(&written.id, TrustTier::Verified, &why).await? {
            crate::Promotion::Proposed { task } => Ok(FiledException {
                id: written.id,
                policy_id,
                approver,
                task,
            }),
            crate::Promotion::Promoted { .. } => Err(Error::Denied(format!(
                "{} was verified without a proposal: an exception takes an approver",
                written.id
            ))),
        }
    }

    /// Propose an amendment to a rule: an ordinary task for a person, which names the
    /// rule's owner and carries the evidence. The same evidence against the same rule
    /// is one task.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for a rule that is not there; [`Error::Denied`] for no
    /// evidence; a ledger error, or a filing the policy refuses.
    pub async fn propose_amendment(
        &self,
        policy_id: &str,
        evidence: &str,
    ) -> Result<dm_ledger::Created, Error> {
        let rule = self.rule(policy_id)?;
        if evidence.trim().is_empty() {
            return Err(Error::Denied(
                "an amendment carries its evidence: what happened, and the entries that show it"
                    .into(),
            ));
        }
        let owner = rule
            .meta
            .owner
            .clone()
            .unwrap_or_else(|| self.grants.owner.clone());
        let domain = if rule.domain.is_empty() {
            self.resolve_domain(None)?
        } else {
            rule.domain.clone()
        };
        let body = format!(
            "For {owner}, who owns {id}.\n\nThe rule: {rule}\n\nIts intent: {intent}\n\n\
             The evidence:\n\n{evidence}\n\nAmend the rule by superseding its entry `{entry}`, \
             retire it, or close this task with why the rule stands.",
            id = rule.meta.id,
            rule = rule.meta.rule,
            intent = rule.meta.intent,
            evidence = evidence.trim(),
            entry = rule.entry_id,
        );
        self.create_task(TaskSpec {
            domain,
            title: format!("Amend {}", rule.meta.id),
            body,
            requirements: vec!["human".into()],
            origin: Some(format!("{AMENDMENT_ORIGIN}{}", rule.meta.id)),
            upstream_event: Some(dm_core::digest(evidence.trim())),
            priority: 3,
            ..TaskSpec::default()
        })
        .await
    }

    /// The person in a rule's lineage: who wrote its entry, who promoted it, or who
    /// approved its promotion.
    async fn person_behind(&self, rule: &record::Entry) -> Result<Option<String>, Error> {
        let owner = &self.grants.owner;
        let entry = self.get(&rule.entry_id).await?;
        if is_person(&entry.front.author, owner) {
            return Ok(Some(entry.front.author));
        }
        // a promotion to curated names who made it; a promotion to verified names who
        // proposed it, and the approval task it closed names who approved
        let l = self.ledger.lock().await;
        let promoted = l.promotions(&rule.domain, &rule.entry_id).await?;
        let approvals = format!("{}{}", crate::PROPOSAL_ORIGIN, rule.entry_id);
        let approved = l.list(&rule.domain, dm_ledger::Status::Done).await?;
        let approvers = approved
            .into_iter()
            .filter(|t| t.spec.origin.as_deref() == Some(approvals.as_str()))
            .filter_map(|t| t.holder);
        Ok(promoted
            .into_iter()
            .map(|p| p.spec.by)
            .chain(approvers)
            .find(|by| is_person(by, owner)))
    }

    /// Reset a domain's rules: the owner retires, in one step, every rule that no
    /// person wrote or promoted, and keeps the rest. A retired rule is superseded by a
    /// version whose block says so, as every change to the record is a version.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for anyone but the owner of the record, or a domain outside
    /// the grants; and as [`Memory::write`].
    pub async fn reset_rules(&self, domain: Option<&str>) -> Result<Reset, Error> {
        if self.author != self.grants.owner {
            return Err(Error::Denied(format!(
                "only the owner {} resets the rules",
                self.grants.owner
            )));
        }
        let domain = self.resolve_domain(domain)?;
        let p = self.policy();
        let mut out = Reset {
            domain: domain.clone(),
            ..Reset::default()
        };
        for rule in p.entries().iter().filter(|e| {
            e.domain == domain && !e.meta.is_exception() && e.state() != Status::Retired
        }) {
            if let Some(person) = self.person_behind(rule).await? {
                out.kept.push(Kept {
                    policy_id: rule.meta.id.clone(),
                    person,
                });
                continue;
            }
            let old = self.get(&rule.entry_id).await?;
            let body = record::with_status(&old.body, Status::Retired).map_err(Error::Denied)?;
            self.write(WriteRequest {
                domain: Some(domain.clone()),
                entry_type: EntryType::Policy,
                title: old.front.title.clone(),
                body,
                supersedes: Some(rule.entry_id.clone()),
                ..WriteRequest::default()
            })
            .await?;
            out.retired.push(rule.meta.id.clone());
        }
        out.kept.sort_by(|a, b| a.policy_id.cmp(&b.policy_id));
        out.retired.sort();
        self.reload_policy().await?;
        Ok(out)
    }
}
