//! Evidence for the horizon scanner, on a schedule.
//!
//! The horizon scanner of a unit is a person: the registry names that person
//! ([`dm_policy::registry::Unit::horizon_scanner`]). Agents can gather evidence, and the
//! person decides. For each period of the unit's cadence, [`Memory::schedule_horizon`]
//! files two tasks: an evidence task that an agent can claim, and a review task that
//! requires a person and waits for the evidence task.

use crate::{Error, Memory};
use dm_core::Label;
use dm_ledger::TaskSpec;
use serde::Serialize;
use time::Date;

/// The origin of the task that gathers evidence for a horizon scanner.
pub const EVIDENCE_ORIGIN: &str = "horizon:evidence";
/// The origin of the task in which the horizon scanner reviews the evidence.
pub const REVIEW_ORIGIN: &str = "horizon:review";

/// The two tasks of one unit for one period.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Scheduled {
    /// The key of the unit in the registry.
    pub unit: String,
    /// The horizon scanner of the unit.
    pub scanner: String,
    /// The first day of the period.
    pub period_start: Date,
    /// The id of the evidence task.
    pub evidence: String,
    /// The id of the review task.
    pub review: String,
    /// Whether this call filed the tasks. False when the period had them already.
    pub filed: bool,
}

/// The Julian day of 1970-01-01: the periods count from that day.
const FIRST_DAY: i64 = 2_440_588;

/// The first day of the period of `cadence_days` days that holds `today`.
fn period_start(today: Date, cadence_days: u32) -> Date {
    let cadence = i64::from(cadence_days.max(1));
    let days = i64::from(today.to_julian_day()) - FIRST_DAY;
    let start = FIRST_DAY + days.div_euclid(cadence) * cadence;
    i32::try_from(start)
        .ok()
        .and_then(|day| Date::from_julian_day(day).ok())
        .unwrap_or(today)
}

fn evidence_task(domain: &str, unit: &str, scanner: &str, start: Date) -> TaskSpec {
    TaskSpec {
        domain: domain.to_string(),
        title: format!("Horizon: gather evidence for the unit {unit}, period from {start}"),
        body: format!(
            "Gather evidence on what changes outside the fleet for the unit `{unit}`: its \
             users, its partner projects, the new tools, and the obligations of the unit. \
             Write each finding as an entry that cites its source. Propose no change to the \
             work or to the strategy. The horizon scanner `{scanner}` reads the evidence and \
             decides."
        ),
        origin: Some(EVIDENCE_ORIGIN.into()),
        upstream_event: Some(format!("{unit}:{start}")),
        classification: Label::Internal,
        priority: 1,
        ..TaskSpec::default()
    }
}

fn review_task(domain: &str, unit: &str, scanner: &str, start: Date, evidence: &str) -> TaskSpec {
    TaskSpec {
        domain: domain.to_string(),
        title: format!("Horizon: review the evidence for the unit {unit}, period from {start}"),
        body: format!(
            "`{scanner}` is the horizon scanner of the unit `{unit}`. Read the evidence that \
             the task `{evidence}` gathered for the period from {start}. Then propose to \
             start work, to change work or to stop work. This task is for that person: an \
             agent does not decide it."
        ),
        requirements: vec!["human".into()],
        depends_on: vec![evidence.to_string()],
        origin: Some(REVIEW_ORIGIN.into()),
        upstream_event: Some(format!("{unit}:{start}")),
        classification: Label::Internal,
        priority: 1,
        ..TaskSpec::default()
    }
}

impl Memory {
    /// File the evidence task and the review task of each unit for the period that
    /// holds `today`. A unit gets them when the registry names its horizon scanner and
    /// the grants of this session hold one of its domains: the tasks are in the first
    /// such domain. The tasks of one period are filed once. A later call in the same
    /// period files nothing and returns the same ids with `filed` false.
    ///
    /// With no registry, or no unit with a horizon scanner, the result is empty.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for a session that is not the session of a person;
    /// [`Error::Stale`] when the grants or the registry changed; else as
    /// [`Memory::file_own`].
    pub async fn schedule_horizon(&self, today: Date) -> Result<Vec<Scheduled>, Error> {
        self.fresh()?;
        if !self.of_a_person() {
            return Err(Error::Denied(
                "only a person schedules the evidence for a horizon scanner".into(),
            ));
        }
        let Some(registry) = self.registry() else {
            return Ok(Vec::new());
        };
        let granted = self.domains();
        let mut out = Vec::new();
        for (key, unit) in &registry.units {
            let Some(scanner) = &unit.horizon_scanner else {
                continue;
            };
            let Some(domain) = unit.domains.iter().find(|d| granted.contains(d)) else {
                continue;
            };
            let start = period_start(today, unit.horizon_cadence_days());
            let evidence = self
                .file_own(evidence_task(domain, key, scanner, start))
                .await?;
            let review = self
                .file_own(review_task(domain, key, scanner, start, &evidence.task.id))
                .await?;
            out.push(Scheduled {
                unit: key.clone(),
                scanner: scanner.clone(),
                period_start: start,
                evidence: evidence.task.id,
                review: review.task.id,
                filed: !(evidence.deduplicated && review.deduplicated),
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{WriteRequest, REGISTRY_FILE};
    use dm_core::EntryType;
    use dm_ledger::Status;
    use std::path::Path;
    use time::{Duration, Month};

    const OWNER: &str = "human:test,2026-11-01:me";
    const SCANNER: &str = "human:test,2026-11-01:scout";
    const DOMAIN: &str = "dom-me";

    /// A root with the unit `u1`, which names a horizon scanner and a cadence of 30
    /// days, and the unit `u2`, which names none.
    fn with_units(dir: &Path) -> Memory {
        Memory::init(dir, OWNER, DOMAIN).unwrap();
        let registry = format!(
            "[units.u1]\ndomains = [\"other\", \"{DOMAIN}\"]\nhorizon_scanner = \"{SCANNER}\"\n\
             horizon_cadence_days = 30\n\n[units.u2]\ndomains = [\"{DOMAIN}\"]\n"
        );
        std::fs::write(dir.join(REGISTRY_FILE), registry).unwrap();
        Memory::open(dir).unwrap()
    }

    fn today() -> Date {
        Date::from_calendar_date(2026, Month::November, 24).unwrap()
    }

    #[tokio::test]
    async fn a_unit_with_a_scanner_gets_an_evidence_task_and_a_review_task() {
        let dir = tempfile::tempdir().unwrap();
        let m = with_units(dir.path());
        let scheduled = m.schedule_horizon(today()).await.unwrap();
        // the unit with no scanner gets none
        assert_eq!(scheduled.len(), 1);
        let s = &scheduled[0];
        assert_eq!((s.unit.as_str(), s.scanner.as_str()), ("u1", SCANNER));
        assert!(s.filed);
        // the period holds today, and the periods count from 1970-01-01
        assert!(s.period_start <= today() && today() < s.period_start + Duration::days(30));
        assert_eq!(
            (i64::from(s.period_start.to_julian_day()) - FIRST_DAY) % 30,
            0
        );
        let event = Some(format!("u1:{}", s.period_start));
        let evidence = m.get_task(&s.evidence).await.unwrap();
        assert_eq!(evidence.spec.origin.as_deref(), Some(EVIDENCE_ORIGIN));
        assert_eq!(evidence.spec.upstream_event, event);
        assert_eq!(evidence.spec.domain, DOMAIN);
        // an agent may claim the evidence task
        assert_eq!(evidence.spec.requirements, Vec::<String>::new());
        assert_eq!(evidence.status, Status::Open);
        assert!(evidence.spec.body.contains(SCANNER));
        let review = m.get_task(&s.review).await.unwrap();
        assert_eq!(review.spec.origin.as_deref(), Some(REVIEW_ORIGIN));
        assert_eq!(review.spec.upstream_event, event);
        assert_eq!(review.spec.requirements, vec!["human".to_string()]);
        assert_eq!(review.spec.depends_on, vec![s.evidence.clone()]);
        assert_eq!(review.status, Status::Blocked);
    }

    #[tokio::test]
    async fn the_tasks_of_a_period_are_filed_once_and_a_new_period_gets_new_tasks() {
        let dir = tempfile::tempdir().unwrap();
        let m = with_units(dir.path());
        let first = m.schedule_horizon(today()).await.unwrap();
        let again = m.schedule_horizon(today()).await.unwrap();
        assert!(!again[0].filed);
        assert_eq!(
            (&again[0].evidence, &again[0].review),
            (&first[0].evidence, &first[0].review)
        );
        let next = m
            .schedule_horizon(today() + Duration::days(30))
            .await
            .unwrap();
        assert!(next[0].filed);
        assert_eq!(
            next[0].period_start,
            first[0].period_start + Duration::days(30)
        );
        assert_ne!(next[0].evidence, first[0].evidence);
        assert_ne!(next[0].review, first[0].review);
    }

    #[tokio::test]
    async fn the_review_task_waits_for_the_evidence_and_for_a_person() {
        let dir = tempfile::tempdir().unwrap();
        let m = with_units(dir.path());
        let s = m.schedule_horizon(today()).await.unwrap().remove(0);
        let lease = Duration::minutes(10);
        let person = Memory::open(dir.path())
            .unwrap()
            .with_manifest(vec!["human".into()]);
        // a person who asks for work does not get the review task before the evidence
        let got = person.claim_next(None, lease).await.unwrap().unwrap();
        assert_eq!(got.task.id, s.evidence);
        assert!(person.claim_next(None, lease).await.unwrap().is_none());
        assert!(person.claim(&s.review, lease).await.is_err());
        let handoff = person
            .write(WriteRequest {
                entry_type: EntryType::Handoff,
                body: "# Evidence\n\nTwo partner projects changed their release dates.\n".into(),
                ..WriteRequest::default()
            })
            .await
            .unwrap();
        person
            .complete(&s.evidence, None, &handoff.id)
            .await
            .unwrap();
        // the evidence is done: an agent does not get the review task, and a person does
        let agent = Memory::open(dir.path()).unwrap().with_author("agent:x");
        assert!(agent.claim_next(None, lease).await.unwrap().is_none());
        let got = person.claim_next(None, lease).await.unwrap().unwrap();
        assert_eq!(got.task.id, s.review);
    }

    #[tokio::test]
    async fn an_agent_schedules_nothing_and_a_root_with_no_registry_gets_nothing() {
        let dir = tempfile::tempdir().unwrap();
        with_units(dir.path());
        let agent = Memory::open(dir.path()).unwrap().with_author("agent:x");
        let refused = agent.schedule_horizon(today()).await.unwrap_err();
        assert!(matches!(refused, Error::Denied(_)), "{refused}");
        assert_eq!(agent.tasks(None, Status::Open).await.unwrap(), Vec::new());

        let bare = tempfile::tempdir().unwrap();
        let m = Memory::init(bare.path(), OWNER, DOMAIN).unwrap();
        assert_eq!(m.schedule_horizon(today()).await.unwrap(), Vec::new());
    }
}
