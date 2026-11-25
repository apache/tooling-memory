//! The ownership registry: which unit answers for which domain, and who owns the unit.
//!
//! The registry is a file beside the grants, edited by hand as the grants are. A unit
//! has an opaque key, because the name of a team changes and the key must not. An owner
//! is a person in the dated form ([`dm_core::identity`]). A rename of a person is a
//! dated assertion, and no entry is rewritten for it.
//!
//! ```toml
//! [units.unit-7f3a]                 # an opaque key, never a team name
//! name = "Infrastructure"          # a label
//! domains = ["dom-demo"]
//! horizon_scanner = "human:demo,2026-11-01:check"
//! horizon_cadence_days = 30
//!
//! [[units.unit-7f3a.owners]]
//! id = "human:demo,2026-11-01:check"
//! until = 2027-01-01               # the last day this person answers for the unit
//!
//! [domains.dom-demo]
//! repositories = ["https://example.org/infra.git"]
//! queues = ["infra"]
//! person = "human:demo,2026-11-01:check"   # makes the domain a personal domain
//! proposal_days = 30
//!
//! [[assertions]]
//! was = "human:old,2019-05-01:check"
//! is = "human:demo,2026-11-01:check"
//! since = 2026-11-01
//! ```

use crate::Error;
use dm_core::identity::{self, Assertion};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use time::Date;

/// How many days pass between two evidence tasks of a horizon scanner when the unit
/// names no cadence.
pub const DEFAULT_HORIZON_CADENCE_DAYS: u32 = 30;
/// How many days a proposal to a personal domain waits for an answer when the domain
/// names no period.
pub const DEFAULT_PROPOSAL_DAYS: u32 = 30;

/// One person who answers for a unit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Owner {
    /// The person, in the dated form.
    pub id: String,
    /// The last day on which the person answers for the unit. Absent means no end.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "day::optional"
    )]
    pub until: Option<Date>,
}

impl Owner {
    /// Whether the person answers for the unit on `today`.
    #[must_use]
    pub fn answers_on(&self, today: Date) -> bool {
        self.until.is_none_or(|until| until >= today)
    }
}

/// One unit: the people who answer for a set of domains.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Unit {
    /// A label for people. The key of the unit is what the registry compares.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// The domains the unit answers for.
    #[serde(default)]
    pub domains: Vec<String>,
    /// The person who reviews the evidence of the unit's horizon, in the dated form.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub horizon_scanner: Option<String>,
    /// How many days pass between two evidence tasks of the horizon scanner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub horizon_cadence_days: Option<u32>,
    /// The people who answer for the unit. Several owners own it together.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub owners: Vec<Owner>,
}

impl Unit {
    /// How many days pass between two evidence tasks of the horizon scanner.
    #[must_use]
    pub fn horizon_cadence_days(&self) -> u32 {
        self.horizon_cadence_days
            .unwrap_or(DEFAULT_HORIZON_CADENCE_DAYS)
    }
}

/// What the registry says of one domain.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DomainRecord {
    /// The repositories whose work the domain holds.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repositories: Vec<String>,
    /// The queues whose work the domain holds.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queues: Vec<String>,
    /// The person the domain is about, in the dated form. A domain with a person is a
    /// personal domain: a write there by another author is a proposal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub person: Option<String>,
    /// How many days a proposal waits for an answer before it expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposal_days: Option<u32>,
}

impl DomainRecord {
    /// How many days a proposal waits for an answer before it expires.
    #[must_use]
    pub fn proposal_days(&self) -> u32 {
        self.proposal_days.unwrap_or(DEFAULT_PROPOSAL_DAYS)
    }
}

/// A rename as the file holds it: the day is a TOML date or text.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AssertionRow {
    was: String,
    is: String,
    #[serde(with = "day")]
    since: Date,
}

/// The file as it is on disk.
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    units: BTreeMap<String, Unit>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    domains: BTreeMap<String, DomainRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    assertions: Vec<AssertionRow>,
}

/// The ownership registry, as parsed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Registry {
    /// The units, by their opaque key.
    pub units: BTreeMap<String, Unit>,
    /// What the registry says of each domain, by id.
    pub domains: BTreeMap<String, DomainRecord>,
    /// The renames of people, in the order of the file.
    pub assertions: Vec<Assertion>,
}

/// Something in the registry that a person must change.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "flag", rename_all = "kebab-case")]
pub enum Flag {
    /// No person answers for the domain today.
    Ownerless {
        /// The domain.
        domain: String,
    },
    /// The grants hold the domain, and no unit of the registry lists it.
    NotInRegistry {
        /// The domain.
        domain: String,
    },
    /// The person of a personal domain does not hold the grants.
    PersonalNotHolder {
        /// The domain.
        domain: String,
        /// The person the registry names for the domain.
        person: String,
    },
    /// The unit names no horizon scanner.
    NoScanner {
        /// The key of the unit.
        unit: String,
    },
}

impl Flag {
    /// The word for the kind of flag, as the tag `flag` holds it.
    #[must_use]
    pub fn word(&self) -> &'static str {
        match self {
            Flag::Ownerless { .. } => "ownerless",
            Flag::NotInRegistry { .. } => "not-in-registry",
            Flag::PersonalNotHolder { .. } => "personal-not-holder",
            Flag::NoScanner { .. } => "no-scanner",
        }
    }

    /// The domain or the unit that the flag is about.
    #[must_use]
    pub fn subject(&self) -> &str {
        match self {
            Flag::Ownerless { domain }
            | Flag::NotInRegistry { domain }
            | Flag::PersonalNotHolder { domain, .. } => domain,
            Flag::NoScanner { unit } => unit,
        }
    }
}

/// Refuse `id` unless it is a person in the dated form.
fn dated(place: &str, id: &str) -> Result<(), Error> {
    let person =
        identity::parse(id).map_err(|e| Error::Registry(format!("{place} = {id:?}: {e}")))?;
    if person.is_dated() {
        Ok(())
    } else {
        Err(Error::Registry(format!(
            "{place} = {id:?}: the registry names a person in the dated form, \
             `human:<authority>,<YYYY-MM-DD>:<user-id>`"
        )))
    }
}

/// Refuse a period of no days.
fn not_zero(place: &str, days: Option<u32>) -> Result<(), Error> {
    if days == Some(0) {
        return Err(Error::Registry(format!(
            "{place} = 0: a period has one day or more"
        )));
    }
    Ok(())
}

impl Registry {
    /// Parse the text of the file.
    ///
    /// # Errors
    ///
    /// [`Error::Registry`] for bad TOML or a value outside the schema; for an owner, a
    /// `person` or a `horizon_scanner` that is not a person in the dated form; for an
    /// assertion whose `was` or `is` names no person; and for a period of no days.
    pub fn parse(text: &str) -> Result<Registry, Error> {
        let file: File = toml::from_str(text).map_err(|e| Error::Registry(e.to_string()))?;
        let registry = Registry {
            units: file.units,
            domains: file.domains,
            assertions: file
                .assertions
                .into_iter()
                .map(|a| Assertion {
                    was: a.was,
                    is: a.is,
                    since: a.since,
                })
                .collect(),
        };
        registry.validate()?;
        Ok(registry)
    }

    fn validate(&self) -> Result<(), Error> {
        for (key, unit) in &self.units {
            for owner in &unit.owners {
                dated(&format!("[[units.{key}.owners]] id"), &owner.id)?;
            }
            if let Some(scanner) = &unit.horizon_scanner {
                dated(&format!("[units.{key}] horizon_scanner"), scanner)?;
            }
            not_zero(
                &format!("[units.{key}] horizon_cadence_days"),
                unit.horizon_cadence_days,
            )?;
        }
        for (id, domain) in &self.domains {
            if let Some(person) = &domain.person {
                dated(&format!("[domains.{id}] person"), person)?;
            }
            not_zero(
                &format!("[domains.{id}] proposal_days"),
                domain.proposal_days,
            )?;
        }
        for a in &self.assertions {
            for (word, id) in [("was", &a.was), ("is", &a.is)] {
                identity::parse(id)
                    .map_err(|e| Error::Registry(format!("[[assertions]] {word} = {id:?}: {e}")))?;
            }
            self.ends(a)?;
        }
        Ok(())
    }

    /// Refuse an assertion whose chain comes back to an identifier that it passed. In a
    /// cycle no identifier is the one that names the person now, so two identifiers of
    /// the cycle would not be one person.
    fn ends(&self, first: &Assertion) -> Result<(), Error> {
        let mut seen = vec![first.was.as_str()];
        let mut now = first.is.as_str();
        loop {
            if seen.contains(&now) {
                return Err(Error::Registry(format!(
                    "[[assertions]] was = {:?}: the assertions come back to {now:?}, so none of them names the person now",
                    first.was
                )));
            }
            seen.push(now);
            match self.assertions.iter().find(|a| a.was == now) {
                Some(next) => now = next.is.as_str(),
                None => return Ok(()),
            }
        }
    }

    /// Read and parse the file. A root with no registry file has no registry.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when the file is there and cannot be read, or what
    /// [`Registry::parse`] returns.
    pub fn load(path: &Path) -> Result<Option<Registry>, Error> {
        match std::fs::read_to_string(path) {
            Ok(text) => Registry::parse(&text).map(Some),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(Error::Io {
                path: path.display().to_string(),
                source,
            }),
        }
    }

    /// Render as TOML.
    ///
    /// # Errors
    ///
    /// [`Error::Render`], which the schema should make impossible.
    pub fn render(&self) -> Result<String, Error> {
        let file = File {
            units: self.units.clone(),
            domains: self.domains.clone(),
            assertions: self
                .assertions
                .iter()
                .map(|a| AssertionRow {
                    was: a.was.clone(),
                    is: a.is.clone(),
                    since: a.since,
                })
                .collect(),
        };
        Ok(toml::to_string_pretty(&file)?)
    }

    /// The person of a personal domain. None for a domain that is not personal.
    #[must_use]
    pub fn personal(&self, domain: &str) -> Option<&str> {
        self.domains.get(domain)?.person.as_deref()
    }

    /// The identifier that names the person of `id` now, by the assertions.
    #[must_use]
    pub fn resolve<'a>(&'a self, id: &'a str) -> &'a str {
        identity::resolve(id, &self.assertions)
    }

    /// Whether two identifiers name one person, by the assertions.
    #[must_use]
    pub fn same_person(&self, a: &str, b: &str) -> bool {
        self.resolve(a) == self.resolve(b)
    }

    /// What a person must change, for the domains that the grants hold. `holder` is the
    /// owner of the grants.
    ///
    /// - [`Flag::PersonalNotHolder`]: the person of a personal domain is not `holder`.
    ///   The assertions decide whether two identifiers name one person. A personal
    ///   domain needs no unit.
    /// - [`Flag::NotInRegistry`]: no unit lists the domain, and it has no person.
    /// - [`Flag::Ownerless`]: the units that list the domain have no owner who answers
    ///   on `today`.
    /// - [`Flag::NoScanner`]: a unit names no horizon scanner.
    #[must_use]
    pub fn check(&self, granted: &[String], holder: &str, today: Date) -> Vec<Flag> {
        let mut flags = Vec::new();
        for domain in granted {
            if let Some(person) = self.personal(domain) {
                if !self.same_person(person, holder) {
                    flags.push(Flag::PersonalNotHolder {
                        domain: domain.clone(),
                        person: person.to_string(),
                    });
                }
                continue;
            }
            let mut owners = self
                .units
                .values()
                .filter(|u| u.domains.contains(domain))
                .peekable();
            if owners.peek().is_none() {
                flags.push(Flag::NotInRegistry {
                    domain: domain.clone(),
                });
            } else if !owners.any(|u| u.owners.iter().any(|o| o.answers_on(today))) {
                flags.push(Flag::Ownerless {
                    domain: domain.clone(),
                });
            }
        }
        for (key, unit) in &self.units {
            if unit.horizon_scanner.is_none() {
                flags.push(Flag::NoScanner { unit: key.clone() });
            }
        }
        flags
    }
}

/// A calendar date in the file: a TOML date, `2027-01-01`, or the same as text.
mod day {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use time::Date;
    use toml::value::Datetime;

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Shape {
        Toml(Datetime),
        Text(String),
    }

    fn to_toml(date: Date) -> Datetime {
        Datetime {
            date: Some(toml::value::Date {
                // a year outside the range of the file is written as year 0, which no
                // date of an owner or an assertion is
                year: u16::try_from(date.year()).unwrap_or(0),
                month: u8::from(date.month()),
                day: date.day(),
            }),
            time: None,
            offset: None,
        }
    }

    fn from_shape<E: serde::de::Error>(shape: Shape) -> Result<Date, E> {
        let (year, month, day) = match &shape {
            Shape::Toml(Datetime {
                date: Some(d),
                time: None,
                offset: None,
            }) => (i32::from(d.year), d.month, d.day),
            Shape::Toml(other) => {
                return Err(E::custom(format!(
                    "{other} is not a calendar date `YYYY-MM-DD`"
                )))
            }
            Shape::Text(text) => {
                let d: Datetime = text
                    .parse()
                    .map_err(|e| E::custom(format!("{text:?}: {e}")))?;
                return from_shape(Shape::Toml(d));
            }
        };
        time::Month::try_from(month)
            .and_then(|m| Date::from_calendar_date(year, m, day))
            .map_err(|e| E::custom(format!("{year:04}-{month:02}-{day:02}: {e}")))
    }

    pub(super) fn serialize<S: Serializer>(date: &Date, s: S) -> Result<S::Ok, S::Error> {
        to_toml(*date).serialize(s)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Date, D::Error> {
        from_shape(Shape::deserialize(d)?)
    }

    /// The same for a date that may be absent.
    pub(super) mod optional {
        use super::{from_shape, to_toml, Shape};
        use serde::{Deserialize, Deserializer, Serialize, Serializer};
        use time::Date;

        #[allow(clippy::ref_option)] // serde passes a reference to the field
        pub(in super::super) fn serialize<S: Serializer>(
            date: &Option<Date>,
            s: S,
        ) -> Result<S::Ok, S::Error> {
            date.map(to_toml).serialize(s)
        }

        pub(in super::super) fn deserialize<'de, D: Deserializer<'de>>(
            d: D,
        ) -> Result<Option<Date>, D::Error> {
            Option::<Shape>::deserialize(d)?.map(from_shape).transpose()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"
[units.unit-7f3a]
name = "Infrastructure"
domains = ["dom-demo"]
horizon_scanner = "human:demo,2026-11-01:check"
horizon_cadence_days = 30

[[units.unit-7f3a.owners]]
id = "human:demo,2026-11-01:check"
until = 2027-01-01

[domains.dom-demo]
repositories = ["https://example.org/infra.git"]
queues = ["infra"]
person = "human:demo,2026-11-01:check"
proposal_days = 30

[[assertions]]
was = "human:old,2019-05-01:check"
is = "human:demo,2026-11-01:check"
since = 2026-11-01
"#;

    fn day(text: &str) -> Date {
        let f = time::macros::format_description!("[year]-[month]-[day]");
        Date::parse(text, f).unwrap()
    }

    fn refused(text: &str) -> String {
        match Registry::parse(text) {
            Err(Error::Registry(why)) => why,
            other => panic!("not refused: {other:?}"),
        }
    }

    /// One unit that lists `dom-a`, with one owner whose last day is `until`.
    fn one_owner(until: Option<&str>) -> Registry {
        let until = until.map_or(String::new(), |d| format!("until = {d}\n"));
        Registry::parse(&format!(
            "[units.u1]\ndomains = [\"dom-a\"]\nhorizon_scanner = \"human:a,2026-01-01:s\"\n\
             [[units.u1.owners]]\nid = \"human:a,2026-01-01:o\"\n{until}"
        ))
        .unwrap()
    }

    const HOLDER: &str = "human:demo,2026-11-01:check";

    #[test]
    fn the_example_parses_and_renders_back_equal() {
        let r = Registry::parse(EXAMPLE).unwrap();
        let unit = &r.units["unit-7f3a"];
        assert_eq!(unit.name, "Infrastructure");
        assert_eq!(unit.owners[0].until, Some(day("2027-01-01")));
        assert_eq!(unit.horizon_cadence_days(), 30);
        assert_eq!(r.domains["dom-demo"].queues, vec!["infra".to_string()]);
        assert_eq!(r.personal("dom-demo"), Some(HOLDER));
        assert_eq!(r.personal("dom-other"), None);
        assert_eq!(r.assertions[0].since, day("2026-11-01"));
        let text = r.render().unwrap();
        assert!(text.contains("until = 2027-01-01"), "a TOML date: {text}");
        assert!(text.contains("since = 2026-11-01"), "a TOML date: {text}");
        assert_eq!(Registry::parse(&text).unwrap(), r);
    }

    #[test]
    fn the_periods_have_defaults_and_a_date_reads_as_text_too() {
        let r = Registry::parse(
            "[units.u1]\n[[units.u1.owners]]\nid = \"human:a,2026-01-01:o\"\n\
             until = \"2027-01-01\"\n[domains.dom-a]\n",
        )
        .unwrap();
        assert_eq!(r.units["u1"].owners[0].until, Some(day("2027-01-01")));
        assert_eq!(r.units["u1"].horizon_cadence_days(), 30);
        assert_eq!(r.domains["dom-a"].proposal_days(), 30);
        assert_eq!(Registry::parse("").unwrap(), Registry::default());
    }

    #[test]
    fn a_person_of_the_registry_is_in_the_dated_form() {
        let why = refused("[units.u1]\n[[units.u1.owners]]\nid = \"human:demo:check\"\n");
        assert!(why.contains("dated form"), "{why}");
        let why = refused("[units.u1]\n[[units.u1.owners]]\nid = \"agent:curator\"\n");
        assert!(why.contains("owners"), "{why}");
        let why = refused("[domains.dom-a]\nperson = \"human:demo:check\"\n");
        assert!(why.contains("person"), "{why}");
        let why = refused("[units.u1]\nhorizon_scanner = \"human:demo:check\"\n");
        assert!(why.contains("horizon_scanner"), "{why}");
    }

    #[test]
    fn an_assertion_names_two_people_and_a_day() {
        let row = |was: &str, is: &str, since: &str| {
            format!("[[assertions]]\nwas = \"{was}\"\nis = \"{is}\"\nsince = {since}\n")
        };
        let why = refused(&row("agent:x", "human:a,2026-01-01:o", "2026-01-01"));
        assert!(why.contains("was"), "{why}");
        let why = refused(&row("human:a:o", "team-a", "2026-01-01"));
        assert!(why.contains("is ="), "{why}");
        refused(&row("human:a:o", "human:b:o", "2026-01-01T10:00:00Z"));
        refused(&row("human:a:o", "human:b:o", "\"2026-02-30\""));
        // the form on disk today is a person: an assertion carries it to the dated form
        Registry::parse(&row("human:a:o", "human:a,2026-01-01:o", "2026-01-01")).unwrap();
    }

    #[test]
    fn assertions_that_form_a_cycle_are_refused() {
        let row = |was: &str, is: &str| {
            format!("[[assertions]]\nwas = \"{was}\"\nis = \"{is}\"\nsince = 2026-01-01\n")
        };
        let (a, b, c) = ("human:a:o", "human:b:o", "human:c:o");
        let why = refused(&(row(a, b) + &row(b, a)));
        assert!(why.contains("come back to"), "{why}");
        refused(&row(a, a));
        refused(&(row(a, b) + &row(b, c) + &row(c, b)));
        // a chain that ends is taken, and each identifier of it names one person
        let chain = Registry::parse(&(row(a, b) + &row(b, c))).unwrap();
        assert!(chain.same_person(a, c) && chain.same_person(b, c));
    }

    #[test]
    fn a_period_of_no_days_is_refused() {
        let why = refused("[units.u1]\nhorizon_cadence_days = 0\n");
        assert!(why.contains("horizon_cadence_days"), "{why}");
        let why = refused("[domains.dom-a]\nproposal_days = 0\n");
        assert!(why.contains("proposal_days"), "{why}");
    }

    #[test]
    fn a_key_the_schema_does_not_have_is_refused() {
        refused(
            "[units.u1]\n[[units.u1.owners]]\nid = \"human:a,2026-01-01:o\"\nuntill = 2027-01-01\n",
        );
    }

    #[test]
    fn a_missing_file_is_no_registry() {
        let dir = std::env::temp_dir().join(format!("dm-registry-{}", std::process::id()));
        assert_eq!(Registry::load(&dir.join("registry.toml")).unwrap(), None);
    }

    #[test]
    fn a_domain_is_ownerless_from_the_day_after_its_last_owner_ended() {
        let granted = vec!["dom-a".to_string()];
        let today = day("2026-11-24");
        let ended = one_owner(Some("2026-11-23"));
        assert_eq!(
            ended.check(&granted, HOLDER, today),
            vec![Flag::Ownerless {
                domain: "dom-a".into()
            }]
        );
        assert!(one_owner(Some("2026-11-24"))
            .check(&granted, HOLDER, today)
            .is_empty());
        assert!(one_owner(None).check(&granted, HOLDER, today).is_empty());
    }

    #[test]
    fn owners_together_keep_a_domain_owned_while_one_of_them_answers() {
        let r = Registry::parse(
            "[units.u1]\ndomains = [\"dom-a\"]\nhorizon_scanner = \"human:a,2026-01-01:s\"\n\
             [[units.u1.owners]]\nid = \"human:a,2026-01-01:o\"\nuntil = 2026-01-31\n\
             [[units.u1.owners]]\nid = \"human:a,2026-01-01:p\"\n",
        )
        .unwrap();
        assert!(r
            .check(&["dom-a".to_string()], HOLDER, day("2026-11-24"))
            .is_empty());
    }

    #[test]
    fn a_granted_domain_in_no_unit_is_not_in_the_registry() {
        let flags = one_owner(None).check(
            &["dom-a".to_string(), "dom-b".to_string()],
            HOLDER,
            day("2026-11-24"),
        );
        assert_eq!(
            flags,
            vec![Flag::NotInRegistry {
                domain: "dom-b".into()
            }]
        );
    }

    #[test]
    fn a_unit_with_no_scanner_is_flagged() {
        let r = Registry::parse(
            "[units.u1]\ndomains = [\"dom-a\"]\n[[units.u1.owners]]\nid = \"human:a,2026-01-01:o\"\n",
        )
        .unwrap();
        let flags = r.check(&["dom-a".to_string()], HOLDER, day("2026-11-24"));
        assert_eq!(flags, vec![Flag::NoScanner { unit: "u1".into() }]);
        assert_eq!(flags[0].word(), "no-scanner");
        assert_eq!(
            serde_json::to_string(&flags[0]).unwrap(),
            r#"{"flag":"no-scanner","unit":"u1"}"#
        );
    }

    #[test]
    fn the_person_of_a_personal_domain_holds_the_grants() {
        let r = Registry::parse(EXAMPLE).unwrap();
        let granted = vec!["dom-demo".to_string()];
        let today = day("2026-11-24");
        assert!(r.check(&granted, HOLDER, today).is_empty());
        // the holder under an older name: the assertion says it is the same person
        assert!(r
            .check(&granted, "human:old,2019-05-01:check", today)
            .is_empty());
        assert_eq!(
            r.check(&granted, "human:demo,2026-11-01:other", today),
            vec![Flag::PersonalNotHolder {
                domain: "dom-demo".into(),
                person: HOLDER.into(),
            }]
        );
    }

    #[test]
    fn a_personal_domain_needs_no_unit() {
        let r = Registry::parse(&format!("[domains.dom-me]\nperson = \"{HOLDER}\"\n")).unwrap();
        assert!(r
            .check(&["dom-me".to_string()], HOLDER, day("2026-11-24"))
            .is_empty());
    }
}
