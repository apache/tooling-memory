//! The grants file: what one owner may read, at what label, and where it may go.
//!
//! On a laptop there is one principal, the owner, and a file they edit. The file is
//! compiled into a [`ReadFilter`] that the index applies inside every query, and into the
//! ceilings that every outbound call is checked against. An institution replaces this
//! crate's input with its identity provider and policy engine; the filter and the
//! ceilings it produces are the same types.
//!
//! The file:
//!
//! ```toml
//! owner = "human:local:striker"
//! handling = "restricted"            # the highest classification the owner reads
//!
//! [domains.dom-striker]              # one table per domain the owner holds
//! default_classification = "internal"
//!
//! [destinations.github]             # one table per place the agent may send to
//! ceiling = "public"
//! [destinations.notes]
//! ceiling = "restricted"
//! ```
//!
//! A destination that is not in the file has no ceiling and is refused. That is the
//! deny-by-default the paper asks for.

pub mod decide;
pub mod record;
pub mod scan;

use dm_core::{Label, ReadFilter};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// What can go wrong with a grants file.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The file could not be read.
    #[error("grants file {path}: {source}")]
    Io {
        /// The path.
        path: String,
        /// Why.
        source: std::io::Error,
    },
    /// The file is not valid TOML, or does not fit the schema.
    #[error("grants file: {0}")]
    Parse(#[from] toml::de::Error),
    /// The file could not be rendered, which the schema should make impossible.
    #[error("grants file: {0}")]
    Render(#[from] toml::ser::Error),
    /// A value is well-formed but not allowed.
    #[error("grants file: {0}")]
    Invalid(String),
    /// The policy would not compile or evaluate.
    #[error("policy: {0}")]
    Policy(String),
}

/// The gates on writes whose blast radius is not routine; `[gates]` in the file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Gates {
    /// How many entries one agent session may supersede before the rest is the
    /// owner's to admit.
    #[serde(default = "default_max_supersedes")]
    pub max_supersedes_per_session: usize,
    /// Whether replacing a verified entry needs an approver who is not the proposer.
    #[serde(default = "default_true")]
    pub verified_needs_approver: bool,
}

fn default_max_supersedes() -> usize {
    20
}

fn default_true() -> bool {
    true
}

impl Default for Gates {
    fn default() -> Gates {
        Gates {
            max_supersedes_per_session: default_max_supersedes(),
            verified_needs_approver: default_true(),
        }
    }
}

/// One domain's settings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DomainGrant {
    /// What a write gets when the entry states no classification.
    #[serde(default)]
    pub default_classification: Label,
    /// Whether a task a session files from a task it holds, a subagent's work or the
    /// continuation of its own, is admitted as it is filed. Tasks from outside, a tap's
    /// say, stay proposed until the owner admits them either way.
    #[serde(default)]
    pub admit_own: bool,
    /// How many days a session's transcript is kept whole in the record before the turns
    /// no entry cites are dropped; [`DEFAULT_TRANSCRIPT_RETENTION_DAYS`] when unset. The
    /// owner sets it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_retention_days: Option<u32>,
}

/// How long a transcript is kept whole when the domain says nothing.
pub const DEFAULT_TRANSCRIPT_RETENTION_DAYS: u32 = 30;

impl DomainGrant {
    /// The domain's transcript retention clock, in days.
    #[must_use]
    pub fn transcript_retention_days(&self) -> u32 {
        self.transcript_retention_days
            .unwrap_or(DEFAULT_TRANSCRIPT_RETENTION_DAYS)
    }
}

/// One destination's settings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Destination {
    /// The highest classification this destination may receive.
    pub ceiling: Label,
}

/// What one tap may do: a connector the wrapper hosts as a WASI component.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TapGrant {
    /// The domain its proposals land in.
    pub domain: String,
    /// Hosts it may reach, as `host` or `host:port`; nothing else answers.
    #[serde(default)]
    pub hosts: Vec<String>,
    /// The highest classification it may send out.
    pub ceiling: Label,
    /// The environment variable holding the tracker's credential, which the host
    /// injects as a bearer token; the tap never sees it. The file names the variable,
    /// never the secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

/// The grants file, as parsed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grants {
    /// The owner, system-qualified. The author of everything written here by a person.
    pub owner: String,
    /// The highest classification the owner reads.
    #[serde(default = "restricted")]
    pub handling: Label,
    /// Domains the owner holds, by id.
    #[serde(default)]
    pub domains: BTreeMap<String, DomainGrant>,
    /// Places the agent may send to, by name.
    #[serde(default)]
    pub destinations: BTreeMap<String, Destination>,
    /// Taps the wrapper may run, by name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub taps: BTreeMap<String, TapGrant>,
    /// The gates on writes; defaults apply when the table is absent.
    #[serde(default)]
    pub gates: Gates,
    /// Where the task ledger lives when not in the SQLite file under the root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ledger: Option<LedgerSettings>,
}

/// The ledger's backend, when it is a server rather than the file under the root:
///
/// ```toml
/// [ledger]
/// url = "postgres://memory:secret@localhost:5432/memory"
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerSettings {
    /// A PostgreSQL connection URL. The tables are created on first use.
    pub url: String,
}

fn restricted() -> Label {
    Label::Restricted
}

impl Grants {
    /// A starting file for one owner and one domain: the owner reads everything, the
    /// domain's writes default to internal, and the model destinations a laptop uses
    /// accept up to internal. Restricted material stays home until the owner raises
    /// a ceiling by hand.
    #[must_use]
    pub fn starter(owner: &str, domain: &str) -> Grants {
        let mut domains = BTreeMap::new();
        domains.insert(
            domain.to_string(),
            DomainGrant {
                default_classification: Label::Internal,
                admit_own: true,
                transcript_retention_days: None,
            },
        );
        let mut destinations = BTreeMap::new();
        for d in [
            "anthropic",
            "openai",
            "chatgpt",
            "gemini",
            "google",
            "github",
            "xai",
            "dashscope",
            "grok",
            "meta",
            "llmao",
        ] {
            destinations.insert(
                d.to_string(),
                Destination {
                    ceiling: Label::Internal,
                },
            );
        }
        Grants {
            owner: owner.to_string(),
            handling: Label::Restricted,
            domains,
            destinations,
            taps: BTreeMap::new(),
            ledger: None,
            gates: Gates::default(),
        }
    }

    /// Parse the file's text.
    ///
    /// # Errors
    ///
    /// [`Error::Parse`] for bad TOML or a value outside the schema, [`Error::Invalid`] for
    /// an empty owner or no domains.
    pub fn parse(text: &str) -> Result<Grants, Error> {
        let g: Grants = toml::from_str(text)?;
        if g.owner.trim().is_empty() {
            return Err(Error::Invalid("owner is required".into()));
        }
        if g.domains.is_empty() {
            return Err(Error::Invalid("at least one domain is required".into()));
        }
        Ok(g)
    }

    /// Read and parse the file.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when it cannot be read, or what [`Grants::parse`] returns.
    pub fn load(path: &Path) -> Result<Grants, Error> {
        let text = std::fs::read_to_string(path).map_err(|source| Error::Io {
            path: path.display().to_string(),
            source,
        })?;
        Grants::parse(&text)
    }

    /// Render as TOML, for `memory init` to write.
    ///
    /// # Errors
    ///
    /// [`Error::Render`], which the schema should make impossible.
    pub fn render(&self) -> Result<String, Error> {
        Ok(toml::to_string_pretty(self)?)
    }

    /// The filter a session under these grants reads with.
    #[must_use]
    pub fn read_filter(&self) -> ReadFilter {
        ReadFilter::for_domains(self.domains.keys().cloned(), self.handling)
    }

    /// A tap's grant, if the file names it and its domain is one the owner holds.
    #[must_use]
    pub fn tap(&self, name: &str) -> Option<&TapGrant> {
        self.taps.get(name).filter(|t| self.may_write(&t.domain))
    }

    /// Whether the owner may write into a domain.
    #[must_use]
    pub fn may_write(&self, domain: &str) -> bool {
        self.domains.contains_key(domain)
    }

    /// The classification a write gets: what the entry asked for, or the domain's
    /// default, raised to the session's label so that nothing written is lower than what
    /// its author read. `None` when the domain is not granted.
    #[must_use]
    pub fn classification_for_write(
        &self,
        domain: &str,
        requested: Option<Label>,
        session: &Session,
    ) -> Option<Label> {
        let d = self.domains.get(domain)?;
        Some(
            requested
                .unwrap_or(d.default_classification)
                .max(session.label),
        )
    }

    /// The ceiling of a destination, if it is granted at all.
    #[must_use]
    pub fn ceiling(&self, destination: &str) -> Option<Label> {
        self.destinations.get(destination).map(|d| d.ceiling)
    }

    /// A fresh session under these grants, with its label at public.
    #[must_use]
    pub fn session(&self) -> Session {
        Session {
            handling: self.handling,
            label: Label::Public,
        }
    }
}

/// What one session has read so far, and what it may therefore do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    /// The highest classification this session may read. Fixed at the start.
    pub handling: Label,
    /// The highest classification it has read. Rises, never falls.
    pub label: Label,
}

impl Session {
    /// Record that the session read something at this classification. The label rises to
    /// it and never falls. Reading above handling cannot happen, because the filter
    /// never returned it; if a caller reports one anyway, the label is capped and the
    /// report is returned as `false` so the caller can treat it as the bug it is.
    pub fn observe(&mut self, classification: Label) -> bool {
        if classification > self.handling {
            self.label = self.handling;
            return false;
        }
        self.label = self.label.max(classification);
        true
    }

    /// Whether this session may send to a destination with the given ceiling. `None`
    /// means the destination is not granted, and the answer is no.
    #[must_use]
    pub fn may_send(&self, ceiling: Option<Label>) -> bool {
        ceiling.is_some_and(|c| self.label <= c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = r#"
owner = "human:local:striker"
handling = "internal"

[domains.dom-striker]
default_classification = "internal"
transcript_retention_days = 7

[domains.dom-notes]

[destinations.github]
ceiling = "public"
[destinations.notes]
ceiling = "restricted"

[taps.github-issues]
domain = "dom-striker"
hosts = ["api.github.com"]
ceiling = "public"
credential = "GH_TOKEN"

[taps.stray]
domain = "dom-else"
ceiling = "public"
"#;

    #[test]
    fn a_tap_grant_names_a_held_domain_and_round_trips() {
        let g = Grants::parse(FILE).unwrap();
        let t = g.tap("github-issues").unwrap();
        assert_eq!(t.domain, "dom-striker");
        assert_eq!(t.hosts, ["api.github.com"]);
        assert_eq!(t.ceiling, Label::Public);
        assert_eq!(t.credential.as_deref(), Some("GH_TOKEN"));
        assert!(
            g.tap("stray").is_none(),
            "a tap on a domain the owner does not hold"
        );
        assert!(g.tap("nope").is_none());
        let again = Grants::parse(&g.render().unwrap()).unwrap();
        assert_eq!(again, g);
        let starter = Grants::starter("human:x:y", "dom-y");
        assert!(starter.taps.is_empty());
        assert!(
            !starter.render().unwrap().contains("[taps"),
            "no section until one is granted"
        );
    }

    #[test]
    fn parses_and_compiles_to_a_filter() {
        let g = Grants::parse(FILE).unwrap();
        assert_eq!(g.owner, "human:local:striker");
        assert_eq!(g.handling, Label::Internal);
        let f = g.read_filter();
        assert_eq!(f.domains, vec!["dom-notes", "dom-striker"]);
        assert_eq!(f.handling, Label::Internal);
        assert!(!f.include_inactive);
        assert!(g.may_write("dom-striker"));
        assert!(!g.may_write("dom-else"));
        assert_eq!(
            g.domains["dom-notes"].default_classification,
            Label::Public,
            "a domain table with nothing in it defaults to public"
        );
        assert_eq!(g.domains["dom-striker"].transcript_retention_days(), 7);
        assert_eq!(
            g.domains["dom-notes"].transcript_retention_days(),
            DEFAULT_TRANSCRIPT_RETENTION_DAYS,
            "a domain that says nothing keeps transcripts thirty days"
        );
    }

    #[test]
    fn the_label_rises_and_never_falls_and_gates_sending() {
        let g = Grants::parse(FILE).unwrap();
        let mut s = g.session();
        assert_eq!(s.label, Label::Public);
        assert!(s.may_send(g.ceiling("github")));
        assert!(s.observe(Label::Internal));
        assert_eq!(s.label, Label::Internal);
        assert!(
            s.observe(Label::Public),
            "reading something public changes nothing"
        );
        assert_eq!(s.label, Label::Internal);
        assert!(
            !s.may_send(g.ceiling("github")),
            "internal exceeds a public ceiling"
        );
        assert!(s.may_send(g.ceiling("notes")));
        assert!(
            !s.may_send(g.ceiling("nowhere")),
            "an ungranted destination is refused"
        );
        assert!(!s.may_send(None));
    }

    #[test]
    fn a_read_above_handling_is_reported_and_capped() {
        let g = Grants::parse(FILE).unwrap();
        let mut s = g.session();
        assert!(!s.observe(Label::Restricted));
        assert_eq!(s.label, Label::Internal);
    }

    #[test]
    fn writes_inherit_the_session_label() {
        let g = Grants::parse(FILE).unwrap();
        let mut s = g.session();
        assert_eq!(
            g.classification_for_write("dom-notes", None, &s),
            Some(Label::Public),
            "domain default"
        );
        assert_eq!(
            g.classification_for_write("dom-striker", None, &s),
            Some(Label::Internal)
        );
        assert_eq!(
            g.classification_for_write("dom-notes", Some(Label::Internal), &s),
            Some(Label::Internal),
            "the entry may ask for more"
        );
        s.observe(Label::Internal);
        assert_eq!(
            g.classification_for_write("dom-notes", Some(Label::Public), &s),
            Some(Label::Internal),
            "never lower than what the author read"
        );
        assert_eq!(g.classification_for_write("dom-else", None, &s), None);
    }

    #[test]
    fn starter_round_trips_and_keeps_restricted_home() {
        let g = Grants::starter("human:local:me", "dom-me");
        let text = g.render().unwrap();
        let back = Grants::parse(&text).unwrap();
        assert_eq!(back, g);
        assert_eq!(back.handling, Label::Restricted);
        assert_eq!(back.ceiling("anthropic"), Some(Label::Internal));
        assert_eq!(back.ceiling("llmao"), Some(Label::Internal));
        assert!(!back.session().may_send(back.ceiling("anything")));
        let mut s = back.session();
        s.observe(Label::Restricted);
        assert!(
            !s.may_send(back.ceiling("anthropic")),
            "restricted stays home"
        );
        assert!(text.contains("[domains.dom-me]"), "{text}");
        assert!(text.contains("[destinations.anthropic]"), "{text}");
        // every destination the launcher's harnesses route to by default has a ceiling
        for d in ["dashscope", "grok", "google", "chatgpt", "github", "xai"] {
            assert_eq!(back.ceiling(d), Some(Label::Internal), "{d}");
        }
        assert_eq!(back.ceiling("qwen"), None, "no such destination");
    }

    #[test]
    fn bad_files_are_errors() {
        assert!(matches!(Grants::parse("owner = 1"), Err(Error::Parse(_))));
        assert!(matches!(
            Grants::parse("owner = \"x\"\nhandling = \"secret\"\n[domains.a]\n"),
            Err(Error::Parse(_))
        ));
        assert!(matches!(
            Grants::parse("owner = \"\"\n[domains.a]\n"),
            Err(Error::Invalid(_))
        ));
        assert!(matches!(
            Grants::parse("owner = \"x\"\n"),
            Err(Error::Invalid(_))
        ));
        assert!(matches!(
            Grants::load(Path::new("/nonexistent/grants.toml")),
            Err(Error::Io { .. })
        ));
    }
}
