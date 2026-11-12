//! The rule lifecycle as a session meets it: the rules in force where it is about to
//! act, an exception asked for, an amendment proposed to a rule's owner, the reset by
//! which a domain's owner retires what no person stands behind, and the lint.
//!
//! A rule is a policy entry ([`dm_policy::record`]). Where it is in its life is its
//! block's `status` held back by its entry's tier, so every step up is a reviewed
//! promotion and every step is a version of an entry. Nothing here is a second store
//! of rules.

use crate::{Error, Memory, WriteRequest};
use dm_core::{EntryType, TrustTier};
use dm_ledger::{Task, TaskSpec};
use dm_policy::decide::Class;
use dm_policy::record::{self, Finding, Meta, Status};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
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
    /// The rule, one sentence. Empty when withheld.
    pub rule: String,
    /// Why: what an edge case is resolved from. Empty when withheld.
    pub intent: String,
    /// `enforced`, which refuses, or `advisory`, which reports.
    pub status: Status,
    /// Whether a module checks it. A rule without one binds by being read.
    pub checked: bool,
    /// Who answers for it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// The rule's entry is above what this session may read: the rule binds, and its
    /// text is not given.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub withheld: bool,
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
}

/// An exception as filed: written, in force for nothing until a person approves it.
#[derive(Clone, Debug, Serialize)]
pub struct FiledException {
    /// The entry that holds it.
    pub id: String,
    /// Its policy id, which a decision that uses it cites.
    pub policy_id: String,
    /// Who should approve: the higher the layer breached, the more senior.
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
    /// The rules retired.
    pub retired: Vec<Retired>,
}

/// A rule a reset kept.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Kept {
    /// The rule.
    pub policy_id: String,
    /// The entry that holds it.
    pub entry: String,
    /// The person who wrote it, curated it or approved it.
    pub person: String,
}

/// A rule a reset retired.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Retired {
    /// The rule.
    pub policy_id: String,
    /// The entry that held it, which the retired version supersedes.
    pub entry: String,
}

/// How often a rule was cited, from the decision log.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Rate {
    /// The policy id.
    pub id: String,
    /// Decisions it refused.
    pub refused: u64,
    /// Decisions an exception let through, for an exception's id.
    pub let_through: u64,
    /// Decisions it would have refused, as an advisory rule.
    pub advised: u64,
}

fn is_person(author: &str, owner: &str) -> bool {
    matches!(Class::of(author, owner), Class::Owner | Class::Human)
}

fn live(status: Status) -> bool {
    matches!(status, Status::Enforced | Status::Advisory)
}

impl Memory {
    /// The rules in force for an action in a domain: every enforced and advisory rule
    /// of layers 0 to 2, and the domain's own conventions. A session asks before an
    /// action it cannot take back. The rules are not chosen by the action: the action
    /// is what the read log keeps, so an audit finds that the session asked. A rule is
    /// read as memory is: one whose entry this session may not read is named, with its
    /// layer and status, and its text is withheld.
    ///
    /// # Errors
    ///
    /// A domain outside the grants, or an index error.
    pub async fn check_policy(
        &self,
        action: &str,
        domain: Option<&str>,
    ) -> Result<Vec<RuleInForce>, Error> {
        let domain = self.resolve_domain(domain)?;
        let readable: Vec<String> = self
            .list(None, Some(EntryType::Policy), 1000)
            .await?
            .into_iter()
            .map(|h| h.id)
            .collect();
        let p = self.policy();
        let mut rules: Vec<RuleInForce> = p
            .entries()
            .iter()
            .filter(|e| !e.meta.is_exception())
            .filter(|e| e.meta.layer < 3 || e.domain == domain)
            .filter(|e| live(p.state_of(e)))
            .map(|e| {
                let withheld = !readable.contains(&e.entry_id);
                let text = |s: &str| {
                    if withheld {
                        String::new()
                    } else {
                        s.to_string()
                    }
                };
                RuleInForce {
                    policy_id: e.meta.id.clone(),
                    layer: e.meta.layer,
                    rule: text(&e.meta.rule),
                    intent: text(&e.meta.intent),
                    status: p.state_of(e),
                    checked: e.rego.is_some(),
                    owner: e.meta.owner.clone().filter(|_| !withheld),
                    withheld,
                }
            })
            .collect();
        // an enforced entry of an id before an advisory one of the same id
        rules.sort_by(|a, b| {
            (a.layer, &a.policy_id, a.status != Status::Enforced).cmp(&(
                b.layer,
                &b.policy_id,
                b.status != Status::Enforced,
            ))
        });
        rules.dedup_by(|a, b| a.policy_id == b.policy_id);
        self.reads
            .text("check_policy", &format!("{action} in {domain}"));
        Ok(rules)
    }

    /// The rule of a policy id: the entry in force when one is, so an entry nobody
    /// reviewed that reuses the id does not speak for the rule. It is read as this
    /// session, so a rule the session may not read is not found.
    async fn rule(&self, policy_id: &str) -> Result<(record::Entry, dm_core::Entry), Error> {
        let p = self.policy();
        let rank = |e: &record::Entry| match (p.state_of(e), e.tier) {
            (Status::Enforced, _) => 0,
            (Status::Advisory, _) => 1,
            (_, TrustTier::Verified) => 2,
            _ => 3,
        };
        let found = p
            .entries()
            .iter()
            .filter(|e| e.meta.id == policy_id && !e.meta.is_exception())
            .min_by_key(|e| (rank(e), e.entry_id.clone()))
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("policy {policy_id}")))?;
        let entry = self
            .get(&found.entry_id)
            .await
            .map_err(|_| Error::NotFound(format!("policy {policy_id}")))?;
        Ok((found, entry))
    }

    /// Who should approve an exception to a rule: the rule's owner for a domain
    /// convention or a practice, and the owner of the record for strategy and values.
    fn approver_of(&self, rule: &record::Entry) -> String {
        match (&rule.meta.owner, rule.meta.layer) {
            (Some(owner), 2..) => owner.clone(),
            _ => self.grants.owner.clone(),
        }
    }

    /// What a request for an exception must be before anything is written: words with
    /// no code fence, an expiry that is a time to come, and a task to hold for. Gives
    /// the task and the time of the check.
    fn exception_asked(&self, ask: &ExceptionRequest) -> Result<(String, OffsetDateTime), Error> {
        if [&ask.scope, &ask.justification]
            .iter()
            .any(|words| words.contains("```") || words.contains("~~~"))
        {
            return Err(Error::Denied(
                "the scope and the justification of an exception are words, with no code fence"
                    .into(),
            ));
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
        Ok((task, now))
    }

    /// Ask for an exception to a rule: for one task, until a time, with a reason. The
    /// exception is written as a policy entry in the rule's domain and proposed for
    /// approval; it lets nothing through until a person who did not file it approves
    /// it, and after its expiry it lets nothing through again.
    ///
    /// The words of the request go into an entry a person is asked to verify, so they
    /// are words only: a request that carries a code fence is refused, and the entry
    /// is written only if it reads back as the exception that was asked for.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for a rule or a task that is not there for this session;
    /// [`Error::Denied`] for a rule of the L0 core, an expiry that is not a time to
    /// come, no task to scope it to, or a code fence in the words; and as
    /// [`Memory::write`] and [`Memory::promote`].
    pub async fn file_exception(&self, ask: ExceptionRequest) -> Result<FiledException, Error> {
        let (rule, rule_entry) = self.rule(&ask.breaches).await?;
        if rule.meta.is_core() {
            return Err(Error::Denied(format!(
                "{} is a rule of the L0 core, which takes no exception",
                rule.meta.id
            )));
        }
        let (task, now) = self.exception_asked(&ask)?;
        self.get_task(&task).await?;
        let approver = self.approver_of(&rule);
        let key = dm_core::digest(&format!("{}\n{task}\n{}", rule.meta.id, ask.expires));
        let policy_id = format!("exc-{}-{}", now.year(), key.get(..8).unwrap_or(&key));
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
        let reads_back = record::parse("", TrustTier::Raw, &body);
        if !reads_back.is_ok_and(|e| e.meta == meta && e.rego.is_none()) {
            return Err(Error::Denied(
                "the exception does not read back as it was asked for, and is not written".into(),
            ));
        }
        let written = self
            .write(WriteRequest {
                domain: Some(rule_entry.front.domain.clone()),
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
    /// rule's owner and carries the evidence. It is classified as the rule's entry is.
    /// A filer has one open amendment to a rule at a time: asked again, it gets that
    /// task back.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for a rule that is not there for this session;
    /// [`Error::Denied`] for no evidence; a ledger error, or a filing the policy
    /// refuses.
    pub async fn propose_amendment(
        &self,
        policy_id: &str,
        evidence: &str,
    ) -> Result<dm_ledger::Created, Error> {
        let (rule, entry) = self.rule(policy_id).await?;
        if evidence.trim().is_empty() {
            return Err(Error::Denied(
                "an amendment carries its evidence: what happened, and the entries that show it"
                    .into(),
            ));
        }
        let domain = entry.front.domain.clone();
        let origin = format!("{AMENDMENT_ORIGIN}{}", rule.meta.id);
        for status in [dm_ledger::Status::Proposed, dm_ledger::Status::Open] {
            let open = self.tasks(Some(&domain), status).await?;
            if let Some(task) = open.into_iter().find(|t| {
                t.spec.origin.as_deref() == Some(origin.as_str())
                    && t.spec.created_by == self.author
            }) {
                return Ok(dm_ledger::Created {
                    task,
                    deduplicated: true,
                });
            }
        }
        let owner = rule
            .meta
            .owner
            .clone()
            .unwrap_or_else(|| self.grants.owner.clone());
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
            origin: Some(origin),
            upstream_event: Some(dm_core::digest(&format!(
                "{}\n{}",
                self.author,
                evidence.trim()
            ))),
            classification: entry.front.classification,
            priority: 3,
            ..TaskSpec::default()
        })
        .await
    }

    /// The person in a rule's lineage: who wrote its entry, who curated it, or who
    /// approved its promotion to verified. A proposal that nobody approved puts nobody
    /// in the lineage.
    async fn person_behind(&self, rule: &record::Entry) -> Result<Option<String>, Error> {
        let owner = &self.grants.owner;
        let entry = self.get(&rule.entry_id).await?;
        if is_person(&entry.front.author, owner) {
            return Ok(Some(entry.front.author));
        }
        let l = self.ledger.lock().await;
        let promoted = l.promotions(&rule.domain, &rule.entry_id).await?;
        let curators = promoted
            .into_iter()
            .filter(|p| p.spec.tier == TrustTier::Curated && entry.front.trust_tier >= p.spec.tier)
            .map(|p| p.spec.by);
        let approvals = format!("{}{}", crate::PROPOSAL_ORIGIN, rule.entry_id);
        let approved = l.list(&rule.domain, dm_ledger::Status::Done).await?;
        let approvers = approved
            .into_iter()
            .filter(|t| t.spec.origin.as_deref() == Some(approvals.as_str()))
            .filter_map(|t| t.holder);
        Ok(curators.chain(approvers).find(|by| is_person(by, owner)))
    }

    /// Reset a domain's rules: the owner retires, in one step, every rule that no
    /// person wrote, curated or approved, and keeps the rest. A retired rule is
    /// superseded by a version whose block says so, as every change to the record is a
    /// version. Exceptions and the rules of other domains are not touched.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for anyone but the owner of the record, or a domain outside
    /// the grants; and as [`Memory::write`]. After an error, what was retired before
    /// it stays retired, and a second reset does the rest.
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
                    entry: rule.entry_id.clone(),
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
            out.retired.push(Retired {
                policy_id: rule.meta.id.clone(),
                entry: rule.entry_id.clone(),
            });
        }
        out.kept
            .sort_by(|a, b| (&a.policy_id, &a.entry).cmp(&(&b.policy_id, &b.entry)));
        out.retired
            .sort_by(|a, b| (&a.policy_id, &a.entry).cmp(&(&b.policy_id, &b.entry)));
        self.reload_policy().await?;
        Ok(out)
    }

    /// Citations per policy, from the decision log: how often each rule refused, how
    /// often an exception let something through, and how often an advisory rule would
    /// have refused.
    #[must_use]
    pub fn policy_rates(&self) -> Vec<Rate> {
        let text =
            std::fs::read_to_string(self.root.join("state/decisions.jsonl")).unwrap_or_default();
        let mut rates: BTreeMap<String, Rate> = BTreeMap::new();
        let names = |list: &serde_json::Value, key: Option<&str>| -> Vec<String> {
            list.as_array()
                .into_iter()
                .flatten()
                .filter_map(|x| key.map_or(x.as_str(), |k| x[k].as_str()))
                .map(str::to_string)
                .collect()
        };
        for line in text.lines() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let allow = v["allow"].as_bool().unwrap_or(false);
            for id in names(&v["cites"], None) {
                let rate = rates.entry(id).or_default();
                if allow {
                    rate.let_through += 1;
                } else {
                    rate.refused += 1;
                }
            }
            for id in names(&v["advisories"], Some("policy")) {
                rates.entry(id).or_default().advised += 1;
            }
        }
        for (id, rate) in &mut rates {
            rate.id.clone_from(id);
        }
        rates.into_values().collect()
    }

    /// The lint over the record's rules: the paper's conflict rules, a module that
    /// would not load, a rule in force with no owner or past its review date, a rule
    /// the ceiling holds at advisory, and an enforced rule that no decision in the log
    /// cites, which goes to its owner to retire or to say why it stays.
    #[must_use]
    pub fn policy_findings(&self) -> Vec<Finding> {
        let p = self.policy();
        let mut findings = record::lint(p.entries(), OffsetDateTime::now_utc());
        findings.extend(p.refused().iter().map(|(id, why)| Finding {
            policy: id.clone(),
            kind: "module-refused".into(),
            why: format!("{id}'s module would not load: {why}"),
        }));
        findings.extend(p.over_ceiling().into_iter().map(|id| Finding {
            why: format!(
                "{id} is over the ceiling on enforced rules and is advisory: a person signs \
                 this one rule off with signed_off_by, or a rule is retired"
            ),
            policy: id,
            kind: "over-ceiling".into(),
        }));
        let used: HashMap<String, Rate> = self
            .policy_rates()
            .into_iter()
            .map(|r| (r.id.clone(), r))
            .collect();
        let mut unused: Vec<&str> = p
            .entries()
            .iter()
            .filter(|e| e.enforced() && p.state_of(e) == Status::Enforced)
            .map(|e| e.meta.id.as_str())
            .filter(|id| used.get(*id).is_none_or(|r| r.refused == 0))
            .collect();
        unused.sort_unstable();
        unused.dedup();
        findings.extend(unused.into_iter().map(|id| Finding {
            policy: id.to_string(),
            kind: "unused".into(),
            why: format!(
                "{id} is enforced and refused nothing in the decision log: its owner retires \
                 it, or says why it stays"
            ),
        }));
        findings
    }

    /// File a task for the owner for each finding of the lint, once per finding. A
    /// rule that is not yet verified is no finding to act on, and gets none. Returns
    /// the tasks filed now.
    ///
    /// # Errors
    ///
    /// A ledger error, or several granted domains and none to file in.
    pub async fn file_policy_findings(&self, findings: &[Finding]) -> Result<Vec<String>, Error> {
        let domain = self.resolve_domain(None)?;
        let mut filed = Vec::new();
        for f in findings.iter().filter(|f| f.kind != "inert") {
            let created = self
                .file_own(TaskSpec {
                    domain: domain.clone(),
                    title: format!("Policy bug, {}: {}", f.kind, f.policy),
                    body: format!(
                        "{}\n\nAcross layers the higher wins and the lower is fixed: amend or supersede {} so it agrees, or file an exception with a scope, an expiry and an approver. Within a layer, resolve from the stated intents and record the trade-off.",
                        f.why, f.policy
                    ),
                    requirements: vec!["human".into()],
                    origin: Some(format!("policy:{}", f.kind)),
                    upstream_event: Some(f.policy.clone()),
                    classification: dm_core::Label::Internal,
                    priority: 4,
                    ..TaskSpec::default()
                })
                .await?;
            if !created.deduplicated {
                filed.push(created.task.id);
            }
        }
        Ok(filed)
    }
}
