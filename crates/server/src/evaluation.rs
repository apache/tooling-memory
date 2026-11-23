//! Earning an autonomy level: a held-out suite, the evaluator that grades on it, and the
//! raise that a passing run permits.
//!
//! The grants may name one domain that holds the held-out suites
//! (`held_out = "dom-held"`) and one principal that grades (`evaluator = "agent:<name>"`).
//! Only the evaluator and the owner in person read that domain: the session filter takes
//! it away from every other session, so no read path serves a scenario to a session
//! that claims tasks. The evaluator claims no task, and it grades no handoff that it
//! wrote.
//!
//! A suite is a fenced block in an entry of the held-out domain, TOML:
//!
//! ````text
//! ```platform suite tidy
//! [pass]
//! review = 2
//!
//! [[scenario]]
//! name = "lease-length"
//! ask = "Which decision set the lease length?"
//! expect = ["DR-"]
//! ```
//! ````
//!
//! The name in the info string is the name of the task definition that the suite
//! grades. A suite is in force only from an active entry that a person verified, and
//! the newest one is in force. `[pass]` says how many scenarios must pass for each
//! level; a level with no pass mark cannot be graded.
//!
//! [`Memory::grade`] grades one version of a definition for one level: each scenario
//! passes when the handoff that answers it holds every string of `expect`. The run is a
//! record on the ledger with the counts, and with no text of the suite.
//! [`Memory::raise`] moves the version in force one level up when its newest run for
//! that level passed on the suite in force. The step to `checks` is a person's, in a
//! session that the memory server does not serve, and that call is the sign-off.

use crate::autonomy::level_after;
use crate::platform::{artifacts_in, definition_block, definition_name, unreadable};
use crate::platform::{Autonomy, TaskDefinition};
use crate::{lock, Error, Memory};
use dm_core::{Entry, EntryType, ReadFilter, TrustTier};
use dm_ledger::{DefinitionChange, DefinitionRecord};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// The kind of the block that holds a suite: `platform suite <definition name>`. It is
/// not one of [`crate::platform::KINDS`]: the launcher loads nothing from it.
pub const SUITE_KIND: &str = "suite";

/// One scenario of a held-out suite.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    /// Its name, one time in its suite. An answer names it.
    pub name: String,
    /// What the work is asked.
    #[serde(default)]
    pub ask: String,
    /// The strings that the handoff of the work must hold, each of them.
    pub expect: Vec<String>,
}

/// How many scenarios of a suite must pass for each autonomy level.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PassMarks {
    /// For work at the level `person`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub person: Option<u32>,
    /// To earn the level `review`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<u32>,
    /// To earn the level `checks`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checks: Option<u32>,
}

impl PassMarks {
    /// The pass mark for `level`, when the suite gives one.
    #[must_use]
    pub fn for_level(self, level: Autonomy) -> Option<u32> {
        match level {
            Autonomy::Person => self.person,
            Autonomy::Review => self.review,
            Autonomy::Checks => self.checks,
        }
    }
}

/// The text of a suite block.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SuiteText {
    #[serde(default)]
    pass: PassMarks,
    #[serde(default)]
    scenario: Vec<Scenario>,
}

/// A held-out suite: the scenarios that grade one task definition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Suite {
    /// The name of the task definition that it grades, from the block's info string.
    pub name: String,
    /// The entry it is in.
    pub entry: String,
    /// Its version: `<entry>@<digest of the entry's text>`. A run names it, so a run on
    /// a suite that changed since is known as one.
    pub version: String,
    /// How many scenarios must pass for each level.
    pub pass: PassMarks,
    /// Its scenarios, in the order of the text.
    pub scenarios: Vec<Scenario>,
}

/// One answer to a scenario: the handoff of the work that was done on it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answer {
    /// The scenario, by its name in the suite.
    pub scenario: String,
    /// The handoff entry that records the work.
    pub handoff: String,
}

/// What a graded run gave.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Graded {
    /// The record of the run on the ledger: the counts, the version of the suite and
    /// the grader.
    pub record: DefinitionRecord,
    /// Whether as many scenarios passed as the level needs.
    pub pass: bool,
    /// The scenarios that did not pass, by name, in the order of the suite. Text of the
    /// suite: it goes to the grader and is in no record.
    pub failed: Vec<String>,
}

/// The suite that the block of `e` named `name` gives: none when the entry has no such
/// block, and an error when the block does not read as a suite.
fn suite_block(e: &Entry, name: &str) -> Option<Result<Suite, ()>> {
    let block = artifacts_in(e)
        .into_iter()
        .find(|a| a.kind == SUITE_KIND && a.name == name)?;
    Some(read_suite(&block.text, name, e))
}

/// Read the text of a suite block. It reads as a suite when it has a scenario, each
/// scenario has a name of its own that is not blank and expects a string that is not
/// blank, and each pass mark is from 1 to the number of scenarios.
fn read_suite(text: &str, name: &str, e: &Entry) -> Result<Suite, ()> {
    let parsed: SuiteText = toml::from_str(text).map_err(|_| ())?;
    let count = u32::try_from(parsed.scenario.len()).map_err(|_| ())?;
    let mut names = BTreeSet::new();
    let scenarios_read = parsed.scenario.iter().all(|s| {
        !s.name.trim().is_empty()
            && names.insert(s.name.as_str())
            && !s.expect.is_empty()
            && s.expect.iter().all(|x| !x.trim().is_empty())
    });
    let marks = [parsed.pass.person, parsed.pass.review, parsed.pass.checks];
    let marks_read = marks.iter().flatten().all(|m| (1..=count).contains(m));
    if count == 0 || !scenarios_read || !marks_read {
        return Err(());
    }
    Ok(Suite {
        name: name.to_string(),
        entry: e.front.id.clone(),
        version: format!("{}@{}", e.front.id, dm_core::digest(&e.render())),
        pass: parsed.pass,
        scenarios: parsed.scenario,
    })
}

/// The refusal for a block that is no suite: the name of its definition, and nothing of
/// its text.
fn not_a_suite(name: &str) -> Error {
    Error::Denied(format!(
        "the held-out suite for {name} does not read as one: it needs a scenario, a name of its own and an expected string for each scenario, and pass marks from 1 to the number of scenarios"
    ))
}

/// The refusal for an answer that does not fit the suite. It quotes the caller's own
/// words and nothing of the suite.
fn answers_fit(suite: &Suite, answers: &[Answer]) -> Result<(), Error> {
    let mut seen = BTreeSet::new();
    for a in answers {
        if !suite.scenarios.iter().any(|s| s.name == a.scenario) {
            return Err(Error::Denied(format!(
                "the held-out suite for {} has no scenario {:?}",
                suite.name, a.scenario
            )));
        }
        if !seen.insert(a.scenario.as_str()) {
            return Err(Error::Denied(format!(
                "the scenario {:?} has two answers: a run gives one handoff for each scenario",
                a.scenario
            )));
        }
    }
    Ok(())
}

/// The newest run in `records`, the records of `reference`, that was graded for the
/// level `next`, when a raise to that level can stand on it: it passed, it is of the
/// suite version `in_force`, and no work outcome was recorded after it.
fn run_to_raise_on<'a>(
    reference: &str,
    records: &'a [DefinitionRecord],
    next: Autonomy,
    in_force: Option<&str>,
) -> Result<&'a DefinitionRecord, Error> {
    let newest = records.iter().enumerate().rev().find_map(|(at, r)| {
        if let DefinitionChange::Graded {
            suite,
            level,
            passed,
            of,
            needed,
            ..
        } = &r.change
        {
            (*level == next).then_some((at, r, suite, (*passed, *of, *needed)))
        } else {
            None
        }
    });
    let Some((at, run, suite, (passed, of, needed))) = newest else {
        return Err(Error::Denied(format!(
            "no run of {reference} was graded for the level {next}: a level is earned on a run of the held-out suite"
        )));
    };
    if passed < needed {
        return Err(Error::Denied(format!(
            "the newest run for the level {next} failed: {passed} of {of} scenarios passed, and the level needs {needed}"
        )));
    }
    if in_force != Some(suite.as_str()) {
        return Err(Error::Denied(format!(
            "the newest run for the level {next} is not of the held-out suite in force now: grade the definition again"
        )));
    }
    let lowered_since = records[at + 1..]
        .iter()
        .any(|r| matches!(r.change, DefinitionChange::Outcome { .. }));
    if lowered_since {
        return Err(Error::Denied(format!(
            "a work outcome was recorded after the newest run for the level {next}: grade the definition again"
        )));
    }
    Ok(run)
}

impl Memory {
    /// The domain that holds the held-out suites, when the grants name one they hold.
    #[must_use]
    pub fn held_out_domain(&self) -> Option<String> {
        self.grants
            .held_out
            .clone()
            .filter(|d| self.grants.domains.contains_key(d))
    }

    /// Whether this session's principal is the evaluator that the grants name.
    fn is_evaluator(&self) -> bool {
        self.grants.evaluator.as_deref() == Some(self.author.as_str())
    }

    /// Whether this session reads the held-out domain: its principal is the evaluator,
    /// or it is the owner in a session that the memory server does not serve. A harness
    /// reaches a served session, so an agent could read the scenarios through it,
    /// whatever the name of the session.
    #[must_use]
    pub fn reads_held_out(&self) -> bool {
        self.is_evaluator() || (self.author == self.grants.owner && !self.served())
    }

    /// Whether `domain` is the held-out domain and this session does not read it.
    pub(crate) fn withheld(&self, domain: &str) -> bool {
        self.held_out_domain().as_deref() == Some(domain) && !self.reads_held_out()
    }

    /// `f` without the held-out domain, for a session that does not read it. A filter
    /// that then names no domain names one that no entry has: an empty list means
    /// every domain to the index.
    pub(crate) fn without_held_out(&self, mut f: ReadFilter) -> ReadFilter {
        if !f.domains.iter().any(|d| self.withheld(d)) {
            return f;
        }
        f.domains.retain(|d| !self.withheld(d));
        if f.domains.is_empty() {
            f.domains = vec![String::new()];
        }
        f
    }

    /// Refuse a write into the held-out domain from a session that does not read it: a
    /// session writes only where it reads, or a worker would write the suite that
    /// grades it.
    pub(crate) fn may_write_into(&self, domain: &str) -> Result<(), Error> {
        if self.withheld(domain) {
            return Err(Error::Denied(format!(
                "domain {domain} is outside what this session reads, and a session writes only where it reads"
            )));
        }
        Ok(())
    }

    /// Refuse a claim from the evaluator: it grades work, so it does none.
    pub(crate) fn claims_work(&self) -> Result<(), Error> {
        if self.is_evaluator() {
            return Err(Error::Denied(format!(
                "{} is the evaluator: the evaluator grades work and claims none",
                self.author
            )));
        }
        Ok(())
    }

    /// The entry that holds the suite in force for the definition `name`: the newest
    /// active entry of the held-out domain that a person verified and that has a suite
    /// block of that name, whether or not the block reads. Read as the record holds it,
    /// whoever asks: nothing of it is told to a session here.
    async fn suite_entry(&self, name: &str) -> Result<Option<Entry>, Error> {
        let Some(domain) = self.held_out_domain() else {
            return Ok(None);
        };
        let mut newest: Option<Entry> = None;
        for id in self.store.list(&domain).await? {
            let Some(e) = self.store.get(&domain, &id).await? else {
                continue;
            };
            let in_force = e.is_active()
                && e.front.trust_tier == TrustTier::Verified
                && artifacts_in(&e)
                    .iter()
                    .any(|a| a.kind == SUITE_KIND && a.name == name);
            let later = newest
                .as_ref()
                .is_none_or(|n| e.front.created > n.front.created);
            if in_force && later {
                newest = Some(e);
            }
        }
        Ok(newest)
    }

    /// The version of the suite in force for the definition `name`: none when there is
    /// no suite, or when the block in force does not read as one. The version is an
    /// entry id and a digest, so it tells nothing of the suite's text.
    async fn suite_version_in_force(&self, name: &str) -> Result<Option<String>, Error> {
        let entry = self.suite_entry(name).await?;
        let suite = entry.and_then(|e| suite_block(&e, name));
        Ok(suite.and_then(Result::ok).map(|s| s.version))
    }

    /// The suite in force for the definition `name`, as the record holds it, whoever
    /// asks: none when there is no suite. The caller tells nothing of its text to a
    /// session; [`Memory::held_out_suite`] is the read for a session.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] when the block in force does not read as a suite, in words
    /// that quote none of its text; a store error.
    pub(crate) async fn suite_in_force(&self, name: &str) -> Result<Option<Suite>, Error> {
        let Some(entry) = self.suite_entry(name).await? else {
            return Ok(None);
        };
        match suite_block(&entry, name) {
            Some(Ok(suite)) => Ok(Some(suite)),
            _ => Err(not_a_suite(name)),
        }
    }

    /// The held-out suite in force for the task definition `name`, for a session that
    /// reads the held-out domain: the one in the newest active entry of that domain
    /// that a person verified. None when there is none, or when the entry is above
    /// what the session may read. The session has then read the entry, and its label
    /// is no lower.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for every session that does not read the held-out domain,
    /// whether or not a suite is there; [`Error::Denied`] when the block in force does
    /// not read as a suite, in words that quote none of its text; a store or index
    /// error.
    pub async fn held_out_suite(&self, name: &str) -> Result<Option<Suite>, Error> {
        if !self.reads_held_out() {
            return Err(Error::NotFound(name.to_string()));
        }
        let Some(entry) = self.suite_entry(name).await? else {
            return Ok(None);
        };
        // under the filter that every read is under, before anything of the entry is told
        let filter = self.session_filter()?;
        let seen = self.index.lock().await.meta(&entry.front.id, &filter)?;
        if seen.is_none() {
            return Ok(None);
        }
        let Some(Ok(suite)) = suite_block(&entry, name) else {
            return Err(not_a_suite(name));
        };
        lock(&self.session).observe(entry.front.classification);
        Ok(Some(suite))
    }

    /// The version of a definition that a run grades. A bare name is the version in
    /// force. `<name>@<entry>` is an active entry of the platform's domain that has a
    /// task block of that name which reads, verified or not: a candidate is graded
    /// before a person verifies it. The session must be one that may read the entry,
    /// and it has then read it.
    async fn gradable_definition(&self, named: &str) -> Result<TaskDefinition, Error> {
        let name = definition_name(named);
        let Some((_, entry)) = named.split_once('@') else {
            return self.readable_definition(name).await;
        };
        let none = || {
            Error::Denied(format!(
                "no version {named} of a task definition is there to grade: an active entry of the platform's domain defines one"
            ))
        };
        let Some(domain) = self.platform_domain() else {
            return Err(none());
        };
        // an id that is no id names no entry
        let found = self.store.get(&domain, entry).await.ok().flatten();
        let Some(entry) = found.filter(Entry::is_active) else {
            return Err(none());
        };
        let filter = self.session_filter()?;
        let seen = self.index.lock().await.meta(&entry.front.id, &filter)?;
        if seen.is_none() {
            return Err(none());
        }
        match definition_block(&entry, name) {
            None => Err(none()),
            Some(Err(())) => Err(unreadable(name)),
            Some(Ok(def)) => {
                lock(&self.session).observe(def.classification);
                Ok(def)
            }
        }
    }

    /// Whether the handoff that `answer` names passes `scenario`: its body holds every
    /// string that the scenario expects. The session reads the handoff, as
    /// [`Memory::get`] does.
    async fn answer_holds(&self, scenario: &Scenario, answer: &Answer) -> Result<bool, Error> {
        let handoff = self.get(&answer.handoff).await?;
        if handoff.front.entry_type != EntryType::Handoff {
            return Err(Error::Denied(format!(
                "{} is no handoff: an answer names the handoff of the work",
                answer.handoff
            )));
        }
        if handoff.front.author == self.author {
            return Err(Error::Denied(format!(
                "{} wrote {}: nobody grades its own work",
                self.author, answer.handoff
            )));
        }
        Ok(scenario.expect.iter().all(|x| handoff.body.contains(x)))
    }

    /// Grade one version of a task definition for one autonomy level on the held-out
    /// suite in force for it, and record the run. `definition` is a bare name, for the
    /// version in force, or `<name>@<entry>`, for an active entry of the platform's
    /// domain that defines it, verified or not.
    ///
    /// Each answer names a scenario of the suite and the handoff of the work on it. A
    /// scenario passes when its handoff holds every string that the scenario expects. A
    /// scenario with no answer fails. The run passes when as many scenarios passed as
    /// the suite asks for the level. The record of the run holds the counts, the
    /// version of the suite and the grader, and no text of the suite. A run moves no
    /// level: [`Memory::raise`] does, on the newest run.
    ///
    /// The caller is the evaluator, or the owner in a session that the memory server
    /// does not serve. It reads the suite and each handoff, so its label is no lower
    /// than they are.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] from every other principal; when no such version of the
    /// definition is there for this session; when no suite is in force for the
    /// definition, the suite does not read as one, or it gives no pass mark for the
    /// level; for an answer to a scenario that the suite does not have, a second answer
    /// to one scenario, an answer that names an entry which is no handoff, and a handoff
    /// that the grader wrote. [`Error::NotFound`], or what else [`Memory::get`] gives,
    /// for a handoff that the session cannot read. A ledger, store or index error. A
    /// refused run records nothing.
    pub async fn grade(
        &self,
        definition: &str,
        level: Autonomy,
        answers: &[Answer],
    ) -> Result<Graded, Error> {
        if !self.reads_held_out() {
            return Err(Error::Denied(
                "only the evaluator grades a task definition, or the owner in a session that the memory server does not serve"
                    .into(),
            ));
        }
        let def = self.gradable_definition(definition).await?;
        let name = def.name.as_str();
        let (Some(domain), Some(suite)) =
            (self.platform_domain(), self.held_out_suite(name).await?)
        else {
            return Err(Error::Denied(format!(
                "no held-out suite is in force for the task definition {name}: a verified entry of the held-out domain holds one"
            )));
        };
        let Some(needed) = suite.pass.for_level(level) else {
            return Err(Error::Denied(format!(
                "the held-out suite for {name} gives no pass mark for the level {level}"
            )));
        };
        answers_fit(&suite, answers)?;
        let mut failed = Vec::new();
        for scenario in &suite.scenarios {
            let passed = match answers.iter().find(|a| a.scenario == scenario.name) {
                Some(answer) => self.answer_holds(scenario, answer).await?,
                None => false,
            };
            if !passed {
                failed.push(scenario.name.clone());
            }
        }
        let count = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
        let of = count(suite.scenarios.len());
        let passed = of - count(failed.len());
        let change = DefinitionChange::Graded {
            suite: suite.version,
            level,
            passed,
            of,
            needed,
            by: self.author.clone(),
        };
        let mut l = self.ledger.lock().await;
        let record = l
            .record_definition(&domain, &def.reference(), change)
            .await?;
        Ok(Graded {
            record,
            pass: passed >= needed,
            failed,
        })
    }

    /// Who signs a raise to `next` off, when this session may make it. The step to
    /// `review` is the evaluator's or a person's, and nobody signs it. The step to
    /// `checks` is a person's, in a session that the memory server does not serve, and
    /// the call is that person's sign-off.
    fn may_raise_to(&self, next: Autonomy) -> Result<Option<String>, Error> {
        if next == Autonomy::Checks {
            self.in_person("signs a task definition off for the level checks")?;
            return Ok(Some(self.author.clone()));
        }
        if self.is_evaluator() {
            return Ok(None);
        }
        self.in_person("or the evaluator raises a task definition to the level review")?;
        Ok(None)
    }

    /// Raise the autonomy level of the version in force of the task definition `name`
    /// one step, on its newest graded run for the next level. The run must have
    /// passed, on the version of the held-out suite that is in force now, and no work
    /// outcome may be newer than the run. The level does not go above the `autonomy`
    /// that the text of the definition declares.
    ///
    /// The step to `review` is the evaluator's, or a person's in a session that the
    /// memory server does not serve. The step to `checks` is only such a person's: the
    /// call is the sign-off, and the record names the person in `signed_by`.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] when no definition of that name is in force for this session,
    /// in the words a task that names it is refused in; for `<name>@<entry>` that is
    /// not the version in force; when the level is `checks` already; when the text of
    /// the definition declares a lower level than the next; from a principal that may
    /// not take the step; when there is no run for the next level, or the newest one
    /// failed, is of another version of the suite than the one in force, or is older
    /// than a work outcome. A ledger, store or index error. A refused raise records
    /// nothing.
    pub async fn raise(&self, name: &str) -> Result<DefinitionRecord, Error> {
        let def = self.readable_definition(definition_name(name)).await?;
        let reference = def.reference();
        if name.contains('@') && name != reference {
            return Err(Error::Denied(format!(
                "{name} is not the version in force: a level is raised for the version in force, which is {reference}"
            )));
        }
        let Some(domain) = self.platform_domain() else {
            return Err(Error::Denied(
                "the grants name no platform domain, so no ledger keeps a record of a task definition"
                    .into(),
            ));
        };
        let in_force = self.suite_version_in_force(&def.name).await?;
        let mut l = self.ledger.lock().await;
        let records = l.definition_records(&domain, Some(&reference)).await?;
        let Some(next) = level_after(&records).next() else {
            return Err(Error::Denied(format!(
                "{reference} is at the level checks already: no level is above it"
            )));
        };
        if def.autonomy < next {
            return Err(Error::Denied(format!(
                "the text of {reference} declares autonomy = \"{}\": that is the highest level that it can earn",
                def.autonomy
            )));
        }
        let signed_by = self.may_raise_to(next)?;
        let run = run_to_raise_on(&reference, &records, next, in_force.as_deref())?;
        let raised = DefinitionChange::Raised {
            to: next,
            run: run.id.clone(),
            signed_by,
        };
        Ok(l.record_definition(&domain, &reference, raised).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{as_author, held_out_root, verify_in, written, written_in, EVALUATOR};
    use crate::WriteRequest;
    use dm_core::Label;
    use dm_ledger::{Status, TaskSpec, WorkOutcome};
    use std::path::Path;
    use time::Duration;

    const OWNER: &str = "human:test:me";

    /// A definition whose text lets it earn the highest level.
    const TIDY: &str = "# Tidy a module\n\n```platform task tidy\npurpose = \"Tidy one module.\"\nacceptance = [\"The handoff names each file that changed.\"]\noutputs = [\"handoff\"]\nautonomy = \"checks\"\n```\n";

    /// Three scenarios. The pass marks: two for `review`, three for `checks`, and none
    /// for `person`.
    const SUITE: &str = "# The suite for tidy\n\n```platform suite tidy\n[pass]\nreview = 2\nchecks = 3\n\n[[scenario]]\nname = \"lease question\"\nask = \"Which decision set the lease length?\"\nexpect = [\"DECISION-12\", \"five minutes\"]\n\n[[scenario]]\nname = \"files question\"\nask = \"Which files changed?\"\nexpect = [\"src/importer\"]\n\n[[scenario]]\nname = \"bird question\"\nask = \"Name the bird.\"\nexpect = [\"BLUEBIRD-7\"]\n```\n";

    /// A root with the definition `tidy` and its suite, both verified.
    struct Fixture {
        owner: Memory,
        /// The version of `tidy` in force, as a task names it.
        reference: String,
        /// The entry of the suite.
        suite: String,
        /// Handoffs of `agent:a`: one that passes each scenario, in the order of the
        /// suite, and one that holds only the first string of the first scenario.
        passing: [String; 3],
        partial: String,
    }

    async fn fixture(dir: &Path) -> Fixture {
        let owner = held_out_root(dir);
        let entry = written(&owner, TIDY, true).await;
        let suite = written_in(&owner, "dom-held", SUITE, true).await;
        let by = "agent:a";
        Fixture {
            owner,
            reference: format!("tidy@{entry}"),
            suite,
            passing: [
                handoff(dir, by, "DECISION-12 set the lease to five minutes.\n").await,
                handoff(dir, by, "Changed src/importer.rs.\n").await,
                handoff(dir, by, "The bird is BLUEBIRD-7.\n").await,
            ],
            partial: handoff(dir, by, "DECISION-12 says so.\n").await,
        }
    }

    impl Fixture {
        /// Answers to the first `n` scenarios, each with the handoff that passes it.
        fn answers(&self, n: usize) -> Vec<Answer> {
            ["lease question", "files question", "bird question"]
                .iter()
                .zip(&self.passing)
                .take(n)
                .map(|(scenario, handoff)| answer(scenario, handoff))
                .collect()
        }
    }

    fn answer(scenario: &str, handoff: &str) -> Answer {
        Answer {
            scenario: scenario.into(),
            handoff: handoff.into(),
        }
    }

    /// A handoff entry in `dom-me`, written in a session of `author` of its own.
    async fn handoff(dir: &Path, author: &str, text: &str) -> String {
        let h = WriteRequest {
            body: text.into(),
            domain: Some("dom-me".into()),
            entry_type: EntryType::Handoff,
            ..WriteRequest::default()
        };
        as_author(dir, author).write(h).await.unwrap().id
    }

    fn evaluator(dir: &Path) -> Memory {
        as_author(dir, EVALUATOR)
    }

    /// A session with the owner's name that the memory server serves.
    fn served_owner(dir: &Path) -> Memory {
        let session = Memory::open(dir).unwrap();
        session.mark_served();
        session
    }

    async fn events(m: &Memory) -> u64 {
        m.ledger.lock().await.event_count().await.unwrap()
    }

    fn denied<T>(r: &Result<T, Error>, saying: &str) -> bool {
        matches!(r, Err(Error::Denied(why)) if why.contains(saying))
    }

    fn not_found<T>(r: &Result<T, Error>) -> bool {
        matches!(r, Err(Error::NotFound(_)))
    }

    /// A run of `n` passing answers for `level`, graded by the evaluator.
    async fn graded(dir: &Path, f: &Fixture, level: Autonomy, n: usize) -> Graded {
        let run = evaluator(dir).grade("tidy", level, &f.answers(n)).await;
        run.unwrap()
    }

    fn raised(to: Autonomy, run: &Graded, signed_by: Option<&str>) -> DefinitionChange {
        DefinitionChange::Raised {
            to,
            run: run.record.id.clone(),
            signed_by: signed_by.map(str::to_string),
        }
    }

    fn ids(hits: &[dm_index::Hit]) -> Vec<&str> {
        hits.iter().map(|h| h.id.as_str()).collect()
    }

    /// Rule 2: the sessions that do not read the held-out domain.
    #[tokio::test]
    async fn a_session_that_claims_tasks_reads_nothing_of_the_held_out_domain() {
        let dir = tempfile::tempdir().unwrap();
        let f = fixture(dir.path()).await;
        let worker = as_author(dir.path(), "agent:a");
        let person = as_author(dir.path(), "human:test:reviewer");
        let served = served_owner(dir.path());
        for session in [&worker, &person, &served] {
            let who = session.author();
            let got = session.get(&f.suite).await;
            assert!(not_found(&got), "{who}: {got:?}");
            let found = session.search("bluebird", None, 10).await.unwrap();
            assert_eq!(ids(&found), [f.passing[2].as_str()], "{who}");
            let listed = session.list(None, None, 100).await.unwrap();
            assert!(!ids(&listed).contains(&f.suite.as_str()), "{who}");
            // the domain by name is outside what the session reads
            assert!(session
                .search("bluebird", Some("dom-held"), 10)
                .await
                .is_err());
            assert!(session.list(Some("dom-held"), None, 10).await.is_err());
            let suite = session.held_out_suite("tidy").await;
            assert!(not_found(&suite), "{who}: {suite:?}");
            let none = session.held_out_suite("no-such").await;
            assert_eq!(
                format!("{suite:?}"),
                format!("{none:?}").replace("no-such", "tidy")
            );
            // what it read before, it reads still
            session.get(&f.passing[0]).await.unwrap();
        }
    }

    /// Rule 2: the sessions that read it, and a session that is served after its first
    /// read.
    #[tokio::test]
    async fn the_evaluator_and_the_owner_in_person_read_the_held_out_domain() {
        let dir = tempfile::tempdir().unwrap();
        let f = fixture(dir.path()).await;
        let owner = Memory::open(dir.path()).unwrap();
        for session in [&evaluator(dir.path()), &owner] {
            let who = session.author();
            assert!(session.reads_held_out(), "{who}");
            assert_eq!(session.get(&f.suite).await.unwrap().front.id, f.suite);
            let found = session.search("bluebird", Some("dom-held"), 10).await;
            assert_eq!(ids(&found.unwrap()), [f.suite.as_str()], "{who}");
            let listed = session.list(Some("dom-held"), None, 10).await.unwrap();
            assert_eq!(ids(&listed), [f.suite.as_str()], "{who}");
            let suite = session.held_out_suite("tidy").await.unwrap().unwrap();
            assert_eq!(suite.entry, f.suite, "{who}");
        }
        // the scope of a session is kept after its first read: the held-out domain
        // leaves it when the memory server starts to serve the session
        owner.mark_served();
        assert!(!owner.reads_held_out());
        let got = owner.get(&f.suite).await;
        assert!(not_found(&got), "{got:?}");
    }

    /// Rule 2: a filter that names no domain means every domain to the index, so a
    /// session whose one granted domain is the held-out one gets no such filter.
    #[tokio::test]
    async fn a_session_with_no_domain_but_the_held_out_one_reads_nothing() {
        let dir = tempfile::tempdir().unwrap();
        Memory::init(dir.path(), OWNER, "dom-held").unwrap();
        let path = dir.path().join(crate::GRANTS_FILE);
        let mut g = dm_policy::Grants::load(&path).unwrap();
        g.held_out = Some("dom-held".into());
        std::fs::write(&path, g.render().unwrap()).unwrap();
        let owner = Memory::open(dir.path()).unwrap();
        let id = written_in(&owner, "dom-held", SUITE, true).await;
        let worker = as_author(dir.path(), "agent:a");
        let listed = worker.list(None, None, 10).await.unwrap();
        assert_eq!(ids(&listed), [""; 0]);
        let found = worker.search("bluebird", None, 10).await.unwrap();
        assert_eq!(ids(&found), [""; 0]);
        let got = worker.get(&id).await;
        assert!(not_found(&got), "{got:?}");
        let listed = owner.list(None, None, 10).await.unwrap();
        assert_eq!(ids(&listed), [id.as_str()]);
    }

    /// Rule 3.
    #[tokio::test]
    async fn a_session_that_does_not_read_the_held_out_domain_writes_nothing_into_it() {
        let dir = tempfile::tempdir().unwrap();
        let f = fixture(dir.path()).await;
        let suite = |body: &str| WriteRequest {
            body: body.into(),
            domain: Some("dom-held".into()),
            entry_type: EntryType::Convention,
            ..WriteRequest::default()
        };
        let worker = as_author(dir.path(), "agent:a");
        let person = as_author(dir.path(), "human:test:reviewer");
        for session in [&worker, &person, &served_owner(dir.path())] {
            let refused = session.write(suite(SUITE)).await;
            assert!(
                denied(&refused, "writes only where it reads"),
                "{}: {refused:?}",
                session.author()
            );
        }
        let held = f.owner.store.list("dom-held").await.unwrap();
        assert_eq!(held, [f.suite.as_str()], "nothing was written");
        // the two that read it write into it, at the trust tier of every write
        for session in [&evaluator(dir.path()), &f.owner] {
            let id = session.write(suite("A note.\n")).await.unwrap().id;
            let e = f.owner.store.get("dom-held", &id).await.unwrap().unwrap();
            assert_eq!(e.front.trust_tier, TrustTier::Raw);
        }
    }

    /// The projection of a session is a read of the record: the held-out domain has no
    /// directory in a worker's.
    #[tokio::test]
    async fn the_projection_of_a_worker_has_no_directory_for_the_held_out_domain() {
        let dir = tempfile::tempdir().unwrap();
        fixture(dir.path()).await;
        let out = tempfile::tempdir().unwrap();
        let worker = as_author(dir.path(), "agent:a");
        let theirs = out.path().join("worker");
        worker.project_into(&theirs).await.unwrap();
        assert!(theirs.join("dom-me").is_dir());
        assert!(!theirs.join("dom-held").exists());
        let graders = out.path().join("evaluator");
        evaluator(dir.path()).project_into(&graders).await.unwrap();
        assert!(graders.join("dom-held").is_dir());
    }

    /// Rule 4.
    #[tokio::test]
    async fn a_suite_is_in_force_once_verified_and_the_newest_one_is() {
        let dir = tempfile::tempdir().unwrap();
        let owner = held_out_root(dir.path());
        let first = written_in(&owner, "dom-held", SUITE, false).await;
        assert_eq!(owner.held_out_suite("tidy").await.unwrap(), None);
        // a suite block outside the held-out domain is no suite, whatever its tier
        written_in(&owner, "dom-me", SUITE, true).await;
        written(&owner, SUITE, true).await;
        assert_eq!(owner.held_out_suite("tidy").await.unwrap(), None);
        verify_in(&owner, "dom-held", &first).await;
        let suite = owner.held_out_suite("tidy").await.unwrap().unwrap();
        let text = owner.store.get("dom-held", &first).await.unwrap().unwrap();
        let version = format!("{first}@{}", dm_core::digest(&text.render()));
        assert_eq!((&suite.entry, &suite.version), (&first, &version));
        assert_eq!((suite.pass.person, suite.pass.review), (None, Some(2)));
        assert_eq!(suite.scenarios.len(), 3);
        assert_eq!(suite.scenarios[0].expect, ["DECISION-12", "five minutes"]);
        let easier = SUITE.replace("review = 2", "review = 1");
        let second = written_in(&owner, "dom-held", &easier, true).await;
        let suite = owner.held_out_suite("tidy").await.unwrap().unwrap();
        assert_eq!((&suite.entry, suite.pass.review), (&second, Some(1)));
        // a newer suite that nobody verified replaces nothing
        written_in(&owner, "dom-held", SUITE, false).await;
        let suite = owner.held_out_suite("tidy").await.unwrap().unwrap();
        assert_eq!(suite.entry, second);
        assert_eq!(owner.held_out_suite("sweep").await.unwrap(), None);
        // the launcher reads no suite: its artifacts are the platform domain's kinds
        let loaded = owner.platform_artifact(SUITE_KIND, "tidy").await.unwrap();
        assert_eq!(loaded, None);
    }

    /// Each text that is no suite, with the mark `BLUEBIRD` in it.
    const NOT_A_SUITE: &[(&str, &str)] = &[
        ("no scenario", "# BLUEBIRD\n"),
        (
            "two scenarios of one name",
            "[[scenario]]\nname = \"BLUEBIRD\"\nexpect = [\"a\"]\n[[scenario]]\nname = \"BLUEBIRD\"\nexpect = [\"b\"]\n",
        ),
        ("a blank name", "[[scenario]]\nname = \" \"\nexpect = [\"BLUEBIRD\"]\n"),
        ("no expected string", "[[scenario]]\nname = \"BLUEBIRD\"\nexpect = []\n"),
        (
            "a blank expected string",
            "[[scenario]]\nname = \"one\"\nexpect = [\"BLUEBIRD\", \" \"]\n",
        ),
        (
            "a pass mark of 0",
            "[pass]\nreview = 0\n[[scenario]]\nname = \"one\"\nexpect = [\"BLUEBIRD\"]\n",
        ),
        (
            "a pass mark above the number of scenarios",
            "[pass]\nchecks = 2\n[[scenario]]\nname = \"one\"\nexpect = [\"BLUEBIRD\"]\n",
        ),
        (
            "a key that this code does not know",
            "bird = \"BLUEBIRD\"\n[[scenario]]\nname = \"one\"\nexpect = [\"a\"]\n",
        ),
    ];

    /// Rule 5.
    #[tokio::test]
    async fn a_block_that_is_no_suite_is_refused_and_its_text_is_not_quoted() {
        let dir = tempfile::tempdir().unwrap();
        let owner = held_out_root(dir.path());
        written(&owner, TIDY, true).await;
        let before = events(&owner).await;
        let said = |r: Result<String, Error>| match r {
            Err(Error::Denied(why)) => why,
            other => format!("it read: {other:?}"),
        };
        for (what, text) in NOT_A_SUITE {
            let body = format!("```platform suite tidy\n{text}```\n");
            let id = written_in(&owner, "dom-held", &body, true).await;
            let read = owner.held_out_suite("tidy").await;
            let why = said(read.map(|s| format!("{s:?}")));
            assert!(why.contains("does not read as one"), "{what}: {why}");
            assert!(!why.to_lowercase().contains("bluebird"), "{what}: {why}");
            let run = owner.grade("tidy", Autonomy::Review, &[]).await;
            assert_eq!(said(run.map(|g| format!("{g:?}"))), why, "{what}");
            owner.store.demote("dom-held", &id).await.unwrap();
        }
        assert_eq!(events(&owner).await, before, "no run was recorded");
    }

    /// Rule 6.
    #[tokio::test]
    async fn only_the_evaluator_and_the_owner_in_person_grade() {
        let dir = tempfile::tempdir().unwrap();
        let f = fixture(dir.path()).await;
        let answers = f.answers(3);
        let before = events(&f.owner).await;
        let worker = as_author(dir.path(), "agent:a");
        let person = as_author(dir.path(), "human:test:reviewer");
        let tap = as_author(dir.path(), "tap:checks");
        for session in [&worker, &person, &tap, &served_owner(dir.path())] {
            let refused = session.grade("tidy", Autonomy::Review, &answers).await;
            assert!(
                denied(&refused, "only the evaluator grades"),
                "{}: {refused:?}",
                session.author()
            );
        }
        assert_eq!(events(&f.owner).await, before, "nothing appended");
        for session in [&evaluator(dir.path()), &f.owner] {
            let run = session.grade("tidy", Autonomy::Review, &answers).await;
            assert!(run.unwrap().pass, "{}", session.author());
        }
        assert_eq!(events(&f.owner).await, before + 2);
    }

    /// Rule 7, what passes.
    #[tokio::test]
    async fn a_scenario_passes_only_when_its_handoff_holds_every_expected_string() {
        let dir = tempfile::tempdir().unwrap();
        let f = fixture(dir.path()).await;
        let grader = evaluator(dir.path());
        // the first handoff holds one of the two strings; the third has no answer
        let mut answers = f.answers(2);
        answers[0].handoff.clone_from(&f.partial);
        let run = grader.grade("tidy", Autonomy::Review, &answers).await;
        let run = run.unwrap();
        assert_eq!(run.failed, ["lease question", "bird question"]);
        assert!(!run.pass, "one of three, and review needs two");
        let run = graded(dir.path(), &f, Autonomy::Review, 2).await;
        assert_eq!(run.failed, ["bird question"]);
        assert!(run.pass);
        // the same answers do not reach the pass mark of the next level
        let run = graded(dir.path(), &f, Autonomy::Checks, 2).await;
        assert!(!run.pass);
        let run = graded(dir.path(), &f, Autonomy::Checks, 3).await;
        assert_eq!((run.pass, run.failed.len()), (true, 0));
        // a run moves no level
        assert_eq!(
            f.owner.level_of(&f.reference).await.unwrap(),
            Autonomy::Person
        );
    }

    /// Rule 7, what is refused.
    #[tokio::test]
    async fn an_answer_that_does_not_fit_the_suite_or_is_the_grader_s_own_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let f = fixture(dir.path()).await;
        let grader = evaluator(dir.path());
        let before = events(&f.owner).await;
        let own = handoff(dir.path(), EVALUATOR, "The bird is BLUEBIRD-7.\n").await;
        let a_fact = written_in(&f.owner, "dom-me", "The bird is BLUEBIRD-7.\n", false).await;
        let twice = [f.answers(1), f.answers(1)].concat();
        for (answers, why) in [
            (vec![answer("bird question", &own)], "grades its own work"),
            (vec![answer("bird question", &a_fact)], "is no handoff"),
            (vec![answer("no such", &f.partial)], "has no scenario"),
            (twice, "has two answers"),
        ] {
            let refused = grader.grade("tidy", Autonomy::Review, &answers).await;
            assert!(denied(&refused, why), "{why}: {refused:?}");
        }
        let unread = [answer("bird question", "mem-me-nothere")];
        let refused = grader.grade("tidy", Autonomy::Review, &unread).await;
        assert!(not_found(&refused), "{refused:?}");
        // the suite gives no pass mark for person
        let refused = grader.grade("tidy", Autonomy::Person, &f.answers(3)).await;
        assert!(denied(&refused, "gives no pass mark"), "{refused:?}");
        // a definition with no suite
        written(&f.owner, &TIDY.replace("tidy", "sweep"), true).await;
        let refused = grader.grade("sweep", Autonomy::Review, &[]).await;
        assert!(
            denied(&refused, "no held-out suite is in force"),
            "{refused:?}"
        );
        assert_eq!(events(&f.owner).await, before, "nothing appended");
    }

    /// Rule 8.
    #[tokio::test]
    async fn the_record_of_a_run_holds_the_counts_and_no_text_of_the_suite() {
        let dir = tempfile::tempdir().unwrap();
        let f = fixture(dir.path()).await;
        let run = graded(dir.path(), &f, Autonomy::Review, 2).await;
        let suite = f.owner.held_out_suite("tidy").await.unwrap().unwrap();
        let counted = DefinitionChange::Graded {
            suite: suite.version,
            level: Autonomy::Review,
            passed: 2,
            of: 3,
            needed: 2,
            by: EVALUATOR.into(),
        };
        let record = &run.record;
        // the ledger holds it, and a session that reads the definition reads it
        let worker = as_author(dir.path(), "agent:a");
        let view = worker.autonomy("tidy").await.unwrap();
        assert_eq!(view.records.as_slice(), std::slice::from_ref(record));
        let kept = serde_json::to_string(&view.records).unwrap();
        for text in [
            "question",
            "Which",
            "bird",
            "DECISION",
            "five minutes",
            "src/importer",
            "BLUEBIRD",
        ] {
            assert!(!kept.contains(text), "{text} is in {kept}");
        }
        assert_eq!(
            (record.domain.as_str(), &record.definition, &record.change),
            ("dom-plat", &f.reference, &counted)
        );
    }

    /// Rule 9.
    #[tokio::test]
    async fn a_passing_run_on_the_suite_in_force_raises_the_level_one_step() {
        let dir = tempfile::tempdir().unwrap();
        let f = fixture(dir.path()).await;
        // a run for each of the two levels above, before any raise
        let for_checks = graded(dir.path(), &f, Autonomy::Checks, 3).await;
        let for_review = graded(dir.path(), &f, Autonomy::Review, 2).await;
        let record = f.owner.raise("tidy").await.unwrap();
        assert_eq!(record.definition, f.reference);
        assert_eq!(record.change, raised(Autonomy::Review, &for_review, None));
        assert_eq!(
            f.owner.level_of(&f.reference).await.unwrap(),
            Autonomy::Review
        );
        let view = f.owner.autonomy("tidy").await.unwrap();
        assert_eq!((view.level, view.records.len()), (Autonomy::Review, 3));
        assert_eq!(view.records[2], record);
        // the next step stands on the run for its own level, by the reference too
        let record = f.owner.raise(&f.reference).await.unwrap();
        assert_eq!(
            record.change,
            raised(Autonomy::Checks, &for_checks, Some(OWNER))
        );
        assert_eq!(
            f.owner.level_of(&f.reference).await.unwrap(),
            Autonomy::Checks
        );
    }

    /// Rule 10: no run for the next level, and a run that failed.
    #[tokio::test]
    async fn a_raise_needs_a_run_for_the_next_level_that_passed() {
        let dir = tempfile::tempdir().unwrap();
        let f = fixture(dir.path()).await;
        let refused = f.owner.raise("tidy").await;
        assert!(denied(&refused, "no run of tidy@"), "{refused:?}");
        // a run for another level is no run for this one
        graded(dir.path(), &f, Autonomy::Checks, 3).await;
        let refused = f.owner.raise("tidy").await;
        assert!(
            denied(&refused, "was graded for the level review"),
            "{refused:?}"
        );
        // the newest run decides: a run that failed after one that passed
        graded(dir.path(), &f, Autonomy::Review, 2).await;
        graded(dir.path(), &f, Autonomy::Review, 1).await;
        let before = events(&f.owner).await;
        let refused = f.owner.raise("tidy").await;
        assert!(
            denied(&refused, "failed: 1 of 3 scenarios passed"),
            "{refused:?}"
        );
        assert_eq!(events(&f.owner).await, before, "nothing appended");
        assert_eq!(
            f.owner.level_of(&f.reference).await.unwrap(),
            Autonomy::Person
        );
    }

    /// Rule 10: a run of an older version of the suite, and a run older than an outcome.
    #[tokio::test]
    async fn a_raise_needs_a_run_of_the_suite_in_force_that_is_newer_than_each_outcome() {
        let dir = tempfile::tempdir().unwrap();
        let f = fixture(dir.path()).await;
        graded(dir.path(), &f, Autonomy::Review, 2).await;
        // a person verifies a changed suite: the run is of the version before it
        let harder = SUITE.replace("BLUEBIRD-7", "BLUEBIRD-8");
        written_in(&f.owner, "dom-held", &harder, true).await;
        let refused = f.owner.raise("tidy").await;
        assert!(
            denied(&refused, "is not of the held-out suite in force now"),
            "{refused:?}"
        );
        // the same answers pass on the suite in force, and then work is reverted
        graded(dir.path(), &f, Autonomy::Review, 2).await;
        let reverted = DefinitionChange::Outcome {
            task: "task-me-1".into(),
            outcome: WorkOutcome::Reverted,
            by: OWNER.into(),
            to: Autonomy::Person,
        };
        let mut l = f.owner.ledger.lock().await;
        l.record_definition("dom-plat", &f.reference, reverted)
            .await
            .unwrap();
        drop(l);
        let before = events(&f.owner).await;
        let refused = f.owner.raise("tidy").await;
        assert!(
            denied(&refused, "a work outcome was recorded after"),
            "{refused:?}"
        );
        assert_eq!(events(&f.owner).await, before, "nothing appended");
        // a run after the outcome earns the level
        let again = graded(dir.path(), &f, Autonomy::Review, 2).await;
        let record = f.owner.raise("tidy").await.unwrap();
        assert_eq!(record.change, raised(Autonomy::Review, &again, None));
    }

    /// Rule 10: the level that the text declares is the highest, and `checks` is the top.
    #[tokio::test]
    async fn a_level_goes_no_higher_than_the_text_declares_and_not_above_checks() {
        let dir = tempfile::tempdir().unwrap();
        let owner = held_out_root(dir.path());
        let capped = TIDY.replace("autonomy = \"checks\"", "autonomy = \"review\"");
        written(&owner, &capped, true).await;
        written_in(&owner, "dom-held", SUITE, true).await;
        let by = "agent:a";
        let all = handoff(
            dir.path(),
            by,
            "DECISION-12: five minutes. src/importer. BLUEBIRD-7.\n",
        )
        .await;
        let answers: Vec<Answer> = ["lease question", "files question", "bird question"]
            .iter()
            .map(|s| answer(s, &all))
            .collect();
        for level in [Autonomy::Review, Autonomy::Checks] {
            let run = owner.grade("tidy", level, &answers).await.unwrap();
            assert!(run.pass);
        }
        owner.raise("tidy").await.unwrap();
        let before = events(&owner).await;
        let refused = owner.raise("tidy").await;
        assert!(
            denied(&refused, "declares autonomy = \"review\""),
            "{refused:?}"
        );
        // a text that declares `checks`, at `checks`
        written(&owner, TIDY, true).await;
        for level in [Autonomy::Review, Autonomy::Checks] {
            owner.grade("tidy", level, &answers).await.unwrap();
        }
        owner.raise("tidy").await.unwrap();
        owner.raise("tidy").await.unwrap();
        let top = events(&owner).await;
        assert_eq!(top, before + 4, "the new version: two runs and two raises");
        let refused = owner.raise("tidy").await;
        assert!(
            denied(&refused, "is at the level checks already"),
            "{refused:?}"
        );
        assert_eq!(events(&owner).await, top, "nothing appended");
    }

    /// Rule 11.
    #[tokio::test]
    async fn the_step_to_checks_is_a_person_s_sign_off_and_the_step_to_review_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let f = fixture(dir.path()).await;
        let for_review = graded(dir.path(), &f, Autonomy::Review, 2).await;
        let for_checks = graded(dir.path(), &f, Autonomy::Checks, 3).await;
        let worker = as_author(dir.path(), "agent:a");
        let grader = evaluator(dir.path());
        let before = events(&f.owner).await;
        for session in [&worker, &served_owner(dir.path())] {
            let refused = session.raise("tidy").await;
            assert!(
                denied(&refused, "only a person or the evaluator raises"),
                "{}: {refused:?}",
                session.author()
            );
        }
        assert_eq!(events(&f.owner).await, before, "nothing appended");
        let record = grader.raise("tidy").await.unwrap();
        assert_eq!(record.change, raised(Autonomy::Review, &for_review, None));
        let before = events(&f.owner).await;
        for session in [&grader, &worker, &served_owner(dir.path())] {
            let refused = session.raise("tidy").await;
            assert!(
                denied(&refused, "only a person signs a task definition off"),
                "{}: {refused:?}",
                session.author()
            );
        }
        assert_eq!(events(&f.owner).await, before, "nothing appended");
        let record = Memory::open(dir.path()).unwrap().raise("tidy").await;
        assert_eq!(
            record.unwrap().change,
            raised(Autonomy::Checks, &for_checks, Some(OWNER))
        );
    }

    /// Rule 12, and a candidate version that is graded before a person verifies it.
    #[tokio::test]
    async fn a_new_version_of_a_definition_starts_at_person() {
        let dir = tempfile::tempdir().unwrap();
        let f = fixture(dir.path()).await;
        graded(dir.path(), &f, Autonomy::Review, 2).await;
        f.owner.raise("tidy").await.unwrap();
        let changed = TIDY.replace("Tidy one module.", "Tidy one module and its tests.");
        let candidate = written(&f.owner, &changed, false).await;
        let next = format!("tidy@{candidate}");
        // the candidate is graded while the first version is in force
        let run = evaluator(dir.path())
            .grade(&next, Autonomy::Review, &f.answers(2))
            .await
            .unwrap();
        assert_eq!((&run.record.definition, run.pass), (&next, true));
        let refused = f.owner.raise(&next).await;
        assert!(
            denied(&refused, "is not the version in force"),
            "{refused:?}"
        );
        crate::testing::verify(&f.owner, &candidate).await;
        let view = f.owner.autonomy("tidy").await.unwrap();
        assert_eq!(
            (view.definition.as_str(), view.level, view.records.len()),
            (next.as_str(), Autonomy::Person, 1)
        );
        // what the first version earned is that version's
        assert_eq!(
            f.owner.level_of(&f.reference).await.unwrap(),
            Autonomy::Review
        );
        let refused = f.owner.raise(&f.reference).await;
        assert!(
            denied(&refused, "is not the version in force"),
            "{refused:?}"
        );
        // and the new version earns its own level on its own run
        let record = f.owner.raise("tidy").await.unwrap();
        assert_eq!(
            (&record.definition, &record.change),
            (&next, &raised(Autonomy::Review, &run, None))
        );
    }

    /// A version that is not active is not graded, and neither is a name with no entry.
    #[tokio::test]
    async fn only_an_active_entry_of_the_platform_domain_is_graded() {
        let dir = tempfile::tempdir().unwrap();
        let f = fixture(dir.path()).await;
        let grader = evaluator(dir.path());
        let entry = f.reference.trim_start_matches("tidy@");
        let elsewhere = written_in(&f.owner, "dom-me", TIDY, true).await;
        let no_block = written(&f.owner, "A note.\n", true).await;
        f.owner.store.demote("dom-plat", entry).await.unwrap();
        let before = events(&f.owner).await;
        for version in [entry, elsewhere.as_str(), no_block.as_str(), "nowhere", ""] {
            let named = format!("tidy@{version}");
            let refused = grader.grade(&named, Autonomy::Review, &f.answers(2)).await;
            assert!(
                denied(&refused, "of a task definition is there to grade"),
                "{named}: {refused:?}"
            );
        }
        let refused = grader.grade("tidy", Autonomy::Review, &f.answers(2)).await;
        assert!(
            denied(&refused, "no task definition tidy is in force"),
            "{refused:?}"
        );
        assert_eq!(events(&f.owner).await, before, "nothing appended");
    }

    /// The grader reads the definition and the suite under its own filter, and its
    /// label rises to what it read.
    #[tokio::test]
    async fn a_grader_reads_the_definition_and_the_suite_under_its_filter() {
        let dir = tempfile::tempdir().unwrap();
        let owner = held_out_root(dir.path());
        let restricted = |domain: &str, body: &str| WriteRequest {
            body: body.into(),
            domain: Some(domain.into()),
            entry_type: EntryType::Convention,
            classification: Some(Label::Restricted),
            ..WriteRequest::default()
        };
        let entry = written(&owner, TIDY, true).await;
        let high = owner.write(restricted("dom-plat", TIDY)).await.unwrap().id;
        let suite = owner.write(restricted("dom-held", SUITE)).await.unwrap().id;
        verify_in(&owner, "dom-held", &suite).await;
        let low = evaluator(dir.path()).with_handling_cap(Label::Internal);
        assert_eq!(low.held_out_suite("tidy").await.unwrap(), None);
        let refused = low.grade("tidy", Autonomy::Review, &[]).await;
        assert!(
            denied(&refused, "no held-out suite is in force"),
            "{refused:?}"
        );
        let words = |r: Result<Graded, Error>| match r {
            Err(Error::Denied(why)) => why.replace("nowhere", &high),
            other => format!("{other:?}"),
        };
        let hidden = low
            .grade(&format!("tidy@{high}"), Autonomy::Review, &[])
            .await;
        let missing = low.grade("tidy@nowhere", Autonomy::Review, &[]).await;
        assert_eq!(words(hidden), words(missing));
        assert_eq!(low.label(), Label::Internal, "the definition {entry}");
        let grader = evaluator(dir.path());
        assert_eq!(grader.label(), Label::Public);
        grader.held_out_suite("tidy").await.unwrap().unwrap();
        assert_eq!(grader.label(), Label::Restricted, "it read the suite");
    }

    /// Rule 13.
    #[tokio::test]
    async fn the_evaluator_claims_no_task() {
        let dir = tempfile::tempdir().unwrap();
        let owner = held_out_root(dir.path());
        let spec = TaskSpec {
            domain: "dom-me".into(),
            title: "Sweep the floor".into(),
            body: "Do it.\n".into(),
            ..TaskSpec::default()
        };
        let mut task = owner.create_task(spec).await.unwrap().task;
        if task.status == Status::Proposed {
            task = owner.admit(&task.id).await.unwrap();
        }
        let lease = Duration::minutes(5);
        let grader = evaluator(dir.path());
        let before = events(&owner).await;
        let named = grader.claim(&task.id, lease).await;
        assert!(denied(&named, "claims none"), "{named:?}");
        let next = grader.claim_next(Some("dom-me"), lease).await;
        assert!(denied(&next, "claims none"), "{next:?}");
        assert_eq!(events(&owner).await, before, "nothing appended");
        // the task is still on offer to a session that does work
        let worker = as_author(dir.path(), "agent:a");
        let claimed = worker.claim_next(Some("dom-me"), lease).await.unwrap();
        assert_eq!(claimed.unwrap().task.id, task.id);
    }
}
