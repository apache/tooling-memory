//! The identity of a person, as an author or an owner names one.
//!
//! An identifier lives longer than the system that issued it. The dated form says which
//! authority issued the identifier and on which day the authority held that name:
//!
//! ```text
//! human:<authority>,<YYYY-MM-DD>:<user-id>
//! ```
//!
//! The form without a date, `human:<system>:<id>`, is what entries on disk hold, and it
//! stays readable. A rename is an [`Assertion`]: a dated statement that one identifier
//! is now another. Entries are never rewritten for a rename; a reader follows the
//! assertions with [`resolve`].

use crate::Error;
use serde::{Deserialize, Serialize};
use time::{macros::format_description, Date};

/// The prefix of every identifier of a person.
const PREFIX: &str = "human:";

/// A person, as an identifier names one.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Person {
    /// Who issued the identifier: an identity system, a domain name, an organisation.
    pub authority: String,
    /// A day on which the authority held that name. None for the form without a date.
    pub held: Option<Date>,
    /// The identifier the authority gave the person.
    pub id: String,
}

impl Person {
    /// The identifier as text: what [`parse`] read.
    #[must_use]
    pub fn render(&self) -> String {
        match self.held {
            Some(day) => format!("{PREFIX}{},{day}:{}", self.authority, self.id),
            None => format!("{PREFIX}{}:{}", self.authority, self.id),
        }
    }

    /// Whether the identifier says on which day its authority held the name.
    #[must_use]
    pub fn is_dated(&self) -> bool {
        self.held.is_some()
    }
}

/// Read a calendar date written as `YYYY-MM-DD`, and nothing shorter.
fn full_date(text: &str) -> Option<Date> {
    if text.len() != 10 {
        return None;
    }
    Date::parse(text, format_description!("[year]-[month]-[day]")).ok()
}

/// Read the identifier of a person: the dated form
/// `human:<authority>,<YYYY-MM-DD>:<user-id>`, or the form without a date,
/// `human:<system>:<id>`.
///
/// # Errors
///
/// [`Error::Invalid`] when the text does not name a person, when the authority or the
/// id is empty, or when the date is not a full calendar date.
pub fn parse(text: &str) -> Result<Person, Error> {
    let bad = |why: &str| Error::Invalid(format!("identity {text:?}: {why}"));
    let rest = text
        .strip_prefix(PREFIX)
        .ok_or_else(|| bad("a person is `human:<authority>,<YYYY-MM-DD>:<user-id>`"))?;
    let (head, id) = rest
        .split_once(':')
        .ok_or_else(|| bad("the user id is missing"))?;
    if id.is_empty() {
        return Err(bad("the user id is empty"));
    }
    let (authority, held) = match head.split_once(',') {
        Some((authority, day)) => {
            let day = full_date(day).ok_or_else(|| bad("the date is not `YYYY-MM-DD`"))?;
            (authority, Some(day))
        }
        None => (head, None),
    };
    if authority.is_empty() {
        return Err(bad("the authority is empty"));
    }
    Ok(Person {
        authority: authority.to_string(),
        held,
        id: id.to_string(),
    })
}

/// A dated statement that the person one identifier named is named by another from a
/// day on. Entries that hold the earlier identifier are not rewritten.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assertion {
    /// The earlier identifier.
    pub was: String,
    /// The identifier that names the same person now.
    pub is: String,
    /// The day from which the statement holds.
    #[serde(with = "day")]
    pub since: Date,
}

/// A calendar date as the text `YYYY-MM-DD`.
mod day {
    use serde::{de::Error as _, Deserialize, Deserializer, Serializer};
    use time::Date;

    pub(super) fn serialize<S: Serializer>(date: &Date, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(date)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Date, D::Error> {
        let text = String::deserialize(d)?;
        super::full_date(&text)
            .ok_or_else(|| D::Error::custom(format!("{text:?} is not a date `YYYY-MM-DD`")))
    }
}

/// The identifier that names the person of `id` now: `was -> is` is followed until no
/// assertion names the identifier. An identifier that no assertion names is its own
/// answer. Assertions that form a cycle end at the first identifier that is met twice.
#[must_use]
pub fn resolve<'a>(id: &'a str, assertions: &'a [Assertion]) -> &'a str {
    let mut seen = vec![id];
    let mut now = id;
    while let Some(next) = assertions.iter().find(|a| a.was == now) {
        now = next.is.as_str();
        if seen.contains(&now) {
            break;
        }
        seen.push(now);
    }
    now
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::date;

    #[test]
    fn the_dated_form_parses_and_renders_to_the_same_text() {
        let text = "human:example.org,2026-11-01:check";
        let p = parse(text).unwrap();
        assert_eq!(
            p,
            Person {
                authority: "example.org".into(),
                held: Some(date!(2026 - 11 - 01)),
                id: "check".into(),
            }
        );
        assert!(p.is_dated());
        assert_eq!(p.render(), text);
    }

    #[test]
    fn the_form_on_disk_parses_with_no_date() {
        let p = parse("human:test:me").unwrap();
        assert_eq!(p.authority, "test");
        assert_eq!(p.held, None);
        assert_eq!(p.id, "me");
        assert!(!p.is_dated());
        assert_eq!(p.render(), "human:test:me");
    }

    #[test]
    fn text_that_names_no_person_is_refused() {
        for text in [
            "agent:x",
            "human::id",
            "human:a,2026-13-40:id",
            "human:a,:id",
            "human:a,2026-11-01:",
            "human:a",
            "human:,2026-11-01:id",
            "human:a,2026-1-1:id",
            "human:a,b,2026-11-01:id",
        ] {
            assert!(parse(text).is_err(), "{text} is no person");
        }
    }

    fn says(was: &str, is: &str) -> Assertion {
        Assertion {
            was: was.into(),
            is: is.into(),
            since: date!(2026 - 11 - 01),
        }
    }

    #[test]
    fn resolve_follows_a_chain_and_leaves_an_id_that_no_assertion_names() {
        let chain = [
            says("human:a:x", "human:b:x"),
            says("human:b:x", "human:c:x"),
        ];
        assert_eq!(resolve("human:a:x", &chain), "human:c:x");
        assert_eq!(resolve("human:b:x", &chain), "human:c:x");
        assert_eq!(resolve("human:other:y", &chain), "human:other:y");
    }

    #[test]
    fn resolve_ends_on_a_cycle() {
        let cycle = [
            says("human:a:x", "human:b:x"),
            says("human:b:x", "human:c:x"),
            says("human:c:x", "human:b:x"),
        ];
        assert_eq!(resolve("human:a:x", &cycle), "human:b:x");
        assert_eq!(resolve("human:b:x", &cycle), "human:b:x");
    }

    #[test]
    fn an_assertion_holds_its_day_as_a_calendar_date() {
        let a = says("human:a:x", "human:b:x");
        let text = serde_norway::to_string(&a).unwrap();
        assert!(text.contains("since: 2026-11-01"), "{text}");
        assert_eq!(serde_norway::from_str::<Assertion>(&text).unwrap(), a);
    }
}
