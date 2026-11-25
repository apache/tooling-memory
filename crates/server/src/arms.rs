//! Session settings: the off switches of the paper's section 11, and the evaluation arms.
//!
//! A session's settings narrow what it reads and nothing else. The launcher sets them
//! once, before the session reads; the session cannot set them. An evaluation arm is one
//! fixed value of the settings, so an arm differs from production in configuration only,
//! and no arm reads more than the session's grants and policy give it.
//!
//! Two places hold the settings. [`Settings::apply`] narrows the read filter every read
//! is under, so no read path serves an entry the settings leave out. The MCP handler asks
//! [`Memory::admits_tool`] before it serves a harness's call, so the agent is told why a
//! read tool is refused, where an empty answer would say the record holds nothing.
//!
//! Writes and the task tools are not governed by the settings. Where the service reads
//! on behalf of a write (the handoff a release names, the definition a task names), it
//! reads under the filter as the grants and the policy give it.

use crate::transcripts::{State, Transcript};
use crate::{lock, Error, Memory};
use dm_core::{approx_tokens, EntryType, ReadFilter, TrustTier};
use dm_index::Hit;
use serde::{Deserialize, Serialize};
use std::fmt;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// The token budget of the transcripts the transcript-replay arm puts in front of a prompt.
pub const TRANSCRIPT_BUDGET_TOKENS: usize = 8000;

/// The most bytes those transcripts take, whatever their tokens: a launcher gives a
/// prompt to a harness as one argument, and an argument holds 128 KiB.
pub const TRANSCRIPT_MAX_BYTES: usize = 64 * 1024;

/// What `t0` answers when the T0 contents are off. It names the setting and holds
/// nothing of the record.
pub const T0_OFF: &str = "T0 is off for this session (the session setting `t0`): nothing of the record is listed here. The task tools and `write` work as before.\n";

/// The tools that serve what is below T0. A harness's call of one is refused when the
/// session's settings leave retrieval out.
pub const RETRIEVAL_TOOLS: &[&str] = &["search", "get", "list", "recall", "transcript_turn"];

/// How deep a session reads.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Disclosure {
    /// T0 and nothing below it: no search, no list, no body.
    T0,
    /// Summaries and bodies, as the read filter admits them.
    #[default]
    Full,
}

/// What a session's T0 holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum T0Contents {
    /// Nothing of the record.
    Off,
    /// The policy digest and how many tasks wait, and no entry.
    Digest,
    /// The handoffs, the decisions, the conventions, the policy digest and the pointers.
    #[default]
    Full,
}

/// The settings of one session. Each field narrows what the session reads; the default
/// is production, which narrows nothing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Memory access. Off: no entry is read by any path, and there is no T0 and no
    /// prefetch.
    pub memory: bool,
    /// The disclosure depth.
    pub depth: Disclosure,
    /// Whether the launcher puts the summaries a task likely needs in its prompt, and
    /// whether a `get` carries the summaries usually read with it.
    pub prefetch: bool,
    /// The T0 contents.
    pub t0: T0Contents,
    /// The highest trust tier read. Verified leaves nothing out.
    pub max_tier: TrustTier,
    /// The snapshot: an entry created later is not read, and a transcript kept later is
    /// not replayed.
    #[serde(
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub not_after: Option<OffsetDateTime>,
    /// Whether whole transcripts of earlier sessions are put in front of the prompt.
    pub transcripts: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            memory: true,
            depth: Disclosure::Full,
            prefetch: true,
            t0: T0Contents::Full,
            max_tier: TrustTier::Verified,
            not_after: None,
            transcripts: false,
        }
    }
}

impl Settings {
    /// Whether the session reads below T0: memory access is on and the depth is full.
    #[must_use]
    pub fn retrieves(&self) -> bool {
        self.memory && self.depth == Disclosure::Full
    }

    /// Whether summaries are pushed to the session: prefetch is on and it retrieves.
    #[must_use]
    pub fn prefetches(&self) -> bool {
        self.prefetch && self.retrieves()
    }

    /// What the session's T0 holds: nothing when memory access is off.
    #[must_use]
    pub fn t0_served(&self) -> T0Contents {
        if self.memory {
            self.t0
        } else {
            T0Contents::Off
        }
    }

    /// `granted` under the bounds of these settings: no tier above `max_tier`, and
    /// nothing created after the snapshot. Each bound is the narrower of the two, so
    /// the result admits nothing that `granted` does not.
    #[must_use]
    pub fn bounds(&self, mut granted: ReadFilter) -> ReadFilter {
        granted.max_tier = granted.max_tier.min(self.max_tier);
        granted.not_after = match (granted.not_after, self.not_after) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        granted
    }

    /// The filter every read of the session is under: `granted` under the bounds, and
    /// closed when the session does not retrieve.
    #[must_use]
    pub fn apply(&self, granted: ReadFilter) -> ReadFilter {
        let f = self.bounds(granted);
        if self.retrieves() {
            f
        } else {
            f.closed()
        }
    }

    /// Why retrieval is off, in words for the agent; none when the session retrieves.
    fn why_no_retrieval(&self) -> Option<&'static str> {
        if !self.memory {
            Some("memory access is off for this session")
        } else if self.depth == Disclosure::T0 {
            Some("the disclosure depth of this session is T0, so nothing below T0 is served")
        } else {
            None
        }
    }
}

/// An evaluation arm: one fixed value of the settings. The order is the paper's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Arm {
    /// The task and nothing of the record.
    NoMemory,
    /// No memory, and earlier sessions' transcripts in front of the prompt.
    TranscriptReplay,
    /// T0 and no retrieval.
    T0Only,
    /// The tasks that wait, the policy digest, and retrieval over raw entries.
    RawOnly,
    /// T0, retrieval and handoffs.
    Full,
}

impl Arm {
    /// Every arm, in the paper's order.
    pub const ALL: [Arm; 5] = [
        Arm::NoMemory,
        Arm::TranscriptReplay,
        Arm::T0Only,
        Arm::RawOnly,
        Arm::Full,
    ];

    /// The arm's name, as the command line and the tape write it.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Arm::NoMemory => "no-memory",
            Arm::TranscriptReplay => "transcript-replay",
            Arm::T0Only => "t0-only",
            Arm::RawOnly => "raw-only",
            Arm::Full => "full",
        }
    }

    /// What the arm stands for in the comparison.
    #[must_use]
    pub fn stands_for(self) -> &'static str {
        match self {
            Arm::NoMemory => "the floor",
            Arm::TranscriptReplay => "today's practice",
            Arm::T0Only => "what the cheap layer buys",
            Arm::RawOnly => "what curation buys",
            Arm::Full => "the system as built",
        }
    }

    /// The settings of the arm.
    #[must_use]
    pub fn settings(self) -> Settings {
        let no_memory = Settings {
            memory: false,
            prefetch: false,
            t0: T0Contents::Off,
            ..Settings::default()
        };
        match self {
            Arm::NoMemory => no_memory,
            Arm::TranscriptReplay => Settings {
                transcripts: true,
                ..no_memory
            },
            Arm::T0Only => Settings {
                depth: Disclosure::T0,
                prefetch: false,
                ..Settings::default()
            },
            Arm::RawOnly => Settings {
                max_tier: TrustTier::Raw,
                t0: T0Contents::Digest,
                ..Settings::default()
            },
            Arm::Full => Settings::default(),
        }
    }

    /// The arm whose settings these are, whatever the snapshot; none when the switches
    /// are no arm's.
    #[must_use]
    pub fn of(settings: &Settings) -> Option<Arm> {
        let switches = Settings {
            not_after: None,
            ..settings.clone()
        };
        Arm::ALL.into_iter().find(|a| a.settings() == switches)
    }
}

impl fmt::Display for Arm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl std::str::FromStr for Arm {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        Arm::ALL.into_iter().find(|a| a.name() == s).ok_or_else(|| {
            let names: Vec<&str> = Arm::ALL.iter().map(|a| a.name()).collect();
            format!("{s:?} is not an arm: {}", names.join(", "))
        })
    }
}

/// The settings a launcher gives a session from an arm and a snapshot, each as the
/// command line or the environment gave it; an empty value is none. None when neither
/// is given, so the session keeps the settings of production.
///
/// # Errors
///
/// A name that is no arm, or a snapshot that is not an RFC 3339 time.
pub fn from_flags(arm: Option<&str>, as_of: Option<&str>) -> Result<Option<Settings>, String> {
    let arm = arm.filter(|a| !a.is_empty());
    let as_of = as_of.filter(|t| !t.is_empty());
    if arm.is_none() && as_of.is_none() {
        return Ok(None);
    }
    let mut settings = match arm {
        Some(a) => a.parse::<Arm>()?.settings(),
        None => Settings::default(),
    };
    if let Some(t) = as_of {
        let at = OffsetDateTime::parse(t, &Rfc3339)
            .map_err(|e| format!("the snapshot {t:?} is not an RFC 3339 time: {e}"))?;
        settings.not_after = Some(at);
    }
    Ok(Some(settings))
}

/// What the lists of a T0 hold.
#[derive(Default)]
pub(crate) struct T0Lists {
    pub(crate) handoffs: Vec<Hit>,
    pub(crate) decisions: Vec<Hit>,
    pub(crate) conventions: Vec<Hit>,
    pub(crate) total: u64,
    pub(crate) due: usize,
}

impl T0Lists {
    /// The session has read what the lists hold: one read event for each domain.
    pub(crate) fn observed_by(&self, m: &Memory) -> Result<(), Error> {
        m.observe(
            &[
                &self.handoffs[..],
                &self.decisions[..],
                &self.conventions[..],
            ]
            .concat(),
        )
    }
}

fn one_transcript(t: &Transcript) -> String {
    let kept = t.kept_at.format(&Rfc3339).unwrap_or_default();
    let mut out = format!(
        "\n### Transcript {} ({}, {}, kept {kept}, {})\n",
        t.session, t.domain, t.harness, t.classification
    );
    for turn in &t.turns {
        out.push_str(&format!(
            "\n**{}** (turn {}):\n\n{}\n",
            turn.role,
            turn.turn,
            turn.text.trim_end()
        ));
    }
    out
}

const TRANSCRIPTS_HEAD: &str = "## Transcripts of earlier sessions\n\nWhole transcripts of earlier sessions, newest first. A transcript is its harness's own account of a session. Nobody curated or verified it.\n";

impl Memory {
    /// Give this session its settings. A launcher calls this once, before the session
    /// reads; nothing a session can call sets them.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] when the session has its settings already.
    pub fn set_settings(&self, settings: Settings) -> Result<(), Error> {
        self.settings.set(settings).map_err(|_| {
            Error::Denied("the settings of a session are set once, by its launcher".into())
        })
    }

    /// The settings of this session: production's when a launcher set none.
    #[must_use]
    pub fn settings(&self) -> Settings {
        self.settings.get().cloned().unwrap_or_default()
    }

    /// Whether a harness may call `tool` under this session's settings. The filter
    /// already serves nothing to a refused tool; the refusal is what tells the agent
    /// why.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`], naming the setting, for a retrieval tool in a session that
    /// does not retrieve.
    pub fn admits_tool(&self, tool: &str) -> Result<(), Error> {
        if !RETRIEVAL_TOOLS.contains(&tool) {
            return Ok(());
        }
        match self.settings().why_no_retrieval() {
            None => Ok(()),
            Some(why) => Err(Error::Denied(format!(
                "{why}: `{tool}` reads nothing. It is a session setting; the task tools and `write` work as before"
            ))),
        }
    }

    /// The filter T0 is read under, for one domain: the granted filter over the task
    /// chain, under the bounds of the settings. It is not closed when the depth is T0,
    /// since T0 is what that depth serves.
    pub(crate) fn t0_filter(&self, domain: &str) -> Result<ReadFilter, Error> {
        let f = self.settings().bounds(self.chained_filter()?);
        self.in_domain(f, Some(domain))
    }

    /// The filter the text of the policy's rules is read under: the granted filter over
    /// the task chain, or nothing when memory access is off. The policy in force is read
    /// as it is now, at every tier, so the tier ceiling and the snapshot do not apply to
    /// it.
    pub(crate) fn policy_filter(&self) -> Result<ReadFilter, Error> {
        self.serving()?;
        self.policy_scope()
    }

    /// [`Memory::policy_filter`] with no check for an outage: for a caller that reads
    /// nothing with the filter.
    pub(crate) fn policy_scope(&self) -> Result<ReadFilter, Error> {
        let f = self.chained_scope()?;
        Ok(if self.settings().memory {
            f
        } else {
            f.closed()
        })
    }

    /// The lists of a T0 under `f`: empty, and nothing read, unless the contents are
    /// full.
    pub(crate) async fn t0_lists(
        &self,
        f: &ReadFilter,
        contents: T0Contents,
    ) -> Result<T0Lists, Error> {
        if contents != T0Contents::Full {
            return Ok(T0Lists::default());
        }
        let ix = self.index.lock().await;
        let mut conventions = ix.list(f, Some(EntryType::Convention), 20)?;
        conventions.extend(ix.list(f, Some(EntryType::Method), 20)?);
        Ok(T0Lists {
            handoffs: ix.list(f, Some(EntryType::Handoff), 5)?,
            decisions: ix.list(f, Some(EntryType::Decision), 30)?,
            conventions,
            total: ix.count(f)?,
            due: ix
                .due_for_review(f, OffsetDateTime::now_utc().date(), 1000)?
                .len(),
        })
    }

    /// The text a harness is given for `prompt`: the transcripts of
    /// [`Memory::transcript_context`], when there are any, and then the prompt. A launch
    /// and a re-run of its tape both give the harness this text, so a re-run under the
    /// settings of the transcript-replay arm is a run of that arm.
    ///
    /// # Errors
    ///
    /// As [`Memory::transcript_context`].
    pub async fn prompt_given(&self, prompt: &str, budget_tokens: usize) -> Result<String, Error> {
        let replayed = self.transcript_context(None, budget_tokens).await?;
        Ok(if replayed.is_empty() {
            prompt.to_string()
        } else {
            format!("{replayed}\n{prompt}")
        })
    }

    /// The transcripts the transcript-replay arm puts in front of a prompt: empty
    /// unless the setting is on. Else the live transcripts of the domains this session's
    /// policy lets it read (of `domain` when one is named), at or below its handling
    /// and kept no later than the snapshot, newest first, as one Markdown section. A
    /// transcript is served whole or not at all: one that does not fit what is left of
    /// `budget_tokens`, or of [`TRANSCRIPT_MAX_BYTES`], is left out, and an older,
    /// shorter one may still fit. The session's label rises to the classification of
    /// each transcript served, and the log of its domain gets a `read` event that names
    /// the transcript by its session. An agent in production reads a transcript only by
    /// citation, one turn at a time; this is the one path that serves one whole, and
    /// only a launcher's setting opens it.
    ///
    /// # Errors
    ///
    /// A domain outside the grants or outside what the session reads, or a store error.
    pub async fn transcript_context(
        &self,
        domain: Option<&str>,
        budget_tokens: usize,
    ) -> Result<String, Error> {
        let settings = self.settings();
        if !settings.transcripts {
            return Ok(String::new());
        }
        let f = self.in_domain(settings.bounds(self.chained_filter()?), domain)?;
        let mut live = Vec::new();
        for d in self.domains() {
            if !f.domains.contains(&d) {
                continue;
            }
            for t in self.transcripts_in(&d).await? {
                let admitted = f.admits(&t.domain, t.classification, TrustTier::Raw, true)
                    && f.admits_at(t.kept_at);
                if t.state == State::Live && admitted {
                    live.push(t);
                }
            }
        }
        live.sort_by(|a, b| {
            b.kept_at
                .cmp(&a.kept_at)
                .then_with(|| a.session.cmp(&b.session))
        });
        let mut out = String::from(TRANSCRIPTS_HEAD);
        let mut tokens = approx_tokens(&out);
        let mut served = 0;
        for t in &live {
            let section = one_transcript(t);
            // the sum of two estimates is never under the estimate of the whole
            let cost = approx_tokens(&section);
            let fits =
                tokens + cost <= budget_tokens && out.len() + section.len() < TRANSCRIPT_MAX_BYTES;
            if !fits || !lock(&self.session).observe(t.classification) {
                continue;
            }
            // a transcript served is read: the log of its domain names it, as for a turn
            self.log_read(&t.domain, vec![t.session.clone()], t.classification)?;
            tokens += cost;
            out.push_str(&section);
            served += 1;
        }
        if served == 0 {
            return Ok(String::new());
        }
        out.push('\n');
        self.reads.text("transcripts", &out);
        Ok(out)
    }
}

#[cfg(test)]
mod tests;
