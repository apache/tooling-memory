//! What the command line and the memory server share for the long timescales: a date
//! and an identifier read from text, the proposals as a list shows them, and a recovery
//! bundle and a restore drill that stay inside the root.
//!
//! The server of a session has access only inside its root. So a bundle that a tool
//! makes is a directory under [`RECOVERY_DIR`], named by its time, and a tool names a
//! bundle by that name and never by a path. The command line can name a path outside
//! the root, and it calls [`Memory::recovery_bundle`] and [`Memory::drill`] for that.

use crate::recovery::{recovery_dir, valid_name, Drill, RECOVERY_DIR};
use crate::{Error, Memory};
use dm_core::{identity, Entry};
use serde::Serialize;
use std::path::PathBuf;
use time::format_description::well_known::Rfc3339;
use time::macros::format_description;
use time::{Date, OffsetDateTime};

/// Read a calendar date written as `YYYY-MM-DD`.
///
/// # Errors
///
/// [`Error::Denied`] when the text is not such a date.
pub fn day(text: &str) -> Result<Date, Error> {
    let date = (text.len() == 10)
        .then(|| Date::parse(text, format_description!("[year]-[month]-[day]")).ok())
        .flatten();
    date.ok_or_else(|| Error::Denied(format!("{text:?} is not a date `YYYY-MM-DD`")))
}

/// An identifier of a person, read and followed through the assertions of the registry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Resolved {
    /// The identifier that was asked about.
    pub id: String,
    /// The identifier that names the same person now. It is `id` when no assertion of
    /// the registry names `id`.
    pub is: String,
    /// Who issued the identifier that names the person now.
    pub authority: String,
    /// The day on which that authority held its name, when the identifier says it.
    pub held: Option<String>,
    /// The identifier that the authority gave the person.
    pub user_id: String,
    /// Whether the identifier that names the person now is a dated identifier.
    pub dated: bool,
    /// Whether the root has a registry file. Without one, there is no assertion.
    pub registry: bool,
}

/// A proposal to a personal domain, as a list shows it: no body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Proposal {
    /// The id of the held entry.
    pub id: String,
    /// The personal domain.
    pub domain: String,
    /// Who wrote the proposal.
    pub author: String,
    /// When it was written.
    pub created: String,
    /// Its title, when it has one.
    pub title: Option<String>,
}

/// Each held proposal as a list shows it.
#[must_use]
pub fn proposal_lines(held: &[Entry]) -> Vec<Proposal> {
    held.iter()
        .map(|e| Proposal {
            id: e.front.id.clone(),
            domain: e.front.domain.clone(),
            author: e.front.author.clone(),
            created: e.front.created.format(&Rfc3339).unwrap_or_default(),
            title: e.front.title.clone(),
        })
        .collect()
}

/// A recovery bundle that a session made under its root.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct BundleMade {
    /// The name of the bundle: one directory under [`RECOVERY_DIR`].
    pub name: String,
    /// The path of the bundle, relative to the root.
    pub bundle: String,
    /// How many domains the bundle holds.
    pub domains: usize,
    /// How many files the manifest names.
    pub files: usize,
    /// How many events of the ledger the bundle holds.
    pub ledger_events: u64,
    /// The claim epoch of the ledger when the bundle was made.
    pub epoch: u64,
}

/// A restore drill on a bundle under the root, with the place of its scratch root.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DrillRun {
    /// The path of the bundle, relative to the root.
    pub bundle: String,
    /// The path of the scratch root that the drill restored into, relative to the root.
    pub scratch: String,
    /// What the drill found.
    #[serde(flatten)]
    pub drill: Drill,
}

/// A name under [`RECOVERY_DIR`] that no directory has: `prefix`, the time, and a
/// number when two are made in one second.
fn new_name(memory: &Memory, prefix: &str) -> String {
    let stamp = OffsetDateTime::now_utc()
        .format(format_description!(
            "[year][month][day]T[hour][minute][second]Z"
        ))
        .unwrap_or_default();
    let base = format!("{prefix}{stamp}");
    let mut name = base.clone();
    let mut n = 1;
    while recovery_dir(memory.root(), &name).exists() {
        n += 1;
        name = format!("{base}-{n}");
    }
    name
}

fn below_root(name: &str) -> String {
    format!("{RECOVERY_DIR}/{name}")
}

impl Memory {
    /// Read the identifier of a person, and follow the assertions of the registry to the
    /// identifier that names the same person now. Nothing of the record is read.
    ///
    /// # Errors
    ///
    /// [`Error::Stale`] when the grants or the registry changed after this session
    /// started; [`Error::Denied`] when the text, or the identifier it resolves to, does
    /// not name a person.
    pub fn resolve_identity(&self, id: &str) -> Result<Resolved, Error> {
        self.fresh()?;
        let not_a_person = |e: dm_core::Error| Error::Denied(e.to_string());
        identity::parse(id).map_err(not_a_person)?;
        let is = self.registry().map_or(id, |r| r.resolve(id));
        let person = identity::parse(is).map_err(not_a_person)?;
        Ok(Resolved {
            id: id.to_string(),
            is: is.to_string(),
            authority: person.authority.clone(),
            held: person.held.map(|d| d.to_string()),
            user_id: person.id.clone(),
            dated: person.is_dated(),
            registry: self.registry().is_some(),
        })
    }

    /// A directory under [`RECOVERY_DIR`] of this root that is not there yet, for the
    /// scratch root of a restore drill.
    #[must_use]
    pub fn drill_scratch(&self) -> PathBuf {
        recovery_dir(self.root(), &new_name(self, "drill-"))
    }

    /// Make a recovery bundle of this root under the root, in a new directory of
    /// [`RECOVERY_DIR`] that is named by the time.
    ///
    /// # Errors
    ///
    /// As [`Memory::recovery_bundle`].
    pub async fn recovery_bundle_here(&self) -> Result<BundleMade, Error> {
        let name = new_name(self, "");
        let manifest = self
            .recovery_bundle(&recovery_dir(self.root(), &name))
            .await?;
        Ok(BundleMade {
            bundle: below_root(&name),
            name,
            domains: manifest.domains.len(),
            files: manifest.files.len(),
            ledger_events: manifest.ledger.events,
            epoch: manifest.ledger.epoch,
        })
    }

    /// Run a restore drill on the bundle `name` under [`RECOVERY_DIR`] of this root. The
    /// scratch root is a new directory under [`RECOVERY_DIR`] too. The name is checked
    /// before it becomes a path: it is one part of a path and nothing else.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for a name that is not a plain name; else as [`Memory::drill`].
    pub async fn drill_named(&self, name: &str) -> Result<DrillRun, Error> {
        let bundle = recovery_dir(self.root(), valid_name(name)?);
        let scratch = new_name(self, "drill-");
        let drill = self
            .drill(&bundle, &recovery_dir(self.root(), &scratch))
            .await?;
        Ok(DrillRun {
            bundle: below_root(name),
            scratch: below_root(&scratch),
            drill,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recovery::Outcome;
    use crate::{WriteRequest, REGISTRY_FILE};

    const OWNER: &str = "human:test,2026-11-01:me";

    fn note(body: &str) -> WriteRequest {
        WriteRequest {
            body: body.into(),
            ..WriteRequest::default()
        }
    }

    #[test]
    fn a_date_is_a_full_calendar_date_and_nothing_else() {
        assert_eq!(day("2026-11-25").unwrap().to_string(), "2026-11-25");
        for bad in [
            "2026-11",
            "2026-13-01",
            "25-11-2026",
            "2026-11-25T10:00:00Z",
            "",
        ] {
            let err = day(bad).unwrap_err();
            assert!(matches!(err, Error::Denied(_)), "{bad}: {err}");
        }
    }

    #[test]
    fn an_identifier_is_followed_through_the_assertions_of_the_registry() {
        let dir = tempfile::tempdir().unwrap();
        Memory::init(dir.path(), OWNER, "dom-me").unwrap();
        let old = "human:old,2019-05-01:me";
        let bare = Memory::open(dir.path()).unwrap();
        let alone = bare.resolve_identity(old).unwrap();
        assert_eq!((alone.is.as_str(), alone.registry), (old, false));
        let registry =
            format!("[[assertions]]\nwas = \"{old}\"\nis = \"{OWNER}\"\nsince = 2026-11-01\n");
        std::fs::write(dir.path().join(REGISTRY_FILE), registry).unwrap();
        // the session that was open before the registry changed is stopped
        assert!(matches!(bare.resolve_identity(old), Err(Error::Stale(_))));
        let m = Memory::open(dir.path()).unwrap();
        let found = m.resolve_identity(old).unwrap();
        assert_eq!(found.id, old);
        assert_eq!(found.is, OWNER);
        assert_eq!(found.authority, "test");
        assert_eq!(found.held.as_deref(), Some("2026-11-01"));
        assert_eq!(found.user_id, "me");
        assert!(found.dated && found.registry);
        let plain = m.resolve_identity("human:test:me").unwrap();
        assert!(!plain.dated);
        assert_eq!(plain.held, None);
        let agent = m.resolve_identity("agent:x").unwrap_err();
        assert!(matches!(agent, Error::Denied(_)), "{agent}");
    }

    #[tokio::test]
    async fn a_bundle_and_its_drill_stay_under_the_root_and_a_name_is_no_path() {
        let dir = tempfile::tempdir().unwrap();
        let m = Memory::init(dir.path(), OWNER, "dom-me").unwrap();
        m.write(note("# Port\n\nThe service listens on 8080.\n"))
            .await
            .unwrap();
        let made = m.recovery_bundle_here().await.unwrap();
        assert_eq!(made.bundle, format!("{RECOVERY_DIR}/{}", made.name));
        assert!(dir
            .path()
            .join(&made.bundle)
            .join("recovery.yaml")
            .is_file());
        assert_eq!(made.domains, 1);
        assert!(made.files > 0);
        // a second bundle in the same second gets its own directory
        let second = m.recovery_bundle_here().await.unwrap();
        assert_ne!(second.name, made.name);
        let run = m.drill_named(&made.name).await.unwrap();
        assert_eq!(run.bundle, made.bundle);
        assert!(run.scratch.starts_with(&format!("{RECOVERY_DIR}/drill-")));
        assert!(dir.path().join(&run.scratch).join("grants.toml").is_file());
        assert!(run.drill.passed);
        assert_eq!(run.drill.cases.len(), 5);
        let absent = |name: &str| {
            let case = run.drill.cases.iter().find(|c| c.name == name).unwrap();
            case.outcome == Outcome::Absent
        };
        assert!(absent("cross-domain-dependency") && absent("uncertain-external-action"));
        for path in ["../x", "a/b", "/tmp/x", "..", ""] {
            let err = m.drill_named(path).await.unwrap_err();
            assert!(err.to_string().contains("is not a name"), "{path}: {err}");
        }
        let agent = Memory::open(dir.path()).unwrap().with_author("agent:x");
        let refused = agent.recovery_bundle_here().await.unwrap_err();
        assert!(refused.to_string().contains("only the owner"), "{refused}");
        let refused = agent.drill_named(&made.name).await.unwrap_err();
        assert!(refused.to_string().contains("only the owner"), "{refused}");
    }

    #[tokio::test]
    async fn a_list_shows_a_proposal_without_its_body() {
        let dir = tempfile::tempdir().unwrap();
        Memory::init(dir.path(), OWNER, "dom-me").unwrap();
        let registry = format!("[domains.dom-me]\nperson = \"{OWNER}\"\n");
        std::fs::write(dir.path().join(REGISTRY_FILE), registry).unwrap();
        let agent = Memory::open(dir.path()).unwrap().with_author("agent:x");
        let body = "# Commits\n\nThe person prefers small commits that a reviewer reads.\n";
        agent.write(note(body)).await.unwrap_err();
        let m = Memory::open(dir.path()).unwrap();
        let lines = proposal_lines(&m.proposals().unwrap());
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].author, "agent:x");
        assert_eq!(lines[0].domain, "dom-me");
        let text = serde_json::to_string(&lines).unwrap();
        assert!(!text.contains("small commits"), "{text}");
    }
}
