//! What the ledger keeps of a task definition: the autonomy level that one version of it
//! has earned, as records on the log.
//!
//! A record is one event under the key `definition:<name>@<entry>`, projected nowhere:
//! the events are the store, as they are for an approval. It is no task. The level of a
//! version is what its records say, read oldest first, so the text of the definition's
//! entry cannot give a level that nobody earned.

use crate::{Backend, Error, Event, Ledger};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// What a completion of a task needs before it counts. The order is the order in which
/// a definition earns them.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Autonomy {
    /// A person's verdict. Where every version of a definition starts.
    #[default]
    Person,
    /// A verdict from another principal than the one that handed the work in.
    Review,
    /// The acceptance checks alone.
    Checks,
}

impl Autonomy {
    /// The level above this one, when there is one.
    #[must_use]
    pub fn next(self) -> Option<Autonomy> {
        match self {
            Autonomy::Person => Some(Autonomy::Review),
            Autonomy::Review => Some(Autonomy::Checks),
            Autonomy::Checks => None,
        }
    }

    /// The level one step below this one. The lowest level stays where it is.
    #[must_use]
    pub fn lower(self) -> Autonomy {
        match self {
            Autonomy::Person | Autonomy::Review => Autonomy::Person,
            Autonomy::Checks => Autonomy::Review,
        }
    }

    /// The level as the ledger stores it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Autonomy::Person => "person",
            Autonomy::Review => "review",
            Autonomy::Checks => "checks",
        }
    }
}

impl std::fmt::Display for Autonomy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Autonomy {
    type Err = String;

    fn from_str(s: &str) -> Result<Autonomy, String> {
        match s {
            "person" => Ok(Autonomy::Person),
            "review" => Ok(Autonomy::Review),
            "checks" => Ok(Autonomy::Checks),
            other => Err(format!("{other:?} is not person, review or checks")),
        }
    }
}

/// What became of work that was handed in, when it did not stand. Not the outcome of an
/// external effect: that is [`crate::Outcome`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkOutcome {
    /// The work was accepted and later undone.
    Reverted,
    /// The work was not accepted.
    Rejected,
}

impl WorkOutcome {
    /// The outcome as the ledger stores it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            WorkOutcome::Reverted => "reverted",
            WorkOutcome::Rejected => "rejected",
        }
    }
}

impl std::str::FromStr for WorkOutcome {
    type Err = String;

    fn from_str(s: &str) -> Result<WorkOutcome, String> {
        match s {
            "reverted" => Ok(WorkOutcome::Reverted),
            "rejected" => Ok(WorkOutcome::Rejected),
            other => Err(format!("{other:?} is not reverted or rejected")),
        }
    }
}

/// One change to what a version of a task definition has earned. No variant holds free
/// text, so an erasure has nothing to remove from one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "change", rename_all = "lowercase")]
pub enum DefinitionChange {
    /// A run of a held-out suite was graded for a level.
    Graded {
        /// The version of the suite: its entry, and the digest of that entry's text.
        suite: String,
        /// The level the run was graded for.
        level: Autonomy,
        /// How many scenarios passed.
        passed: u32,
        /// How many scenarios the suite has.
        of: u32,
        /// How many must pass for the level.
        needed: u32,
        /// Who graded, system-qualified.
        by: String,
    },
    /// The level was raised on a graded run.
    Raised {
        /// The level from here on.
        to: Autonomy,
        /// The record of the graded run it was raised on.
        run: String,
        /// The person who signed the raise off, where the level needs one.
        signed_by: Option<String>,
    },
    /// Work of a task under the definition did not stand, and the level went down a step.
    Outcome {
        /// The task whose work it was.
        task: String,
        /// What became of the work.
        outcome: WorkOutcome,
        /// Who recorded it, system-qualified.
        by: String,
        /// The level from here on.
        to: Autonomy,
    },
}

/// A change to a definition's level as the ledger holds it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DefinitionRecord {
    /// The record's id, `def-<domain>-<suffix>`.
    pub id: String,
    /// The domain whose ledger keeps the record: the platform's.
    pub domain: String,
    /// The version of the definition, as a task names it: `<name>@<entry>`.
    pub definition: String,
    /// What changed.
    #[serde(flatten)]
    pub change: DefinitionChange,
    /// When it was recorded, on the ledger's clock.
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
}

/// The key a definition's records are appended under. A task id starts `task-`, so the
/// two never meet.
fn definition_key(reference: &str) -> String {
    format!("definition:{reference}")
}

impl Ledger {
    /// Record a change to what a version of a task definition has earned: one event,
    /// projected nowhere. The ledger gives it an id and the time of its clock. Nothing
    /// is checked and nothing is deduplicated: who may record which change is the
    /// caller's rule.
    ///
    /// # Errors
    ///
    /// A database error.
    pub async fn record_definition(
        &mut self,
        domain: &str,
        definition: &str,
        change: DefinitionChange,
    ) -> Result<DefinitionRecord, Error> {
        let record = DefinitionRecord {
            id: dm_core::id::new_task_id(domain).replacen("task-", "def-", 1),
            domain: domain.to_string(),
            definition: definition.to_string(),
            change,
            at: self.now(),
        };
        let key = definition_key(definition);
        let event = Event::Definition(Box::new(record.clone()));
        match &mut self.backend {
            Backend::Sqlite(db) => db.append(&key, record.at, &event)?,
            Backend::Postgres(db) => db.append_record(domain, &key, record.at, &event).await?,
        }
        Ok(record)
    }

    /// The definition records of a domain, oldest first: those of one version of one
    /// definition when `definition` names it as `<name>@<entry>`, else all of them.
    ///
    /// # Errors
    ///
    /// A database error, or an event the ledger did not write.
    pub async fn definition_records(
        &self,
        domain: &str,
        definition: Option<&str>,
    ) -> Result<Vec<DefinitionRecord>, Error> {
        let key = definition.map(definition_key);
        let payloads = self
            .record_payloads(domain, "definition", key.as_deref())
            .await?;
        let mut out = Vec::new();
        for p in payloads {
            if let Event::Definition(record) =
                Event::parse(key.as_deref().unwrap_or("definition"), &p)?
            {
                // SQLite keeps every domain's events in one table
                if record.domain == domain {
                    out.push(*record);
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_level_is_earned_one_step_at_a_time_and_lost_one_step_at_a_time() {
        use Autonomy::{Checks, Person, Review};
        assert!(Person < Review && Review < Checks);
        assert_eq!(Autonomy::default(), Person);
        assert_eq!(
            [Person.next(), Review.next(), Checks.next()],
            [Some(Review), Some(Checks), None]
        );
        assert_eq!(
            [Checks.lower(), Review.lower(), Person.lower()],
            [Review, Person, Person]
        );
    }

    #[test]
    fn a_level_and_an_outcome_read_as_they_are_stored() {
        for level in [Autonomy::Person, Autonomy::Review, Autonomy::Checks] {
            let text = serde_json::to_string(&level).unwrap();
            assert_eq!(text, format!("\"{level}\""));
            assert_eq!(serde_json::from_str::<Autonomy>(&text).unwrap(), level);
        }
        for outcome in [WorkOutcome::Reverted, WorkOutcome::Rejected] {
            let text = serde_json::to_string(&outcome).unwrap();
            assert_eq!(text, format!("\"{}\"", outcome.as_str()));
            assert_eq!(outcome.as_str().parse::<WorkOutcome>(), Ok(outcome));
        }
        assert!("applied".parse::<WorkOutcome>().is_err());
    }
}
