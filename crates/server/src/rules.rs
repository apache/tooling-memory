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
use std::collections::{BTreeMap, BTreeSet, HashMap};
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
        // the policy entries this session may read; its label rises only to the rules
        // whose text it is given
        let readable = {
            let f = self.filter_for(None)?;
            let ix = self.index.lock().await;
            ix.list_every(&f, Some(EntryType::Policy))?
        };
        let p = self.policy();
        let mut rules: Vec<(&str, RuleInForce)> = p
            .entries()
            .iter()
            .filter(|e| !e.meta.is_exception())
            .filter(|e| e.meta.layer < 3 || e.domain == domain)
            .filter(|e| live(p.state_of(e)))
            .map(|e| {
                let withheld = !readable.iter().any(|h| h.id == e.entry_id);
                let text = |s: &str| {
                    if withheld {
                        String::new()
                    } else {
                        s.to_string()
                    }
                };
                let rule = RuleInForce {
                    policy_id: e.meta.id.clone(),
                    layer: e.meta.layer,
                    rule: text(&e.meta.rule),
                    intent: text(&e.meta.intent),
                    status: p.state_of(e),
                    checked: e.rego.is_some(),
                    owner: e.meta.owner.clone().filter(|_| !withheld),
                    withheld,
                };
                (e.entry_id.as_str(), rule)
            })
            .collect();
        // one row for an id: an enforced entry before an advisory one
        rules.sort_by(|(_, a), (_, b)| {
            (&a.policy_id, a.status != Status::Enforced, a.layer).cmp(&(
                &b.policy_id,
                b.status != Status::Enforced,
                b.layer,
            ))
        });
        rules.dedup_by(|(_, a), (_, b)| a.policy_id == b.policy_id);
        rules.sort_by(|(_, a), (_, b)| (a.layer, &a.policy_id).cmp(&(b.layer, &b.policy_id)));
        let read: Vec<dm_index::Hit> = readable
            .iter()
            .filter(|h| rules.iter().any(|(entry, r)| *entry == h.id && !r.withheld))
            .cloned()
            .collect();
        self.observe(&read)?;
        self.reads.hits(
            "check_policy",
            Some(&format!("{action} in {domain}")),
            &read,
        );
        Ok(rules.into_iter().map(|(_, r)| r).collect())
    }

    /// The rule of a policy id: the entry in force, or else a verified one. An entry
    /// that nobody verified speaks for no rule, whatever id it carries, so a rule with
    /// no verified entry is not found. It is read as this session, so a rule the
    /// session may not read is not found either.
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
            .filter(|e| e.meta.id == policy_id && !e.meta.is_exception() && rank(e) < 3)
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
        if [&ask.scope, &ask.justification].iter().any(|words| {
            ["```", "~~~", "\n", "\r", "<"]
                .iter()
                .any(|mark| words.contains(mark))
        }) {
            return Err(Error::Denied(
                "the scope and the justification of an exception are words on one line, with \
                 no code fence and no markup"
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
        // asked before for this rule, task and expiry: one entry and one approval task
        if let Some(before) = self
            .policy()
            .entries()
            .iter()
            .find(|e| e.meta.id == policy_id && e.meta.status != Some(Status::Retired))
        {
            return Err(Error::Denied(format!(
                "this exception was asked for before, as {}: it is not asked for twice",
                before.entry_id
            )));
        }
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
        // looked for on the ledger, by origin and filer: the session reads no other task
        for status in [
            dm_ledger::Status::Proposed,
            dm_ledger::Status::Open,
            dm_ledger::Status::Claimed,
            dm_ledger::Status::Blocked,
        ] {
            let waiting = self.ledger.lock().await.list(&domain, status).await?;
            if let Some(task) = waiting.into_iter().find(|t| {
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
        // a done approval task whose entry is verified was approved: one that a person
        // closed without approving left the entry where it was
        let verified = entry.front.trust_tier == TrustTier::Verified;
        let approvers = approved
            .into_iter()
            .filter(|t| verified && t.spec.origin.as_deref() == Some(approvals.as_str()))
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

/// The origin of a task that asks a curator to propose one rule for two that overlap.
pub const MERGE_RULES_ORIGIN: &str = "curator:merge-rules";

/// The origin of a task that asks a curator to propose a task definition.
pub const TASK_DEFINITION_ORIGIN: &str = "curator:task-definition";

/// How many done tasks with one normalised title and no definition are repeated work.
pub const REPEATED_MIN: usize = 3;

/// Two rules in force in one layer whose rule sentences say nearly the same.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Overlap {
    /// The policy id of one rule. It sorts before `b`.
    pub a: String,
    /// The policy id of the other rule.
    pub b: String,
    /// The layer of the two rules.
    pub layer: u8,
    /// Why the two overlap: how many words the sentences share.
    pub why: String,
}

/// Work that was done three times or more with no task definition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Repeated {
    /// The title of the tasks, in lowercase with the digits removed.
    pub title: String,
    /// The done tasks with that title, sorted.
    pub tasks: Vec<String>,
}

/// What the curator finds in the policy and in the done work.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct PolicyCuration {
    /// The pairs of rules that overlap.
    pub overlaps: Vec<Overlap>,
    /// The repeated work with no task definition.
    pub repeated: Vec<Repeated>,
}

/// The words of a rule sentence: lowercase, letters and digits only.
fn words(sentence: &str) -> BTreeSet<String> {
    sentence
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// The words that two rule sentences share and the words of the two together, when the
/// share is four fifths or more: the Jaccard overlap is 0.8 or more.
fn shared_words(a: &str, b: &str) -> Option<(usize, usize)> {
    let (a, b) = (words(a), words(b));
    let shared = a.intersection(&b).count();
    let all = a.union(&b).count();
    (all > 0 && shared * 5 >= all * 4).then_some((shared, all))
}

/// The pairs of `rules` that overlap. Each rule is in force. Two rules in different
/// layers do not overlap.
fn overlaps_among(rules: &[&Meta]) -> Vec<Overlap> {
    let mut out = Vec::new();
    for (i, a) in rules.iter().enumerate() {
        for b in &rules[i + 1..] {
            if a.layer != b.layer || a.id == b.id {
                continue;
            }
            let Some((shared, all)) = shared_words(&a.rule, &b.rule) else {
                continue;
            };
            let (first, second) = if a.id < b.id { (a, b) } else { (b, a) };
            out.push(Overlap {
                a: first.id.clone(),
                b: second.id.clone(),
                layer: a.layer,
                why: format!("the rule sentences share {shared} of {all} words"),
            });
        }
    }
    out.sort_by(|x, y| (&x.a, &x.b).cmp(&(&y.a, &y.b)));
    out
}

/// A title in lowercase with its digits removed and its spaces made single.
fn normalised(title: &str) -> String {
    let kept: String = title
        .to_lowercase()
        .chars()
        .filter(|c| !c.is_ascii_digit())
        .collect();
    kept.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The repeated work among `done`: [`REPEATED_MIN`] tasks or more with one normalised
/// title. A task that names a definition is not counted. Only the work of a session
/// is counted: a task whose origin is absent or starts with `task:`. A task with each
/// other origin, which the system or a tap filed, is not counted.
fn repeated_among(done: &[Task]) -> Vec<Repeated> {
    let mut by_title: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for t in done {
        let own_work = t
            .spec
            .origin
            .as_deref()
            .is_none_or(|o| o.starts_with("task:"));
        let title = normalised(&t.spec.title);
        if t.spec.definition.is_some() || !own_work || title.is_empty() {
            continue;
        }
        by_title.entry(title).or_default().push(t.id.clone());
    }
    by_title
        .into_iter()
        .filter(|(_, tasks)| tasks.len() >= REPEATED_MIN)
        .map(|(title, mut tasks)| {
            tasks.sort();
            Repeated { title, tasks }
        })
        .collect()
}

fn merge_rules_task(domain: &str, o: &Overlap) -> TaskSpec {
    TaskSpec {
        domain: domain.to_string(),
        title: format!("Curator: propose one rule for {} and {}", o.a, o.b),
        body: format!(
            "{a} and {b} are in force in layer {layer}, and {why}.\n\nWrite one policy entry \
             with status `proposed` that states the two rules as one rule and cites {a} and \
             {b}. Then call `propose_amendment` on {a} and on {b}, with the new entry as the \
             evidence, so that the owner of each rule decides. Change no rule that is in \
             force.\n",
            a = o.a,
            b = o.b,
            layer = o.layer,
            why = o.why
        ),
        origin: Some(MERGE_RULES_ORIGIN.to_string()),
        upstream_event: Some(format!("{}+{}", o.a, o.b)),
        classification: dm_core::Label::Internal,
        priority: 0,
        ..TaskSpec::default()
    }
}

fn task_definition_task(domain: &str, r: &Repeated) -> TaskSpec {
    TaskSpec {
        domain: domain.to_string(),
        title: format!("Curator: propose a task definition for \"{}\"", r.title),
        body: format!(
            "{n} done tasks have the title \"{title}\" and name no task definition: \
             {tasks}.\n\nPropose a task definition in the platform domain for this kind of \
             work. The definition is in force only after a person verifies it.\n",
            n = r.tasks.len(),
            title = r.title,
            tasks = r.tasks.join(", ")
        ),
        origin: Some(TASK_DEFINITION_ORIGIN.to_string()),
        upstream_event: Some(r.title.clone()),
        classification: dm_core::Label::Internal,
        priority: 0,
        ..TaskSpec::default()
    }
}

impl Memory {
    /// What the curator finds for one domain: the rules that overlap, and the repeated
    /// work with no task definition.
    ///
    /// Two rules overlap when both are in force (advisory or enforced), both are in one
    /// layer, and the words of their rule sentences have a Jaccard overlap of 0.8 or
    /// more. An exception, a proposed rule and a retired rule overlap nothing. A rule
    /// above the handling of this session is not compared. One of the two rules is a
    /// rule of the domain.
    ///
    /// Work is repeated when [`REPEATED_MIN`] done tasks of the domain or more have one
    /// title, after the title is put in lowercase and its digits are removed, and none
    /// names a definition. The done tasks that this session sees are read.
    ///
    /// # Errors
    ///
    /// An ungranted domain, or a ledger error.
    pub async fn policy_curation(&self, domain: Option<&str>) -> Result<PolicyCuration, Error> {
        let domain = self.resolve_domain(domain)?;
        let handling = self.handling()?;
        let p = self.policy();
        let mut in_force: BTreeMap<&str, &record::Entry> = BTreeMap::new();
        for e in p.entries() {
            let state = p.state_of(e);
            if !e.meta.is_exception()
                && matches!(state, Status::Advisory | Status::Enforced)
                && e.classification <= handling
            {
                in_force.entry(e.meta.id.as_str()).or_insert(e);
            }
        }
        let rules: Vec<&Meta> = in_force.values().map(|e| &e.meta).collect();
        let of_domain = |id: &str| in_force.get(id).is_some_and(|e| e.domain == domain);
        let mut overlaps = overlaps_among(&rules);
        overlaps.retain(|o| of_domain(&o.a) || of_domain(&o.b));
        let done = self.tasks(Some(&domain), dm_ledger::Status::Done).await?;
        Ok(PolicyCuration {
            overlaps,
            repeated: repeated_among(&done),
        })
    }

    /// File one task for a curator for each finding of [`Memory::policy_curation`], in
    /// one domain. A finding is filed once: a second run files nothing. Returns the ids
    /// of the tasks filed now, and the number of findings that had a task already.
    ///
    /// An overlap gets a task with the origin [`MERGE_RULES_ORIGIN`]: the curator
    /// proposes one rule and the owners decide. Repeated work gets a task with the
    /// origin [`TASK_DEFINITION_ORIGIN`]: the curator proposes a task definition. The
    /// curator changes no rule in force.
    ///
    /// # Errors
    ///
    /// An ungranted domain, or as [`Memory::file_own`].
    pub async fn file_policy_curation(
        &self,
        domain: Option<&str>,
        found: &PolicyCuration,
    ) -> Result<(Vec<String>, usize), Error> {
        let domain = self.resolve_domain(domain)?;
        let merges = found.overlaps.iter().map(|o| merge_rules_task(&domain, o));
        let definitions = found
            .repeated
            .iter()
            .map(|r| task_definition_task(&domain, r));
        let mut filed = Vec::new();
        let mut already = 0;
        for spec in merges.chain(definitions).collect::<Vec<_>>() {
            let created = self.file_own(spec).await?;
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
    use crate::Promotion;
    use dm_core::Label;
    use std::path::Path;

    const OWNER: &str = "human:test:me";
    const SENTENCE: &str = "Decisions are superseded by a person, not an agent.";

    fn meta(id: &str, layer: u8, rule: &str) -> Meta {
        Meta {
            id: id.into(),
            layer,
            rule: rule.into(),
            ..Meta::default()
        }
    }

    #[test]
    fn two_sentences_overlap_at_four_fifths_of_their_words() {
        assert_eq!(
            shared_words("One two, three FOUR.", "one two three four five"),
            Some((4, 5))
        );
        assert_eq!(shared_words("one two three", "one two three four"), None);
        assert_eq!(shared_words("a b c d", "D c-b a!"), Some((4, 4)));
        assert_eq!(shared_words("", ""), None, "no words, no overlap");
    }

    #[test]
    fn rules_overlap_only_in_one_layer() {
        let near = "Decisions are superseded by a person, not an agent, always.";
        let rules = [
            meta("pol-L2-002", 2, near),
            meta("pol-L2-001", 2, SENTENCE),
            meta("pol-L3-003", 3, SENTENCE),
            meta("pol-L2-004", 2, "A handoff names the next step."),
        ];
        let got = overlaps_among(&rules.iter().collect::<Vec<_>>());
        let want = Overlap {
            a: "pol-L2-001".into(),
            b: "pol-L2-002".into(),
            layer: 2,
            why: "the rule sentences share 9 of 10 words".into(),
        };
        assert_eq!(got, [want]);
    }

    fn done(id: &str, title: &str, origin: Option<&str>, definition: Option<&str>) -> Task {
        Task {
            id: id.into(),
            spec: TaskSpec {
                domain: "dom-me".into(),
                title: title.into(),
                origin: origin.map(str::to_string),
                definition: definition.map(str::to_string),
                ..TaskSpec::default()
            },
            created: OffsetDateTime::UNIX_EPOCH,
            status: dm_ledger::Status::Done,
            holder: None,
            lease_until: None,
            generation: 1,
            handoff: None,
            reason: None,
            superseded_by: None,
            posted_to: None,
        }
    }

    #[test]
    fn work_is_repeated_at_three_done_tasks_with_one_title_and_no_definition() {
        let def = Some("release@1");
        let tasks = [
            done("t3", "Release 1.4.12", None, None),
            done("t1", "release  1.5.0", None, None),
            done("t2", "RELEASE 2.0.0", Some("task:t0"), None),
            // two are not enough
            done("t4", "Rotate key 1", None, None),
            done("t5", "Rotate key 2", None, None),
            // a task that names a definition is not counted
            done("t6", "Ship build 1", None, None),
            done("t7", "Ship build 2", None, None),
            done("t8", "Ship build 3", None, def),
            // a task that the system filed is not counted
            done("t9", "Verify 1", None, None),
            done("t10", "Verify 2", None, None),
            done("t11", "Verify 3", Some("curator:verify"), None),
        ];
        let want = Repeated {
            title: "release ..".into(),
            tasks: vec!["t1".into(), "t2".into(), "t3".into()],
        };
        assert_eq!(repeated_among(&tasks), [want]);
    }

    /// A rule with no module, written by the owner and verified.
    async fn rule(root: &Path, id: &str, layer: u8, sentence: &str, extra: &str) -> String {
        let owner = Memory::open(root).unwrap().with_author(OWNER);
        let body = format!(
            "# A rule\n\n```policy\nid = \"{id}\"\nlayer = {layer}\nrule = \"{sentence}\"\n\
             intent = \"An agent rewrote a standing decision.\"\nowner = \"human:test:lead\"\n\
             {extra}```\n"
        );
        let entry = owner
            .write(WriteRequest {
                entry_type: EntryType::Policy,
                body,
                ..WriteRequest::default()
            })
            .await
            .unwrap()
            .id;
        let agent = Memory::open(root).unwrap().with_author("agent:a");
        let promoted = agent.promote(&entry, TrustTier::Verified, "reviewed");
        let Promotion::Proposed { task } = promoted.await.unwrap() else {
            panic!("proposed")
        };
        owner.approve(&task.id).await.unwrap();
        entry
    }

    async fn tasks_of(m: &Memory, origin: &str) -> Vec<Task> {
        let l = m.ledger.lock().await;
        l.with_origin("dom-me", origin).await.unwrap()
    }

    #[tokio::test]
    async fn only_rules_in_force_overlap_and_an_overlap_is_filed_once_for_a_curator() {
        let dir = tempfile::tempdir().unwrap();
        Memory::init(dir.path(), OWNER, "dom-me").unwrap();
        let root = dir.path();
        let near = "Decisions are superseded by a person, not an agent, always.";
        rule(root, "pol-L2-101", 2, SENTENCE, "status = \"enforced\"\n").await;
        rule(root, "pol-L2-102", 2, near, "status = \"advisory\"\n").await;
        rule(root, "pol-L3-103", 3, SENTENCE, "status = \"enforced\"\n").await;
        rule(root, "pol-L2-104", 2, SENTENCE, "status = \"retired\"\n").await;
        rule(root, "pol-L2-105", 2, SENTENCE, "status = \"proposed\"\n").await;
        let exception = "breaches = \"pol-L2-101\"\nscope = \"one task\"\n\
             approver = \"human:test:me\"\njustification = \"a test\"\n\
             expires = \"2999-01-01T00:00:00Z\"\n";
        rule(root, "exc-2026-001", 2, SENTENCE, exception).await;
        let m = Memory::open(root).unwrap().with_author("agent:curator");
        let found = m.policy_curation(None).await.unwrap();
        let pairs: Vec<(&str, &str, u8)> = found
            .overlaps
            .iter()
            .map(|o| (o.a.as_str(), o.b.as_str(), o.layer))
            .collect();
        assert_eq!(pairs, [("pol-L2-101", "pol-L2-102", 2)]);
        assert!(found.repeated.is_empty());
        let before = m.check_policy("supersede a decision", None).await.unwrap();
        let (filed, already) = m.file_policy_curation(None, &found).await.unwrap();
        assert_eq!((filed.len(), already), (1, 0));
        let spec = tasks_of(&m, MERGE_RULES_ORIGIN).await.remove(0).spec;
        assert_eq!((spec.priority, spec.requirements.len()), (0, 0));
        assert_eq!(spec.classification, Label::Internal);
        assert_eq!(
            spec.title,
            "Curator: propose one rule for pol-L2-101 and pol-L2-102"
        );
        for part in [
            "Write one policy entry with status `proposed`",
            "cites pol-L2-101 and pol-L2-102",
            "call `propose_amendment` on pol-L2-101 and on pol-L2-102",
            "the owner of each rule decides",
            "Change no rule that is in force.",
        ] {
            assert!(spec.body.contains(part), "{part}: {}", spec.body);
        }
        // a second run files nothing, and no rule moved
        let again = m.policy_curation(None).await.unwrap();
        assert_eq!(again, found);
        assert_eq!(
            m.file_policy_curation(None, &again).await.unwrap(),
            (vec![], 1)
        );
        let after = Memory::open(root).unwrap().with_author("agent:curator");
        let after = after.check_policy("supersede a decision", None).await;
        assert_eq!(after.unwrap(), before);
    }

    /// File a task as `m`, do it and close it with a handoff.
    async fn done_task(m: &Memory, title: &str) -> String {
        let spec = TaskSpec {
            domain: "dom-me".into(),
            title: title.into(),
            body: "Do it.\n".into(),
            ..TaskSpec::default()
        };
        let id = m.create_task(spec).await.unwrap().task.id;
        let _ = m.admit(&id).await;
        m.claim(&id, time::Duration::minutes(5)).await.unwrap();
        let handoff = WriteRequest {
            entry_type: EntryType::Handoff,
            body: "# Done\n\nIt is done.\n".into(),
            ..WriteRequest::default()
        };
        let handoff = m.write(handoff).await.unwrap().id;
        m.complete(&id, None, &handoff).await.unwrap();
        id
    }

    #[tokio::test]
    async fn repeated_work_is_filed_once_for_a_curator_to_propose_a_definition() {
        let dir = tempfile::tempdir().unwrap();
        let m = Memory::init(dir.path(), OWNER, "dom-me").unwrap();
        let mut ids = Vec::new();
        for n in 1..=3 {
            ids.push(done_task(&m, &format!("Rotate the signing key {n}")).await);
        }
        done_task(&m, "Ship the report 1").await;
        done_task(&m, "Ship the report 2").await;
        // an open task with the same title is not done work
        let open = TaskSpec {
            domain: "dom-me".into(),
            title: "Ship the report 3".into(),
            body: "Do it.\n".into(),
            ..TaskSpec::default()
        };
        m.create_task(open).await.unwrap();
        ids.sort();
        let found = m.policy_curation(None).await.unwrap();
        let want = Repeated {
            title: "rotate the signing key".into(),
            tasks: ids.clone(),
        };
        assert_eq!(found.repeated, [want]);
        let (filed, already) = m.file_policy_curation(None, &found).await.unwrap();
        assert_eq!((filed.len(), already), (1, 0));
        let spec = tasks_of(&m, TASK_DEFINITION_ORIGIN).await.remove(0).spec;
        assert_eq!((spec.priority, spec.requirements.len()), (0, 0));
        assert_eq!(
            spec.title,
            "Curator: propose a task definition for \"rotate the signing key\""
        );
        for part in [
            "3 done tasks have the title \"rotate the signing key\"",
            ids.join(", ").as_str(),
            "Propose a task definition in the platform domain for this kind of work.",
            "The definition is in force only after a person verifies it.",
        ] {
            assert!(spec.body.contains(part), "{part}: {}", spec.body);
        }
        let again = m.policy_curation(None).await.unwrap();
        assert_eq!(
            m.file_policy_curation(None, &again).await.unwrap(),
            (vec![], 1)
        );
    }
}
