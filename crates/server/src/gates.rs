//! The gates on a candidate for the platform: what stands between an entry of the
//! platform's domain and the tier at which it is loaded.
//!
//! A candidate is an entry of the platform's domain that is proposed for verified. Two
//! gates run before the policy is asked, when the entry is proposed and again when a
//! person approves it:
//!
//! - **The safety gate.** A candidate version of a task definition is compared with the
//!   version it replaces ([`Memory::widens`]). When it declares a tool, a scope domain
//!   or an autonomy level that the version before it does not, and its author is not a
//!   person, the promotion is refused: no change to the platform adds a permission, and
//!   a person writes the definition that does. The refusal is a line of the decision
//!   log, so repeated ones count as probing ([`crate::fabric::probing`]).
//! - **The performance gate.** Where a held-out suite in force gives a pass mark for the
//!   level `person`, the newest run of the candidate version for that level must be of
//!   that suite version and must pass.
//!
//! Both are code of this crate and no policy module: a root's own policy files replace
//! the built-in policy, and no policy change waives these two.
//!
//! [`Memory::candidate_gates`] reports which gates an entry needs, by what its artifacts
//! reach, and which of them are met.

use crate::evaluation::SUITE_KIND;
use crate::platform::{artifacts_in, definition_block, unreadable, Autonomy, TaskDefinition};
use crate::{lock, Error, Memory};
use dm_core::{Entry, Label, TrustTier};
use dm_ledger::DefinitionChange;
use dm_policy::decide::{Class, Decision, Review};
use serde::Serialize;
use std::collections::BTreeSet;

/// One gate on a candidate, as [`Memory::candidate_gates`] reports it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GateState {
    /// Whether what the entry holds needs this gate.
    pub needed: bool,
    /// Whether the gate holds the entry back no longer. A gate that is not needed is
    /// met.
    pub met: bool,
    /// Why, in a sentence for each artifact that needs the gate.
    pub why: String,
}

impl GateState {
    /// A gate that nothing needs yet.
    fn open() -> GateState {
        GateState {
            needed: false,
            met: true,
            why: String::new(),
        }
    }

    /// One artifact needs this gate: the gate is met when it is met for each.
    fn need(&mut self, met: bool, why: &str) {
        self.needed = true;
        self.met &= met;
        if !self.why.is_empty() {
            self.why.push_str("; ");
        }
        self.why.push_str(why);
    }
}

/// The gates of one entry: what its artifacts reach, and what each gate says.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CandidateGates {
    /// The entry.
    pub entry: String,
    /// One line for each artifact: what a change to it reaches, and the gates that it
    /// needs.
    pub reach: Vec<String>,
    /// The performance gate: a passed run of the held-out suite.
    pub performance: GateState,
    /// The safety gate: no added permission.
    pub safety: GateState,
    /// The review of a person: met once the entry is verified.
    pub review: GateState,
}

/// What the performance gate finds for one version of a task definition.
enum Run {
    /// No held-out suite in force gives a pass mark for the level `person`.
    NoSuite,
    /// The newest run for the level `person` passed on the suite in force.
    Passed,
    /// No such run: why, in words that hold no text of the suite.
    Missing(String),
}

/// The task definitions in the blocks of `entry` that read as one, one for each name.
fn definitions_in(entry: &Entry) -> Vec<TaskDefinition> {
    let mut names = BTreeSet::new();
    artifacts_in(entry)
        .into_iter()
        .filter(|a| a.kind == "task" && names.insert(a.name.clone()))
        .filter_map(|a| definition_block(entry, &a.name)?.ok())
        .collect()
}

/// What `def` declares that `base` does not: each tool, each scope domain as
/// `scope:<domain>`, and the autonomy as `autonomy:<level>` when it is higher.
fn added_to(base: &TaskDefinition, def: &TaskDefinition) -> BTreeSet<String> {
    let tools = def.tools.iter().filter(|t| !base.tools.contains(t));
    let scope = def.scope.iter().filter(|d| !base.scope.contains(d));
    let autonomy = (def.autonomy > base.autonomy).then(|| format!("autonomy:{}", def.autonomy));
    tools
        .cloned()
        .chain(scope.map(|d| format!("scope:{d}")))
        .chain(autonomy)
        .collect()
}

/// Whether a person wrote `entry`: the owner, or another person.
fn person_wrote(entry: &Entry, owner: &str) -> bool {
    matches!(
        Class::of(&entry.front.author, owner),
        Class::Owner | Class::Human
    )
}

/// The entries of `entries` that `candidate` replaces: the entry that it supersedes, the
/// entry that this one supersedes, and so on to the first version.
fn replaced_by<'a>(candidate: &'a Entry, entries: &'a [Entry]) -> BTreeSet<&'a str> {
    let mut chain = BTreeSet::new();
    let mut next = candidate.front.supersedes.as_deref();
    // no chain is longer than its domain, so entries that replace each other in a
    // circle end the walk here
    for _ in 0..entries.len() {
        let Some(e) = next.and_then(|id| entries.iter().find(|e| e.front.id == id)) else {
            break;
        };
        chain.insert(e.front.id.as_str());
        next = e.front.supersedes.as_deref();
    }
    chain
}

fn listed(items: &BTreeSet<String>) -> String {
    items.iter().cloned().collect::<Vec<_>>().join(", ")
}

impl Memory {
    /// Whether `entry` is an entry of the platform's domain.
    fn in_platform(&self, entry: &Entry) -> bool {
        self.platform_domain().as_deref() == Some(entry.front.domain.as_str())
    }

    /// The version of the task definition `name` that `candidate` is compared with: the
    /// newest verified entry of the platform's domain, other than the candidate, that
    /// defines the name and is active or is an entry that the candidate replaces. A
    /// write that supersedes an entry takes that entry out of force at once, so the
    /// version in force before the candidate is among those it replaces. None when no
    /// such entry is there.
    ///
    /// # Errors
    ///
    /// A store error, or [`Error::Denied`] when the block of that entry does not read as
    /// a definition: a comparison that could not read its baseline has not passed.
    async fn baseline(
        &self,
        candidate: &Entry,
        name: &str,
    ) -> Result<Option<TaskDefinition>, Error> {
        let entries = self.platform_entries().await?;
        let replaced = replaced_by(candidate, &entries);
        let newest = entries
            .iter()
            .filter(|e| {
                e.front.id != candidate.front.id
                    && e.front.trust_tier == TrustTier::Verified
                    && (e.is_active() || replaced.contains(e.front.id.as_str()))
                    && definition_block(e, name).is_some()
            })
            .max_by_key(|e| e.front.created);
        newest
            .and_then(|e| definition_block(e, name))
            .transpose()
            .map_err(|()| unreadable(name))
    }

    /// What `entry` adds to the task definitions in force: empty unless it is an entry
    /// of the platform's domain. Each `platform task <name>` block of it that reads as a
    /// definition is compared with its baseline: the version in force in another entry,
    /// or an entry that this one replaces when that entry defined the name at verified,
    /// and the newest of them. The result lists `tool:<name>` for each tool
    /// that the baseline does not declare, `scope:<domain>` for each scope domain that
    /// the baseline does not have, and `autonomy:<level>` when the declared autonomy is
    /// above the baseline's. It is sorted and has no duplicates. A definition with no
    /// baseline adds nothing here: a person reviews the tools of a new definition
    /// against its purpose.
    ///
    /// The comparison reads the baseline as the record holds it, whoever asks.
    ///
    /// # Errors
    ///
    /// A store error, or [`Error::Denied`] when a baseline does not read as a
    /// definition.
    pub async fn widens(&self, entry: &Entry) -> Result<Vec<String>, Error> {
        let (added, _) = self.added_by(entry).await?;
        Ok(added.into_iter().collect())
    }

    /// [`Memory::widens`], with the highest classification of the baselines that the
    /// result tells something of.
    async fn added_by(&self, entry: &Entry) -> Result<(BTreeSet<String>, Label), Error> {
        let mut added = BTreeSet::new();
        let mut label = Label::default();
        if !self.in_platform(entry) {
            return Ok((added, label));
        }
        for def in definitions_in(entry) {
            if let Some(base) = self.baseline(entry, &def.name).await? {
                let more = added_to(&base, &def);
                if !more.is_empty() {
                    added.extend(more);
                    label = label.max(base.classification);
                }
            }
        }
        Ok((added, label))
    }

    /// The added items as this session is told them. The items say what a baseline
    /// does not declare, so a session whose handling is below that baseline is told
    /// that there is one, and not which. A session that is told has read the baseline,
    /// and its label is no lower.
    fn told(&self, added: &BTreeSet<String>, baseline: Label) -> Result<String, Error> {
        if baseline > self.handling()? {
            return Ok("a permission".into());
        }
        lock(&self.session).observe(baseline);
        Ok(listed(added))
    }

    /// The safety gate: refuse the promotion of `entry` to verified when it adds a
    /// permission to a task definition and no person wrote it. The refusal is a line of
    /// the decision log, of the action `promote`, for the principal that asked.
    async fn safety_gate(&self, entry: &Entry, phase: &str, proposer: &str) -> Result<(), Error> {
        if person_wrote(entry, &self.grants.owner) {
            return Ok(());
        }
        let (added, baseline) = self.added_by(entry).await?;
        if added.is_empty() {
            return Ok(());
        }
        let rule = "to a task definition that is in force: no change to the platform adds a permission, and a person writes the definition that does";
        let id = &entry.front.id;
        // the log keeps the decision whole; the session is told what it may read
        let reason = format!("{id} adds {} {rule}", listed(&added));
        let said = format!("{id} adds {} {rule}", self.told(&added, baseline)?);
        let mut input = self.promotion_input(entry, TrustTier::Verified, phase, proposer);
        input.held_task = self.held_task();
        self.log_decision(
            &input,
            &Decision {
                allow: false,
                admit: false,
                review: Review::Human,
                reason,
                policy: self.policy().version().to_string(),
                cites: Vec::new(),
                denials: Vec::new(),
                advisories: Vec::new(),
            },
        );
        Err(Error::Denied(said))
    }

    /// What the performance gate finds for the version `def`. The suite is read as the
    /// record holds it, whoever asks, and nothing of its text is told.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] when the suite in force does not read as one, since no run of
    /// it can pass; a store or ledger error.
    async fn person_run(&self, def: &TaskDefinition) -> Result<Run, Error> {
        let Some(suite) = self.suite_in_force(&def.name).await? else {
            return Ok(Run::NoSuite);
        };
        let (Some(_), Some(domain)) = (suite.pass.person, self.platform_domain()) else {
            return Ok(Run::NoSuite);
        };
        let reference = def.reference();
        let records = {
            let l = self.ledger.lock().await;
            l.definition_records(&domain, Some(&reference)).await?
        };
        let newest = records.iter().rev().find_map(|r| match &r.change {
            DefinitionChange::Graded {
                suite,
                level: Autonomy::Person,
                passed,
                of,
                needed,
                ..
            } => Some((suite, *passed, *of, *needed)),
            _ => None,
        });
        let first = "the evaluator grades the version first, and then a person verifies it";
        Ok(match newest {
            None => Run::Missing(format!(
                "no run of {reference} was graded for the level person on its held-out suite: {first}"
            )),
            Some((version, ..)) if *version != suite.version => Run::Missing(format!(
                "the newest run of {reference} for the level person is not of the held-out suite in force now: {first}"
            )),
            Some((_, passed, of, needed)) if passed < needed => Run::Missing(format!(
                "the newest run of {reference} for the level person failed, with {passed} of {of} scenarios passed where the level needs {needed}: {first}"
            )),
            Some(_) => Run::Passed,
        })
    }

    /// The performance gate: refuse the promotion of `entry` to verified while a task
    /// definition in it has a held-out suite with a pass mark for the level `person`
    /// and no passed run of that suite version.
    async fn performance_gate(&self, entry: &Entry) -> Result<(), Error> {
        for def in definitions_in(entry) {
            if let Run::Missing(why) = self.person_run(&def).await? {
                return Err(Error::Denied(why));
            }
        }
        Ok(())
    }

    /// The gates on a promotion of `entry` to verified, before the policy is asked: the
    /// safety gate, then the performance gate. Both must pass, and the order decides
    /// only which refusal a candidate that fails both gets. An entry outside the
    /// platform's domain passes: neither gate is about it.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] from the gate that refuses; a store or ledger error.
    pub(crate) async fn platform_gates(
        &self,
        entry: &Entry,
        phase: &str,
        proposer: &str,
    ) -> Result<(), Error> {
        if !self.in_platform(entry) {
            return Ok(());
        }
        self.safety_gate(entry, phase, proposer).await?;
        self.performance_gate(entry).await
    }

    /// What the safety gate says of the version `def` against `base`, for the report:
    /// whether it is met, and why. A session whose handling is below the entry of the
    /// baseline is told that the candidate adds a permission, and not which.
    fn safety_state(
        &self,
        entry: &Entry,
        def: &TaskDefinition,
        base: &TaskDefinition,
    ) -> Result<(bool, String), Error> {
        let added = added_to(base, def);
        let name = &def.name;
        if added.is_empty() {
            return Ok((
                true,
                format!("{name} declares no tool, scope or autonomy that the version before it does not"),
            ));
        }
        let what = self.told(&added, base.classification)?;
        if person_wrote(entry, &self.grants.owner) {
            return Ok((
                true,
                format!("{name} adds {what}, and a person wrote the entry"),
            ));
        }
        Ok((
            false,
            format!("{name} adds {what}: no change to the platform adds a permission, and a person writes the definition that does"),
        ))
    }

    /// The reach of one task block of `entry`, and what it needs, into `gates`.
    async fn task_reach(
        &self,
        entry: &Entry,
        name: &str,
        gates: &mut CandidateGates,
    ) -> Result<(), Error> {
        let Some(Ok(def)) = definition_block(entry, name) else {
            gates.reach.push(format!(
                "task {name}: the block does not read as a task definition, so it defines nothing; needs the review of a person"
            ));
            return Ok(());
        };
        let run = self.person_run(&def).await?;
        let performance = match &run {
            Run::NoSuite => (
                true,
                format!("no held-out suite gives a pass mark for the level person for {name}, so no run holds this version back"),
            ),
            Run::Passed => (
                true,
                format!("the newest run of {} for the level person passed", def.reference()),
            ),
            Run::Missing(why) => (false, why.clone()),
        };
        if let Some(base) = self.baseline(entry, name).await? {
            gates.reach.push(format!(
                "task {name}: a version of it was in force before this one, so a change reaches every task of that kind; needs the performance gate, the safety gate and the review of a person"
            ));
            let (met, why) = self.safety_state(entry, &def, &base)?;
            gates.safety.need(met, &why);
            gates.performance.need(performance.0, &performance.1);
            return Ok(());
        }
        let with_suite = !matches!(run, Run::NoSuite);
        gates.reach.push(format!(
            "task {name}: no version of it was in force before this one, so it is one new kind of work; needs the review of a person, of its tools against its purpose{}",
            if with_suite { ", and the performance gate" } else { "" }
        ));
        if with_suite {
            gates.performance.need(performance.0, &performance.1);
        }
        Ok(())
    }

    /// The gates that the entry `id` needs, by what its artifacts reach, and which of
    /// them are met. The entry is read as [`Memory::get`] reads it.
    ///
    /// One line of `reach` for each artifact. A task block whose name had a version in
    /// force before the entry reaches every task of that kind, and needs the performance gate, the safety
    /// gate and the review of a person. A task block with no version before it is one new
    /// kind of work: it needs the review of a person, and the performance gate only
    /// where a held-out suite gives a pass mark for the level `person`. An `agent`,
    /// `template` or `harness` block needs the review of a person. An `eval` block, and
    /// a `suite` block in the held-out domain, reach every later verdict: they need the
    /// review of a person always, and no other gate. The review is met once the entry
    /// is verified. An entry that holds no artifact needs no gate.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for an entry that this session cannot read, and what else
    /// [`Memory::get`] gives; [`Error::Denied`] when a version in force or a suite in
    /// force does not read; a store or ledger error.
    pub async fn candidate_gates(&self, id: &str) -> Result<CandidateGates, Error> {
        let entry = self.get(id).await?;
        let mut gates = CandidateGates {
            entry: entry.front.id.clone(),
            reach: Vec::new(),
            performance: GateState::open(),
            safety: GateState::open(),
            review: GateState::open(),
        };
        let held_out = self.held_out_domain().as_deref() == Some(entry.front.domain.as_str());
        let platform = self.in_platform(&entry);
        let mut tasks = BTreeSet::new();
        for a in artifacts_in(&entry) {
            if platform && a.kind == "task" {
                if tasks.insert(a.name.clone()) {
                    self.task_reach(&entry, &a.name, &mut gates).await?;
                }
            } else if let Some(line) = block_reach(&a.kind, &a.name, platform, held_out) {
                gates.reach.push(line);
            }
        }
        if !gates.reach.is_empty() {
            let verified = entry.front.trust_tier == TrustTier::Verified;
            let why = if verified {
                "a person verified the entry".to_string()
            } else {
                format!(
                    "the entry is {}; a person who did not propose it approves it",
                    entry.front.trust_tier
                )
            };
            gates.review.need(verified, &why);
        }
        Ok(gates)
    }
}

/// The reach of a block that is no task definition, and what it needs: none for a block
/// that nothing reads where it is. `platform` and `held_out` say which of the two
/// domains the entry is in.
fn block_reach(kind: &str, name: &str, platform: bool, held_out: bool) -> Option<String> {
    let loaded = platform && matches!(kind, "agent" | "template" | "harness");
    let grades = (platform && kind == "eval") || (held_out && kind == SUITE_KIND);
    if loaded {
        Some(format!(
            "{kind} {name}: a change reaches the sessions that load it; needs the review of a person"
        ))
    } else if grades {
        Some(format!(
            "{kind} {name}: a change reaches every later verdict; needs the review of a person always, and no other gate"
        ))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evaluation::Answer;
    use crate::testing::{as_author, held_out_root, root, verify, written, written_in, EVALUATOR};
    use crate::{Promotion, WriteRequest};
    use dm_core::EntryType;
    use dm_ledger::Task;
    use std::path::Path;

    const OWNER: &str = "human:test:me";
    const AGENT: &str = "agent:a";
    const CURATOR: &str = "agent:curator";

    /// The text of an entry that defines the task `name`.
    fn definition(name: &str, tools: &[&str], scope: &[&str], autonomy: &str) -> String {
        let quoted = |items: &[&str]| {
            let all: Vec<String> = items.iter().map(|i| format!("{i:?}")).collect();
            all.join(", ")
        };
        format!(
            "# One kind of work\n\n```platform task {name}\npurpose = \"Tidy one module.\"\ntools = [{}]\nscope = [{}]\nacceptance = [\"The handoff names each file that changed.\"]\nautonomy = \"{autonomy}\"\n```\n",
            quoted(tools),
            quoted(scope)
        )
    }

    /// The definition `tidy` with these tools, the scope `dom-me` and the lowest
    /// autonomy.
    fn tidy(tools: &[&str]) -> String {
        definition("tidy", tools, &["dom-me"], "person")
    }

    /// A raw entry of `dom-plat` that `author` wrote in a session of its own; its id.
    async fn candidate(dir: &Path, author: &str, body: &str, supersedes: Option<&str>) -> String {
        let request = WriteRequest {
            body: body.into(),
            domain: Some("dom-plat".into()),
            entry_type: EntryType::Convention,
            supersedes: supersedes.map(str::to_string),
            ..WriteRequest::default()
        };
        as_author(dir, author).write(request).await.unwrap().id
    }

    /// Propose `id` for verified in a new session of `by`.
    async fn propose(dir: &Path, by: &str, id: &str) -> Result<Promotion, Error> {
        as_author(dir, by)
            .promote(id, TrustTier::Verified, "read against the tree")
            .await
    }

    /// The task of a proposal that was filed.
    fn filed(proposal: Result<Promotion, Error>) -> Task {
        match proposal {
            Ok(Promotion::Proposed { task }) => *task,
            other => panic!("not proposed: {other:?}"),
        }
    }

    fn denied<T>(r: &Result<T, Error>, saying: &str) -> bool {
        matches!(r, Err(Error::Denied(why)) if why.contains(saying))
    }

    async fn events(m: &Memory) -> u64 {
        m.ledger.lock().await.event_count().await.unwrap()
    }

    async fn tier(m: &Memory, id: &str) -> TrustTier {
        let e = m.store.get("dom-plat", id).await.unwrap().unwrap();
        e.front.trust_tier
    }

    /// The reasons of the refused `promote` lines of the decision log for `principal`.
    fn refused_promotions(dir: &Path, principal: &str) -> Vec<String> {
        let log = std::fs::read_to_string(dir.join("state/decisions.jsonl")).unwrap();
        log.lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter(|v| {
                v["action"] == "promote" && v["allow"] == false && v["principal"] == principal
            })
            .map(|v| v["reason"].as_str().unwrap().to_string())
            .collect()
    }

    /// Rule 1.
    #[tokio::test]
    async fn an_agent_s_candidate_that_adds_a_tool_is_refused_and_the_refusal_is_logged() {
        let dir = tempfile::tempdir().unwrap();
        let owner = root(dir.path());
        written(&owner, &tidy(&["tool:fetch"]), true).await;
        let wider = tidy(&["tool:fetch", "tool:shell"]);
        let id = candidate(dir.path(), AGENT, &wider, None).await;
        let entry = owner.store.get("dom-plat", &id).await.unwrap().unwrap();
        assert_eq!(owner.widens(&entry).await.unwrap(), ["tool:shell"]);
        let before = events(&owner).await;
        let refused = propose(dir.path(), AGENT, &id).await;
        assert!(denied(&refused, "tool:shell"), "{refused:?}");
        assert!(denied(
            &refused,
            "no change to the platform adds a permission"
        ));
        assert!(denied(&refused, "a person writes the definition that does"));
        assert!(!denied(&refused, "tool:fetch"), "{refused:?}");
        assert_eq!(events(&owner).await, before, "no proposal was filed");
        assert_eq!(tier(&owner, &id).await, TrustTier::Raw);
        let logged = refused_promotions(dir.path(), AGENT);
        assert_eq!(logged.len(), 1, "{logged:?}");
        assert!(logged[0].contains("tool:shell"), "{logged:?}");
        let log = std::fs::read_to_string(dir.path().join("state/decisions.jsonl")).unwrap();
        let line = log
            .lines()
            .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
            .find(|v| v["action"] == "promote")
            .unwrap();
        assert_eq!(line["class"], "agent");
        assert_eq!(line["review"], "human");
        assert_eq!(line["domain"], "dom-plat");
    }

    /// Rule 2.
    #[tokio::test]
    async fn a_wider_scope_and_a_higher_autonomy_are_refused_as_an_added_tool_is() {
        let dir = tempfile::tempdir().unwrap();
        let owner = root(dir.path());
        let base = definition("tidy", &["tool:fetch"], &["dom-me"], "review");
        written(&owner, &base, true).await;
        let scope = definition("tidy", &["tool:fetch"], &["dom-me", "dom-plat"], "review");
        let autonomy = definition("tidy", &["tool:fetch"], &["dom-me"], "checks");
        let all = definition("tidy", &["tool:shell"], &["dom-plat"], "checks");
        let before = events(&owner).await;
        for (body, added) in [
            (&scope, vec!["scope:dom-plat"]),
            (&autonomy, vec!["autonomy:checks"]),
            (
                &all,
                vec!["autonomy:checks", "scope:dom-plat", "tool:shell"],
            ),
        ] {
            let id = candidate(dir.path(), AGENT, body, None).await;
            let entry = owner.store.get("dom-plat", &id).await.unwrap().unwrap();
            assert_eq!(owner.widens(&entry).await.unwrap(), added);
            let refused = propose(dir.path(), AGENT, &id).await;
            assert!(denied(&refused, &added.join(", ")), "{refused:?}");
        }
        assert_eq!(events(&owner).await, before, "no proposal was filed");
        assert_eq!(refused_promotions(dir.path(), AGENT).len(), 3);
    }

    /// Rule 3.
    #[tokio::test]
    async fn a_candidate_that_adds_nothing_is_proposed_as_before() {
        let dir = tempfile::tempdir().unwrap();
        let owner = root(dir.path());
        let base = definition(
            "tidy",
            &["tool:fetch", "tool:search"],
            &["dom-me"],
            "review",
        );
        written(&owner, &base, true).await;
        let fewer = definition("tidy", &["tool:search"], &[], "person");
        for body in [&base, &fewer] {
            let id = candidate(dir.path(), AGENT, body, None).await;
            let entry = owner.store.get("dom-plat", &id).await.unwrap().unwrap();
            assert_eq!(owner.widens(&entry).await.unwrap(), [""; 0]);
            let task = filed(propose(dir.path(), AGENT, &id).await);
            assert_eq!(task.spec.created_by, AGENT);
        }
        assert_eq!(refused_promotions(dir.path(), AGENT), [""; 0]);
    }

    /// Rule 4: the entry that a candidate supersedes is out of force at once, and the
    /// candidate is compared with it.
    #[tokio::test]
    async fn a_candidate_is_compared_with_the_version_that_it_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let owner = root(dir.path());
        let first = written(&owner, &tidy(&["tool:fetch"]), true).await;
        let wider = tidy(&["tool:fetch", "tool:shell"]);
        let second = candidate(dir.path(), OWNER, &wider, Some(&first)).await;
        assert_eq!(owner.task_definition("tidy").await.unwrap(), None);
        let entry = owner.store.get("dom-plat", &second).await.unwrap().unwrap();
        assert_eq!(owner.widens(&entry).await.unwrap(), ["tool:shell"]);
        // an agent's candidate that replaces the person's is compared with the verified
        // version at the start of the chain
        let widest = tidy(&["tool:fetch", "tool:shell", "tool:search"]);
        let third = candidate(dir.path(), AGENT, &widest, Some(&second)).await;
        let refused = propose(dir.path(), AGENT, &third).await;
        assert!(denied(&refused, "tool:search, tool:shell"), "{refused:?}");
    }

    /// Rule 4: of a version that is in force and a version that the candidate replaces,
    /// the newer is the one that was in force before the candidate.
    #[tokio::test]
    async fn the_newest_version_before_a_candidate_is_what_it_is_compared_with() {
        let dir = tempfile::tempdir().unwrap();
        let owner = root(dir.path());
        let wide = tidy(&["tool:fetch", "tool:shell"]);
        let old = written(&owner, &wide, true).await;
        let narrow = written(&owner, &tidy(&["tool:fetch"]), true).await;
        let back = candidate(dir.path(), OWNER, &wide, Some(&narrow)).await;
        // the old, wide version is in force again, and the candidate still adds a tool
        let in_force = owner.task_definition("tidy").await.unwrap().unwrap();
        assert_eq!(in_force.entry, old);
        let entry = owner.store.get("dom-plat", &back).await.unwrap().unwrap();
        assert_eq!(owner.widens(&entry).await.unwrap(), ["tool:shell"]);
    }

    /// Rule 5.
    #[tokio::test]
    async fn a_candidate_for_a_name_with_no_version_in_force_is_proposed() {
        let dir = tempfile::tempdir().unwrap();
        let owner = root(dir.path());
        written(&owner, &tidy(&["tool:fetch"]), true).await;
        // a version that nobody verified is no baseline
        written(&owner, &definition("sweep", &[], &[], "person"), false).await;
        let sweep = definition("sweep", &["tool:shell"], &["dom-plat"], "checks");
        let id = candidate(dir.path(), AGENT, &sweep, None).await;
        let entry = owner.store.get("dom-plat", &id).await.unwrap().unwrap();
        assert_eq!(owner.widens(&entry).await.unwrap(), [""; 0]);
        filed(propose(dir.path(), AGENT, &id).await);
        // an entry outside the platform's domain defines nothing, so it widens nothing
        let wider = tidy(&["tool:fetch", "tool:shell"]);
        let elsewhere = written_in(&as_author(dir.path(), AGENT), "dom-me", &wider, false).await;
        let entry = owner
            .store
            .get("dom-me", &elsewhere)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(owner.widens(&entry).await.unwrap(), [""; 0]);
    }

    /// A version in force that does not read is no reason to let a candidate through.
    #[tokio::test]
    async fn a_baseline_that_does_not_read_refuses_the_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let owner = root(dir.path());
        let broken = "```platform task tidy\nsecret_key_bluebird = \"TOPSECRET\"\n```\n";
        written(&owner, broken, true).await;
        let id = candidate(dir.path(), AGENT, &tidy(&["tool:shell"]), None).await;
        let refused = propose(dir.path(), AGENT, &id).await;
        assert!(denied(&refused, "does not read as one"), "{refused:?}");
        assert!(!denied(&refused, "bluebird"), "{refused:?}");
    }

    /// Rule 6.
    #[tokio::test]
    async fn a_person_s_candidate_is_not_refused_for_what_it_adds() {
        let dir = tempfile::tempdir().unwrap();
        let owner = root(dir.path());
        written(&owner, &tidy(&["tool:fetch"]), true).await;
        let wider = tidy(&["tool:fetch", "tool:shell"]);
        for person in [OWNER, "human:test:reviewer"] {
            let id = candidate(dir.path(), person, &wider, None).await;
            let entry = owner.store.get("dom-plat", &id).await.unwrap().unwrap();
            assert_eq!(owner.widens(&entry).await.unwrap(), ["tool:shell"]);
            filed(propose(dir.path(), CURATOR, &id).await);
        }
        assert_eq!(refused_promotions(dir.path(), CURATOR), [""; 0]);
    }

    /// Rule 7.
    #[tokio::test]
    async fn the_safety_gate_runs_again_when_a_person_approves() {
        let dir = tempfile::tempdir().unwrap();
        let owner = root(dir.path());
        let both = tidy(&["tool:fetch", "tool:search"]);
        written(&owner, &both, true).await;
        let id = candidate(dir.path(), AGENT, &both, None).await;
        let task = filed(propose(dir.path(), CURATOR, &id).await);
        // the version in force becomes narrower before a person reads the proposal
        written(&owner, &tidy(&["tool:fetch"]), true).await;
        let before = events(&owner).await;
        let refused = owner.approve(&task.id).await;
        assert!(denied(&refused, "tool:search"), "{refused:?}");
        assert_eq!(tier(&owner, &id).await, TrustTier::Raw);
        assert_eq!(events(&owner).await, before, "the proposal was not taken");
        let logged = refused_promotions(dir.path(), OWNER);
        assert_eq!(logged.len(), 1, "{logged:?}");
        // without the narrower version the same proposal is approved
        let other = tempfile::tempdir().unwrap();
        let owner = root(other.path());
        written(&owner, &both, true).await;
        let id = candidate(other.path(), AGENT, &both, None).await;
        let task = filed(propose(other.path(), CURATOR, &id).await);
        owner.approve(&task.id).await.unwrap();
        assert_eq!(tier(&owner, &id).await, TrustTier::Verified);
    }

    /// A suite for `tidy` with one scenario and these pass marks.
    fn suite(marks: &str, expect: &str) -> String {
        format!(
            "# The suite for tidy\n\n```platform suite tidy\n[pass]\n{marks}\n\n[[scenario]]\nname = \"files question\"\nask = \"Which files changed?\"\nexpect = [\"{expect}\"]\n```\n"
        )
    }

    /// A handoff of `agent:a` in `dom-me` with `text`; its id.
    async fn handoff(dir: &Path, text: &str) -> String {
        let h = WriteRequest {
            body: text.into(),
            domain: Some("dom-me".into()),
            entry_type: EntryType::Handoff,
            ..WriteRequest::default()
        };
        as_author(dir, AGENT).write(h).await.unwrap().id
    }

    /// Grade the version `tidy@<entry>` for the level `person` on the handoff, as the
    /// evaluator. Returns whether the run passed.
    async fn graded(dir: &Path, entry: &str, handoff: &str) -> bool {
        let answers = [Answer {
            scenario: "files question".into(),
            handoff: handoff.into(),
        }];
        let version = format!("tidy@{entry}");
        let evaluator = as_author(dir, EVALUATOR);
        let run = evaluator.grade(&version, Autonomy::Person, &answers);
        run.await.unwrap().pass
    }

    /// Rule 8.
    #[tokio::test]
    async fn a_candidate_needs_a_passed_run_of_the_suite_in_force() {
        let dir = tempfile::tempdir().unwrap();
        let owner = held_out_root(dir.path());
        let good = handoff(dir.path(), "Changed src/importer.rs.\n").await;
        let bad = handoff(dir.path(), "Changed nothing.\n").await;
        // a suite with no pass mark for the level person holds nothing back
        written_in(
            &owner,
            "dom-held",
            &suite("review = 1", "src/importer"),
            true,
        )
        .await;
        let id = candidate(dir.path(), OWNER, &tidy(&["tool:fetch"]), None).await;
        let task = filed(propose(dir.path(), CURATOR, &id).await);
        written_in(
            &owner,
            "dom-held",
            &suite("person = 1", "src/importer"),
            true,
        )
        .await;
        let first = "the evaluator grades the version first";
        let no_run = "no run of";
        let refused = propose(dir.path(), CURATOR, &id).await;
        assert!(
            denied(&refused, no_run) && denied(&refused, first),
            "{refused:?}"
        );
        let refused = owner.approve(&task.id).await;
        assert!(
            denied(&refused, no_run) && denied(&refused, first),
            "{refused:?}"
        );
        assert!(!graded(dir.path(), &id, &bad).await);
        let refused = owner.approve(&task.id).await;
        assert!(denied(&refused, "failed, with 0 of 1"), "{refused:?}");
        assert!(denied(&refused, first), "{refused:?}");
        assert_eq!(tier(&owner, &id).await, TrustTier::Raw);
        assert!(graded(dir.path(), &id, &good).await);
        owner.approve(&task.id).await.unwrap();
        assert_eq!(tier(&owner, &id).await, TrustTier::Verified);
        // a run of an older version of the suite does not count
        let next = candidate(dir.path(), OWNER, &tidy(&["tool:search"]), None).await;
        assert!(graded(dir.path(), &next, &good).await);
        written_in(
            &owner,
            "dom-held",
            &suite("person = 1", "importer.rs"),
            true,
        )
        .await;
        let refused = propose(dir.path(), CURATOR, &next).await;
        assert!(
            denied(&refused, "not of the held-out suite in force now"),
            "{refused:?}"
        );
        assert!(
            denied(&refused, first) && !denied(&refused, "importer"),
            "{refused:?}"
        );
        assert!(graded(dir.path(), &next, &good).await);
        filed(propose(dir.path(), CURATOR, &next).await);
        // the gate is about the platform's domain: the same text elsewhere is proposed
        let writer = as_author(dir.path(), AGENT);
        let elsewhere = written_in(&writer, "dom-me", &tidy(&["tool:fetch"]), false).await;
        filed(propose(dir.path(), CURATOR, &elsewhere).await);
    }

    /// A suite in force that does not read can pass no run, so it holds a candidate back.
    #[tokio::test]
    async fn a_suite_that_does_not_read_holds_a_candidate_back() {
        let dir = tempfile::tempdir().unwrap();
        let owner = held_out_root(dir.path());
        let broken = "```platform suite tidy\nbird = \"BLUEBIRD\"\n```\n";
        written_in(&owner, "dom-held", broken, true).await;
        let id = candidate(dir.path(), OWNER, &tidy(&["tool:fetch"]), None).await;
        let refused = propose(dir.path(), CURATOR, &id).await;
        assert!(denied(&refused, "does not read as one"), "{refused:?}");
        assert!(!denied(&refused, "BLUEBIRD"), "{refused:?}");
        let report = owner.candidate_gates(&id).await;
        assert!(denied(&report, "does not read as one"), "{report:?}");
    }

    /// A promotion to curated passes no gate of the platform.
    #[tokio::test]
    async fn a_promotion_to_curated_passes_no_gate() {
        let dir = tempfile::tempdir().unwrap();
        let owner = held_out_root(dir.path());
        written(&owner, &tidy(&["tool:fetch"]), true).await;
        written_in(
            &owner,
            "dom-held",
            &suite("person = 1", "src/importer"),
            true,
        )
        .await;
        let wider = tidy(&["tool:fetch", "tool:shell"]);
        let id = candidate(dir.path(), AGENT, &wider, None).await;
        let curated = as_author(dir.path(), CURATOR)
            .promote(&id, TrustTier::Curated, "read against the tree")
            .await;
        assert!(
            matches!(curated, Ok(Promotion::Promoted { .. })),
            "{curated:?}"
        );
        assert_eq!(tier(&owner, &id).await, TrustTier::Curated);
        assert_eq!(refused_promotions(dir.path(), CURATOR), [""; 0]);
    }

    /// Rule 9: a task definition.
    #[tokio::test]
    async fn the_report_names_the_gates_that_a_task_definition_needs() {
        let dir = tempfile::tempdir().unwrap();
        let owner = held_out_root(dir.path());
        let base = written(&owner, &tidy(&["tool:fetch"]), true).await;
        let wider = tidy(&["tool:fetch", "tool:shell"]);
        let id = candidate(dir.path(), AGENT, &wider, None).await;
        let gates = owner.candidate_gates(&id).await.unwrap();
        assert_eq!(gates.entry, id);
        assert_eq!(gates.reach.len(), 1, "{gates:?}");
        assert!(
            gates.reach[0].starts_with("task tidy: a version of it was in force before this one")
        );
        assert!(gates.reach[0].contains("every task of that kind"));
        let needed = |g: &CandidateGates| (g.performance.needed, g.safety.needed, g.review.needed);
        assert_eq!(needed(&gates), (true, true, true));
        assert!(
            !gates.safety.met && gates.safety.why.contains("tool:shell"),
            "{gates:?}"
        );
        assert!(!gates.safety.why.contains("tool:fetch"), "{gates:?}");
        assert!(
            !gates.review.met && gates.review.why.contains("raw"),
            "{gates:?}"
        );
        assert!(gates.performance.met, "{gates:?}");
        assert!(gates
            .performance
            .why
            .contains("no held-out suite gives a pass mark"));
        // the same text from a person: the gate is needed and holds nothing back
        let theirs = candidate(dir.path(), OWNER, &wider, None).await;
        let gates = owner.candidate_gates(&theirs).await.unwrap();
        assert!(gates.safety.needed && gates.safety.met, "{gates:?}");
        assert!(gates.safety.why.contains("tool:shell"), "{gates:?}");
        assert!(
            gates.safety.why.contains("a person wrote the entry"),
            "{gates:?}"
        );
        // with a suite, a version with no run does not meet the performance gate
        written_in(
            &owner,
            "dom-held",
            &suite("person = 1", "src/importer"),
            true,
        )
        .await;
        let gates = owner.candidate_gates(&id).await.unwrap();
        assert!(
            gates.performance.needed && !gates.performance.met,
            "{gates:?}"
        );
        assert!(gates
            .performance
            .why
            .contains("the evaluator grades the version first"));
        let good = handoff(dir.path(), "Changed src/importer.rs.\n").await;
        assert!(graded(dir.path(), &id, &good).await);
        let gates = owner.candidate_gates(&id).await.unwrap();
        assert!(
            gates.performance.met && gates.performance.why.contains("passed"),
            "{gates:?}"
        );
        // the version in force had no version before it, and a person verified it
        let gates = owner.candidate_gates(&base).await.unwrap();
        assert!(gates.reach[0].starts_with("task tidy: no version of it was in force before"));
        assert_eq!(needed(&gates), (true, false, true));
        assert!(
            gates.review.met && gates.review.why.contains("a person verified"),
            "{gates:?}"
        );
    }

    /// Rule 9: what is no task definition in force, and who reads the report.
    #[tokio::test]
    async fn the_report_asks_only_a_person_s_review_for_a_suite_and_for_a_new_kind_of_work() {
        let dir = tempfile::tempdir().unwrap();
        let owner = held_out_root(dir.path());
        let needed = |g: &CandidateGates| (g.performance.needed, g.safety.needed, g.review.needed);
        let held = written_in(
            &owner,
            "dom-held",
            &suite("person = 1", "src/importer"),
            true,
        )
        .await;
        let gates = owner.candidate_gates(&held).await.unwrap();
        assert_eq!(gates.reach.len(), 1, "{gates:?}");
        assert!(gates.reach[0].starts_with("suite tidy: a change reaches every later verdict"));
        assert_eq!(needed(&gates), (false, false, true));
        assert!(
            gates.review.met && gates.performance.met && gates.safety.met,
            "{gates:?}"
        );
        // a session that does not read the entry learns nothing of it
        for session in [as_author(dir.path(), AGENT), as_author(dir.path(), CURATOR)] {
            let hidden = session.candidate_gates(&held).await;
            assert!(matches!(hidden, Err(Error::NotFound(_))), "{hidden:?}");
        }
        // a new kind of work: review, and the performance gate where a suite is
        let sweep = definition("sweep", &["tool:shell"], &[], "person");
        let id = candidate(dir.path(), AGENT, &sweep, None).await;
        let gates = owner.candidate_gates(&id).await.unwrap();
        assert!(
            gates.reach[0].starts_with("task sweep: no version of it was in force before this one")
        );
        assert!(gates.reach[0].ends_with("against its purpose"), "{gates:?}");
        assert_eq!(needed(&gates), (false, false, true));
        let id = candidate(dir.path(), AGENT, &tidy(&["tool:shell"]), None).await;
        let gates = owner.candidate_gates(&id).await.unwrap();
        assert!(
            gates.reach[0].ends_with("and the performance gate"),
            "{gates:?}"
        );
        assert_eq!(needed(&gates), (true, false, true));
        assert!(!gates.performance.met, "{gates:?}");
        // the blocks that the launcher loads, an eval suite, and blocks that nothing reads
        let body = "```platform agent claude\nRead the decision first.\n```\n\n```platform eval docs\nq = 1\n```\n\n```platform suite tidy\nnot read here\n```\n\n```platform note x\nnot read\n```\n";
        let id = candidate(dir.path(), OWNER, body, None).await;
        let gates = owner.candidate_gates(&id).await.unwrap();
        assert_eq!(gates.reach.len(), 2, "{gates:?}");
        assert!(gates.reach[0].starts_with("agent claude: a change reaches the sessions"));
        assert!(gates.reach[1].starts_with("eval docs: a change reaches every later verdict"));
        assert_eq!(needed(&gates), (false, false, true));
        assert!(!gates.review.met, "{gates:?}");
        // an entry with no artifact that is read where it is needs no gate
        let elsewhere = "A fact.\n\n```platform agent claude\nnot loaded from here\n```\n";
        let plain = written_in(&owner, "dom-me", elsewhere, false).await;
        let gates = owner.candidate_gates(&plain).await.unwrap();
        assert_eq!(gates.reach, [""; 0]);
        assert_eq!(needed(&gates), (false, false, false));
        assert!(gates.review.met, "{gates:?}");
    }

    /// What a candidate adds says what the version before it does not declare: a
    /// session whose handling is below that version is not told which.
    #[tokio::test]
    async fn a_session_below_the_version_in_force_is_not_told_what_a_candidate_adds() {
        let dir = tempfile::tempdir().unwrap();
        let owner = root(dir.path());
        let base = WriteRequest {
            body: tidy(&["tool:fetch"]),
            domain: Some("dom-plat".into()),
            entry_type: EntryType::Convention,
            classification: Some(Label::Restricted),
            ..WriteRequest::default()
        };
        let base = owner.write(base).await.unwrap().id;
        verify(&owner, &base).await;
        let wider = tidy(&["tool:fetch", "tool:shell"]);
        let low = || as_author(dir.path(), AGENT).with_handling_cap(Label::Internal);
        let request = WriteRequest {
            body: wider,
            domain: Some("dom-plat".into()),
            entry_type: EntryType::Convention,
            ..WriteRequest::default()
        };
        let id = low().write(request).await.unwrap().id;
        let proposer = low();
        let refused = proposer
            .promote(&id, TrustTier::Verified, "read against the tree")
            .await;
        assert!(denied(&refused, "adds a permission to"), "{refused:?}");
        assert!(!denied(&refused, "tool:shell"), "{refused:?}");
        assert!(proposer.label() < Label::Restricted);
        // the log keeps the decision whole
        let logged = refused_promotions(dir.path(), AGENT);
        assert!(logged[0].contains("tool:shell"), "{logged:?}");
        let gates = low().candidate_gates(&id).await.unwrap();
        assert!(!gates.safety.met, "{gates:?}");
        assert!(
            gates.safety.why.contains("tidy adds a permission:"),
            "{gates:?}"
        );
        assert!(!gates.safety.why.contains("tool:shell"), "{gates:?}");
        // a session that may read the version is told, and has then read it
        let high = Memory::open(dir.path()).unwrap();
        let gates = high.candidate_gates(&id).await.unwrap();
        assert!(gates.safety.why.contains("tool:shell"), "{gates:?}");
        assert_eq!(high.label(), Label::Restricted);
    }
}
