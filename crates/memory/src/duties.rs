//! The curator's and the auditor's duties on the command line and in the curation
//! cycle: demotion behind a tombstone, the convention checks, outcome events and
//! corrections, the curation of the policy, and the qualification and the checks of the
//! auditor. Each command prints what a function of `dm_server` answers. The one thing
//! this module decides is how a question goes to a model through the egress.

use crate::auditor::{self, Spend};
use crate::curation::Cycle;
use crate::{AuthArg, Credentials, Fallible, Prepared, Projected};
use clap::Subcommand;
use dm_egress::Egress;
use dm_ledger::OutcomeKind;
use dm_sandbox::{AuthMode, Harness};
use dm_server::audit::{self, Judged, Score};
use dm_server::duties::{CheckOptions, DemoteReason};
use dm_server::Memory;
use serde_json::json;
use std::path::Path;
use time::OffsetDateTime;

type Failed = Box<dyn std::error::Error>;

/// What `amem auditor` does.
#[derive(Subcommand)]
pub enum AuditorOp {
    /// How the auditor runs: the mode, whether a qualification is on file, and the
    /// number of verdicts that a person reviews in each cycle.
    Mode,
    /// Qualify the auditor: put each planted case to the model of the curators and to
    /// a candidate model, count what each caught, and keep the decision under the
    /// root. Without --model there is no candidate, and the mode is a fresh session.
    /// The owner's act.
    Qualify {
        /// The model that the curators run on.
        #[arg(long)]
        curator_model: String,
        /// The candidate: a model that is not the model of the curators.
        #[arg(long)]
        model: Option<String>,
        #[arg(long, value_enum, default_value = "gateway")]
        auth: AuthArg,
        #[arg(long)]
        provider: Option<String>,
    },
    /// One curated entry as a judge reads it: the entry, the version that it replaced
    /// and the entries that it cites, and whether it cites nothing that can be read.
    Case { id: String },
    /// Test entries together: the scan of the write gate over each ordered pair of the
    /// entries that one search returns. With --file, a task for a person for each pair.
    Together {
        #[arg(long)]
        domain: Option<String>,
        /// How many of the newest entries to search from.
        #[arg(long, default_value_t = 8)]
        sample: usize,
        #[arg(long)]
        file: bool,
    },
    /// The newest handoffs of a domain: the sample that the auditor checks for an
    /// instruction to the next agent.
    Handoffs {
        #[arg(long)]
        domain: Option<String>,
        #[arg(long, default_value_t = 8)]
        sample: usize,
    },
}

/// `amem demote`.
pub async fn demote(m: &Memory, id: &str, reason: &str) -> Fallible {
    let reason = DemoteReason::parse(reason).ok_or("--reason: dead or wrong")?;
    crate::json(&m.demote(id, reason).await?)
}

/// `amem restore`.
pub async fn restore(m: &Memory, id: &str) -> Fallible {
    m.restore(id).await?;
    crate::json(&json!({ "restored": id }))
}

/// `amem dead`.
pub async fn dead(m: &Memory, domain: Option<&str>, days: i64) -> Fallible {
    crate::json(&json!({ "dead": m.dead(domain, days).await? }))
}

/// `amem conventions`.
pub async fn conventions(
    m: &Memory,
    domain: Option<&str>,
    overdue_days: i64,
    file: bool,
) -> Fallible {
    let options = CheckOptions {
        overdue_days,
        ..CheckOptions::default()
    };
    let today = OffsetDateTime::now_utc().date();
    let failures = m.check_conventions(domain, today, &options).await?;
    let (filed, already) = if file {
        m.file_failures(domain, &failures).await?
    } else {
        (Vec::new(), 0)
    };
    crate::json(&json!({ "failures": failures, "filed": filed, "already_filed": already }))
}

/// `amem task outcome --kind`: an outcome event.
pub async fn record_outcome(m: &Memory, task: &str, kind: &str, note: &str) -> Fallible {
    let kind =
        OutcomeKind::parse(kind).ok_or("--kind: accepted, corrected, rejected or reverted")?;
    crate::json(&json!({ "outcomes": m.record_outcome_event(task, kind, note).await? }))
}

/// `amem task outcomes`.
pub async fn outcomes(m: &Memory, task: &str) -> Fallible {
    crate::json(&json!({ "outcomes": m.outcomes(task).await? }))
}

/// `amem corrections`.
pub async fn corrections(m: &Memory, domain: Option<&str>, file: bool) -> Fallible {
    let found = m.corrections(domain).await?;
    let (filed, already) = if file {
        m.file_corrections(domain, &found).await?
    } else {
        (Vec::new(), 0)
    };
    crate::json(&json!({ "corrections": found, "filed": filed, "already_filed": already }))
}

/// `amem policy curate`.
pub async fn policy_curate(m: &Memory, domain: Option<&str>, file: bool) -> Fallible {
    let found = m.policy_curation(domain).await?;
    let (filed, already) = if file {
        m.file_policy_curation(domain, &found).await?
    } else {
        (Vec::new(), 0)
    };
    crate::json(&json!({
        "overlaps": found.overlaps, "repeated": found.repeated,
        "filed": filed, "already_filed": already,
    }))
}

/// `amem auditor`.
pub async fn auditor_cmd(root: &Path, op: AuditorOp) -> Fallible {
    let (domain, sample, file) = match op {
        AuditorOp::Qualify {
            curator_model,
            model,
            auth,
            provider,
        } => return qualify(root, auth, provider, &curator_model, model.as_deref()).await,
        AuditorOp::Mode => return crate::json(&Memory::open(root)?.auditor_mode()?),
        AuditorOp::Case { id } => {
            return crate::json(&Memory::open(root)?.curation_case(&id).await?.to_json());
        }
        AuditorOp::Handoffs { domain, sample } => {
            let m = Memory::open(root)?;
            let sample = m.handoff_sample(domain.as_deref(), sample).await?;
            let rows: Vec<_> = sample
                .iter()
                .map(|e| json!({ "id": e.front.id, "title": e.title() }))
                .collect();
            return crate::json(&json!({ "handoffs": rows }));
        }
        AuditorOp::Together {
            domain,
            sample,
            file,
        } => (domain, sample, file),
    };
    let m = Memory::open(root)?;
    // the survey lists and searches many entries: these are not reads of a session
    m.disable_read_log();
    let found = m.test_together(domain.as_deref(), sample).await?;
    let filed = if file {
        m.file_together(domain.as_deref(), &found).await?
    } else {
        Vec::new()
    };
    crate::json(&json!({ "together": found, "filed": filed }))
}

/// A model that answers the questions of the auditor through the egress, inside the
/// token budget of a cycle when there is one.
pub struct Judge<'a> {
    pub egress: &'a Egress,
    pub destination: &'a str,
    pub model: &'a str,
    pub spend: Option<&'a Spend<'a>>,
}

impl Judge<'_> {
    /// Put one question to the model: whether its first line says `yes`, and the reason
    /// after it. `None` when the budget is spent, and the model is not called.
    async fn ask(&self, question: &str) -> Result<Option<(bool, String)>, Failed> {
        if self.spend.is_some_and(Spend::exhausted) {
            return Ok(None);
        }
        let (path, req) = crate::mine::request_with(self.destination, self.model, question);
        let answer = self.egress.call(self.destination, &path, &req).await?;
        let text = crate::mine::answer_text(self.destination, &answer);
        Ok(Some(auditor::verdict_of(&text)))
    }
}

/// The score of one model on the planted cases. A summary that the model does not
/// accept with `yes` is a summary that it judged faulty.
async fn score_of(egress: &Egress, destination: &str, model: &str) -> Result<Score, Failed> {
    let judge = Judge {
        egress,
        destination,
        model,
        spend: None,
    };
    let mut verdicts = Vec::new();
    for case in audit::planted() {
        let question = auditor::question(case.title, case.summary, case.body);
        if let Some((entails, _)) = judge.ask(&question).await? {
            verdicts.push((case.id.to_string(), !entails));
        }
    }
    Ok(audit::score(model, &verdicts)?)
}

/// `amem auditor qualify`: the planted cases put to each model, and the decision kept.
async fn qualify(
    root: &Path,
    auth: AuthArg,
    provider: Option<String>,
    curator_model: &str,
    candidate: Option<&str>,
) -> Fallible {
    let auth = AuthMode::from(auth);
    let provider = crate::auditor_destination(auth, provider);
    let p: Prepared = crate::prepare_launch(
        root,
        Harness::Command,
        Credentials::keyed(auth, &provider),
        None,
        None,
        Projected::No,
    )
    .await?;
    let curators = score_of(&p.egress, &p.provider, curator_model).await?;
    let candidate = match candidate {
        Some(model) => Some(score_of(&p.egress, &p.provider, model).await?),
        None => None,
    };
    let decided = audit::qualify(curators, candidate, OffsetDateTime::now_utc());
    // the session of the model calls is an agent's: the owner keeps the decision
    Memory::open(root)?.set_qualification(&decided)?;
    crate::json(&decided)
}

/// Whether an open task is work for a curator run, which the budget of a cycle counts.
/// A task for a person waits for that person, and does not take the room of the survey.
#[must_use]
pub fn curators_work(spec: &dm_ledger::TaskSpec) -> bool {
    let curation = spec.origin.as_deref().is_some_and(|o| {
        o.starts_with("curator:") || o.starts_with("auditor:") || o.starts_with("telemetry:")
    });
    curation && !spec.requirements.iter().any(|r| r == "human")
}

/// The owner's surveys of a cycle, each filed: the convention checks with the date of
/// today, the corrections, and the curation of the policy.
pub async fn file_surveys(m: &Memory, domain: &str, cycle: &mut Cycle) -> Result<(), Failed> {
    let domain = Some(domain);
    let today = OffsetDateTime::now_utc().date();
    let failures = m
        .check_conventions(domain, today, &CheckOptions::default())
        .await?;
    cycle.conventions = failures.len();
    m.file_failures(domain, &failures).await?;
    let corrections = m.corrections(domain).await?;
    cycle.corrections = corrections.len();
    m.file_corrections(domain, &corrections).await?;
    let found = m.policy_curation(domain).await?;
    cycle.policy_curation = found.overlaps.len() + found.repeated.len();
    m.file_policy_curation(domain, &found).await?;
    Ok(())
}

/// The auditor's checks of a cycle beyond the summaries: entries together, the newest
/// curations against their sources, and the newest handoffs. Each verdict of the model
/// goes into `judged`. Then the sample of `judged` for a person is filed.
pub async fn auditor_duties(
    judge: &Judge<'_>,
    m: &Memory,
    domain: &str,
    sample: usize,
    cycle: &mut Cycle,
    mut judged: Vec<Judged>,
) -> Result<(), Failed> {
    let together = m.test_together(Some(domain), sample).await?;
    cycle.together = together.len();
    m.file_together(Some(domain), &together).await?;
    judge_curations(judge, m, domain, sample, cycle, &mut judged).await?;
    judge_handoffs(judge, m, domain, sample, cycle, &mut judged).await?;
    let review = m.file_human_review(Some(domain), judge.model, &judged);
    cycle.human_review = review.await?.len();
    Ok(())
}

/// Each of the newest curations: a finding with no model when it cites nothing that a
/// judge reads, else the model's answer on the text of the case, and a finding on `no`.
async fn judge_curations(
    judge: &Judge<'_>,
    m: &Memory,
    domain: &str,
    sample: usize,
    cycle: &mut Cycle,
    judged: &mut Vec<Judged>,
) -> Result<(), Failed> {
    for id in m.curations(Some(domain), sample).await? {
        let case = m.curation_case(&id).await?;
        if case.unsupported() {
            m.file_curation_finding(Some(domain), &id, None, audit::UNSUPPORTED)
                .await?;
            cycle.curations_judged += 1;
            continue;
        }
        let Some((ok, why)) = judge.ask(&audit::curation_question(&case)).await? else {
            break;
        };
        cycle.curations_judged += 1;
        if !ok {
            m.file_curation_finding(Some(domain), &id, Some(judge.model), &why)
                .await?;
        }
        judged.push(Judged { entry: id, ok, why });
    }
    Ok(())
}

/// Each of the newest handoffs: the model's answer on whether it holds an instruction
/// for the next agent, and a finding on `yes`.
async fn judge_handoffs(
    judge: &Judge<'_>,
    m: &Memory,
    domain: &str,
    sample: usize,
    cycle: &mut Cycle,
    judged: &mut Vec<Judged>,
) -> Result<(), Failed> {
    for handoff in m.handoff_sample(Some(domain), sample).await? {
        let Some((found, why)) = judge.ask(&audit::handoff_question(&handoff)).await? else {
            break;
        };
        cycle.handoffs_sampled += 1;
        let id = handoff.front.id;
        if found {
            m.file_handoff_finding(Some(domain), &id, judge.model, &why)
                .await?;
        }
        judged.push(Judged {
            entry: id,
            ok: !found,
            why,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_budget_counts_work_for_a_curator_and_not_a_task_for_a_person() {
        let spec = |origin: Option<&str>, requirements: &[&str]| dm_ledger::TaskSpec {
            origin: origin.map(str::to_string),
            requirements: requirements.iter().map(|r| (*r).to_string()).collect(),
            ..dm_ledger::TaskSpec::default()
        };
        for origin in ["curator:verify", "auditor:summary", "telemetry:relay"] {
            assert!(curators_work(&spec(Some(origin), &[])), "{origin}");
            assert!(curators_work(&spec(Some(origin), &["rust"])), "{origin}");
            assert!(!curators_work(&spec(Some(origin), &["human"])), "{origin}");
        }
        assert!(!curators_work(&spec(Some("convention:ownership"), &[])));
        assert!(!curators_work(&spec(None, &[])));
    }
}
