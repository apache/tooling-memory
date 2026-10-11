//! Transcripts are sources, not memory (the paper, Appendix B).
//!
//! A session's transcript is kept in the record, in the session's domain, beside the
//! entries and never as one: it is not indexed, not in T0, and no search, list or recall
//! returns it. It is reached only by citation, by turn: an entry distilled from it names
//! `transcript:<session>#<turn>` in its `source`, and [`Memory::transcript_turn`] serves
//! a turn some entry the reader may read cites, under the reader's filter, raising the
//! reader's label to the transcript's classification as a `get` does.
//!
//! Transcripts expire. Each carries a `retain_until` from its domain's clock
//! (`transcript_retention_days` in the grants, thirty days when unset). When it runs out,
//! [`Memory::sweep_transcripts`] keeps the turns live entries cite as excerpts, one per
//! citing entry, each carrying the turn's classification, and drops the rest; the drop is
//! a line in `state/transcripts.jsonl`. An excerpt belongs to its entry: it is served
//! only while the entry is, so it is demoted with it, it is erased with it, and an export
//! of the entry carries it. Erasing a transcript before its clock is the owner's act and
//! takes its turns and their excerpts alike, leaving a stub and a receipt.
//!
//! On disk, under the record:
//!
//! ```text
//! record/<domain>/.transcripts/<session>.json          the header and its turns
//! record/<domain>/.transcripts/<session>.jsonl         the harness's own lines, as left
//! record/<domain>/.transcripts/<session>.receipt.yaml  the receipt of an early erasure
//! record/<domain>/.excerpts/<entry>/<session>.<turn>.json   a turn an entry kept
//! ```

use crate::{lock, reads, Error, Memory};
use dm_core::erasure::{self, Reason, Receipt, Removed};
use dm_core::manifest::ManifestEntry;
use dm_core::{digest, Entry, Label, ReadFilter, TrustTier};
use dm_policy::decide::{Action, Input};
use dm_store::Area;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use time::{Duration, OffsetDateTime};

/// How a citation of a transcript begins in an entry's `source`.
pub const CITATION: &str = "transcript:";
/// Where sweeps and early erasures say what they dropped, under the root.
pub const LOG: &str = "state/transcripts.jsonl";
/// Who observed what a record holds: the harness, reporting on itself, as a transcript
/// does; a harness may leave out, reorder or invent what it did.
pub const OBSERVED_BY_HARNESS: &str = "harness";
/// The egress, in the path of every model call, as a decision context is.
pub const OBSERVED_BY_EGRESS: &str = "egress";

fn by_harness() -> String {
    OBSERVED_BY_HARNESS.to_string()
}

/// The widest range one citation may name, so a typo cannot ask for a billion turns.
const WIDEST: usize = 10_000;

/// One turn of a transcript: what a user or the assistant said, numbered from one in the
/// order the harness wrote them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Turn {
    /// Its number, from one.
    pub turn: usize,
    /// `user` or `assistant`.
    pub role: String,
    /// The text, tool calls named and their results elided.
    pub text: String,
}

/// Where a transcript is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// Whole, inside its clock.
    Live,
    /// Its clock ran out: the cited turns are excerpts, the rest is gone.
    Swept,
    /// Erased before its clock by the owner: a stub.
    Erased,
}

/// A transcript as the record keeps it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transcript {
    /// The id citations name: the memory session's, with `-2`, `-3` for a second and
    /// third transcript from one session.
    pub session: String,
    /// The domain of the session that produced it.
    pub domain: String,
    /// The harness that wrote it.
    pub harness: String,
    /// Where the harness left it, relative to its private home where it was under one.
    pub origin: String,
    /// The session's label at the end, at least the domain's default.
    pub classification: Label,
    /// When the record took it.
    #[serde(with = "time::serde::rfc3339")]
    pub kept_at: OffsetDateTime,
    /// When its clock runs out, from the domain's retention when it was kept.
    #[serde(with = "time::serde::rfc3339")]
    pub retain_until: OffsetDateTime,
    /// Live, swept or erased.
    pub state: State,
    /// When it was swept or erased.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub ended_at: Option<OffsetDateTime>,
    /// How many turns it had when kept.
    pub turn_count: usize,
    /// The turns, while live.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub turns: Vec<Turn>,
    /// After a sweep: each kept turn and the entries holding it as an excerpt.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub excerpts: BTreeMap<usize, Vec<String>>,
    /// Who observed it: always the harness, a transcript being its own account.
    #[serde(default = "by_harness")]
    pub observed_by: String,
}

impl Transcript {
    /// When its clock runs out under a domain clock of `days`: the earlier of what it
    /// was given and what the domain says now, so an owner who shortens the clock
    /// shortens it for what is already kept.
    #[must_use]
    pub fn expires(&self, days: u32) -> OffsetDateTime {
        self.retain_until
            .min(self.kept_at + Duration::days(i64::from(days)))
    }
}

/// A turn an entry kept when its transcript expired.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Excerpt {
    /// The transcript.
    pub session: String,
    /// The turn.
    pub turn: usize,
    /// Who said it.
    pub role: String,
    /// What was said.
    pub text: String,
    /// The turn's classification: the transcript's.
    pub classification: Label,
    /// The entry it belongs to.
    pub entry: String,
    /// When it was kept.
    #[serde(with = "time::serde::rfc3339")]
    pub kept_at: OffsetDateTime,
    /// Who observed it: the harness, as for the transcript it came from.
    #[serde(default = "by_harness")]
    pub observed_by: String,
}

/// What the launcher hands the record at the end of a session.
#[derive(Clone, Debug, Default)]
pub struct Keep {
    /// The session's domain; the only granted one when omitted.
    pub domain: Option<String>,
    /// The harness.
    pub harness: String,
    /// Where the harness left it.
    pub origin: String,
    /// The harness's own lines.
    pub raw: String,
    /// The turns, as `(role, text)`, in order.
    pub turns: Vec<(String, String)>,
}

/// What keeping a transcript did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Kept {
    /// The id to cite.
    pub session: String,
    /// Its domain.
    pub domain: String,
    /// Its classification.
    pub classification: Label,
    /// When its clock runs out.
    #[serde(with = "time::serde::rfc3339")]
    pub retain_until: OffsetDateTime,
    /// Its turns.
    pub turns: usize,
}

/// One turn, read by citation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TurnRead {
    /// The transcript.
    pub session: String,
    /// The turn.
    pub turn: usize,
    /// Who said it.
    pub role: String,
    /// What was said.
    pub text: String,
    /// Its classification, which the reader's label rose to.
    pub classification: Label,
    /// The readable entries that cite it, or the one whose excerpt this is.
    pub cited_by: Vec<String>,
    /// Whether it came from an excerpt, the transcript having expired.
    pub excerpt: bool,
    /// Who observed it: the harness, so a reader weighs it as a self-report.
    pub observed_by: String,
}

/// What a transcript looks like to the owner's `show`: the header, and which turns the
/// entries this session may read cite.
#[derive(Clone, Debug, Serialize)]
pub struct Shown {
    /// The header, turns left out.
    #[serde(flatten)]
    pub transcript: Transcript,
    /// Cited turns and the entries citing each, while live.
    pub cited: BTreeMap<usize, Vec<String>>,
}

/// What a sweep did to one transcript.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Swept {
    /// The transcript.
    pub session: String,
    /// Its domain.
    pub domain: String,
    /// Turns kept as excerpts.
    pub kept: Vec<usize>,
    /// Turns dropped.
    pub dropped: Vec<usize>,
    /// Excerpts written: one per citing entry and turn.
    pub excerpts: usize,
}

/// The citations of `session` for `turns`, runs of turns as one range:
/// `transcript:<session>#3`, `transcript:<session>#5-9`.
#[must_use]
pub fn cite(session: &str, turns: &[usize]) -> Vec<String> {
    let set: BTreeSet<usize> = turns.iter().copied().filter(|t| *t > 0).collect();
    let mut out = Vec::new();
    let mut run: Option<(usize, usize)> = None;
    let close = |(a, b): (usize, usize), out: &mut Vec<String>| {
        out.push(if a == b {
            format!("{CITATION}{session}#{a}")
        } else {
            format!("{CITATION}{session}#{a}-{b}")
        });
    };
    for t in set {
        run = match run {
            Some((a, b)) if t == b + 1 => Some((a, t)),
            Some(r) => {
                close(r, &mut out);
                Some((t, t))
            }
            None => Some((t, t)),
        };
    }
    if let Some(r) = run {
        close(r, &mut out);
    }
    out
}

/// The transcript and turns a source names, when it is a citation by turn:
/// `transcript:<session>#<n>`, `#<a>-<b>`, or several of those joined by commas. A
/// `transcript:` source without a turn, the path an older launcher wrote, is none.
#[must_use]
pub fn cited(source: &str) -> Option<(&str, BTreeSet<usize>)> {
    let rest = source.trim().strip_prefix(CITATION)?;
    let (session, spec) = rest.split_once('#')?;
    if session.is_empty() || session.contains('/') {
        return None;
    }
    let mut turns = BTreeSet::new();
    for part in spec.split(',') {
        let (a, b) = match part.trim().split_once('-') {
            Some((a, b)) => (a.trim().parse().ok()?, b.trim().parse().ok()?),
            None => {
                let n: usize = part.trim().parse().ok()?;
                (n, n)
            }
        };
        if a == 0 || b < a || b - a > WIDEST {
            return None;
        }
        turns.extend(a..=b);
    }
    Some((session, turns))
}

/// Turn numbers as ranges, for the log: `1-11,22-30`.
#[must_use]
pub fn ranges(turns: &[usize]) -> String {
    cite("", turns)
        .iter()
        .filter_map(|c| c.split_once('#').map(|(_, r)| r.to_string()))
        .collect::<Vec<_>>()
        .join(",")
}

fn header_name(session: &str) -> String {
    format!("{session}.json")
}

fn raw_name(session: &str) -> String {
    format!("{session}.jsonl")
}

fn receipt_name(session: &str) -> String {
    format!("{session}.receipt.yaml")
}

fn excerpt_name(session: &str, turn: usize) -> String {
    format!("{session}.{turn}.json")
}

/// The turn an excerpt's file name holds, when it is one of `session`'s.
fn excerpt_turn(name: &str, session: &str) -> Option<usize> {
    name.strip_prefix(session)?
        .strip_prefix('.')?
        .strip_suffix(".json")?
        .parse()
        .ok()
}

fn bad(what: &str, e: &serde_json::Error) -> Error {
    Error::Transcript(format!("{what}: {e}"))
}

fn rfc3339(t: OffsetDateTime) -> String {
    t.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// What every live entry in the granted domains cites, by transcript: the citing entry,
/// its domain, and the turns.
type Citations = BTreeMap<String, Vec<(String, String, BTreeSet<usize>)>>;

impl Memory {
    /// Keep a session's transcript in the record as a source: in the session's domain,
    /// classified at the session's label at the end and no lower than the domain's
    /// default, with a retention clock from the domain's grant. Keeping the same
    /// transcript again, as the session-end hook and mining both do, updates it in place
    /// while it is live and changes nothing once it has expired or been erased.
    ///
    /// # Errors
    ///
    /// An ungranted domain, or a store error.
    pub async fn keep_transcript(&self, keep: Keep) -> Result<Kept, Error> {
        let domain = self.resolve_domain(keep.domain.as_deref())?;
        let grant = self
            .grants
            .domains
            .get(&domain)
            .ok_or_else(|| Error::Denied(format!("domain {domain} is not granted")))?;
        let classification = {
            let s = lock(&self.session);
            self.grants
                .classification_for_write(&domain, None, &s)
                .unwrap_or(Label::Restricted)
        };
        let turns: Vec<Turn> = keep
            .turns
            .into_iter()
            .enumerate()
            .map(|(i, (role, text))| Turn {
                turn: i + 1,
                role,
                text,
            })
            .collect();
        let now = OffsetDateTime::now_utc();
        let t = match self.transcript_from(&domain, &keep.origin).await? {
            Some(old) if old.state != State::Live => old,
            Some(mut old) => {
                old.classification = old.classification.max(classification);
                old.turn_count = turns.len();
                old.turns = turns;
                self.put_transcript(&old, Some(&keep.raw)).await?;
                old
            }
            None => {
                let t = Transcript {
                    session: self.free_transcript_id(&domain).await?,
                    domain: domain.clone(),
                    harness: keep.harness,
                    origin: keep.origin,
                    classification,
                    kept_at: now,
                    retain_until: now
                        + Duration::days(i64::from(grant.transcript_retention_days())),
                    state: State::Live,
                    ended_at: None,
                    turn_count: turns.len(),
                    turns,
                    excerpts: BTreeMap::new(),
                    observed_by: by_harness(),
                };
                self.put_transcript(&t, Some(&keep.raw)).await?;
                t
            }
        };
        Ok(Kept {
            session: t.session,
            domain,
            classification: t.classification,
            retain_until: t.retain_until,
            turns: t.turn_count,
        })
    }

    /// This session's transcript from `origin`, if the record has it already.
    async fn transcript_from(
        &self,
        domain: &str,
        origin: &str,
    ) -> Result<Option<Transcript>, Error> {
        for t in self.transcripts_in(domain).await? {
            if self.is_ours(&t) && t.origin == origin {
                return Ok(Some(t));
            }
        }
        Ok(None)
    }

    /// Where the harness left each transcript this session kept, relative to its private
    /// home: the copies the record now holds, which the launcher may take out of the
    /// home at the session's end.
    ///
    /// # Errors
    ///
    /// A store error, or a header that does not parse.
    pub async fn kept_origins(&self) -> Result<Vec<String>, Error> {
        let mut out = Vec::new();
        for domain in self.domains() {
            for t in self.transcripts_in(&domain).await? {
                if self.is_ours(&t) && t.state == State::Live && !out.contains(&t.origin) {
                    out.push(t.origin);
                }
            }
        }
        Ok(out)
    }

    /// Whether this session kept `t`: its id is the session's, or the session's with `-n`.
    fn is_ours(&self, t: &Transcript) -> bool {
        t.session == self.session_id
            || t.session
                .strip_prefix(&self.session_id)
                .is_some_and(|r| r.starts_with('-'))
    }

    /// The id for this session's next transcript in `domain`.
    async fn free_transcript_id(&self, domain: &str) -> Result<String, Error> {
        let mut id = self.session_id.clone();
        let mut n = 1;
        while self
            .store
            .get_object(domain, Area::Transcripts, &[&header_name(&id)])
            .await?
            .is_some()
        {
            n += 1;
            id = format!("{}-{n}", self.session_id);
        }
        Ok(id)
    }

    async fn put_transcript(&self, t: &Transcript, raw: Option<&str>) -> Result<(), Error> {
        let text = serde_json::to_string_pretty(t).map_err(|e| bad(&t.session, &e))?;
        if let Some(raw) = raw {
            self.store
                .put_object(
                    &t.domain,
                    Area::Transcripts,
                    &[&raw_name(&t.session)],
                    raw.as_bytes().to_vec(),
                )
                .await?;
        }
        self.store
            .put_object(
                &t.domain,
                Area::Transcripts,
                &[&header_name(&t.session)],
                text.into_bytes(),
            )
            .await?;
        Ok(())
    }

    /// Every transcript header kept in a domain, whatever its state.
    async fn transcripts_in(&self, domain: &str) -> Result<Vec<Transcript>, Error> {
        let mut out = Vec::new();
        for name in self
            .store
            .objects(domain, Area::Transcripts, &[], false)
            .await?
        {
            let Some(session) = name.strip_suffix(".json") else {
                continue;
            };
            if let Some(t) = self.read_header(domain, session).await? {
                out.push(t);
            }
        }
        Ok(out)
    }

    async fn read_header(&self, domain: &str, session: &str) -> Result<Option<Transcript>, Error> {
        match self
            .store
            .get_object(domain, Area::Transcripts, &[&header_name(session)])
            .await?
        {
            None => Ok(None),
            Some(text) => serde_json::from_str(&text)
                .map(Some)
                .map_err(|e| bad(session, &e)),
        }
    }

    /// A live transcript's turns, whole: the decision contexts replay presents again.
    /// The owner's alone: replay reads what no agent may read whole, and only while the
    /// transcript is live, since the sweep keeps only the turns entries cite.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for anyone but the owner or a transcript no longer live;
    /// [`Error::NotFound`]; a store error.
    pub async fn replay_turns(&self, session: &str) -> Result<Vec<Turn>, Error> {
        if self.author != self.grants.owner {
            return Err(Error::Denied(format!(
                "replay is the owner's: {} reads whole transcripts",
                self.grants.owner
            )));
        }
        let t = self.find_transcript(session).await?;
        if t.state != State::Live {
            return Err(Error::Denied(format!(
                "{session} is {:?}: its decision contexts are gone, its cited turns kept as excerpts",
                t.state
            )));
        }
        Ok(t.turns)
    }

    /// A transcript by id across the granted domains, whatever its state.
    async fn find_transcript(&self, session: &str) -> Result<Transcript, Error> {
        for domain in self.domains() {
            if let Some(t) = self.read_header(&domain, session).await? {
                return Ok(t);
            }
        }
        Err(Error::NotFound(session.to_string()))
    }

    /// Every transcript kept in the granted domains, turns left out: the owner's view of
    /// what the record holds and when each runs out. Not a read of any of them.
    ///
    /// # Errors
    ///
    /// A store error, or a header that does not parse.
    pub async fn transcripts(&self) -> Result<Vec<Transcript>, Error> {
        let mut out = Vec::new();
        for domain in self.domains() {
            for mut t in self.transcripts_in(&domain).await? {
                t.turns.clear();
                out.push(t);
            }
        }
        Ok(out)
    }

    /// One transcript's header and which of its turns are cited, by the entries this
    /// session may read. Under the session's filter, and the label rises to the
    /// transcript's classification, as for a turn.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for a transcript that is not there or not for this session,
    /// or a store error.
    pub async fn transcript(&self, session: &str) -> Result<Shown, Error> {
        let f = self.session_filter()?;
        let mut t = self.readable_transcript(session, &f).await?;
        let mut cited = BTreeMap::new();
        if t.state == State::Live {
            for (entry, _, turns) in self
                .citations(&f)
                .await?
                .remove(session)
                .unwrap_or_default()
            {
                for n in turns.into_iter().filter(|n| *n <= t.turn_count) {
                    cited.entry(n).or_insert_with(Vec::new).push(entry.clone());
                }
            }
        }
        t.turns.clear();
        Ok(Shown {
            transcript: t,
            cited,
        })
    }

    /// The transcript, if the filter admits its domain and classification; the label
    /// rises to it.
    async fn readable_transcript(
        &self,
        session: &str,
        f: &ReadFilter,
    ) -> Result<Transcript, Error> {
        let t = self.find_transcript(session).await?;
        if !f.admits(&t.domain, t.classification, TrustTier::Raw, true)
            || !lock(&self.session).observe(t.classification)
        {
            return Err(Error::NotFound(session.to_string()));
        }
        Ok(t)
    }

    /// One turn of a transcript, by citation: served only when an entry this session
    /// may read cites it, under the session's filter, and the session's label rises to
    /// the transcript's classification as a `get` raises it to an entry's. Once the
    /// transcript has expired, the turn is served from the excerpt of a citing entry the
    /// session may read, which is how an excerpt is demoted and erased with its entry.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] when the transcript or turn is not there, is not cited by a
    /// readable entry, or is not for this session; a store error.
    pub async fn transcript_turn(&self, session: &str, turn: usize) -> Result<TurnRead, Error> {
        let f = self.session_filter()?;
        let header = self.find_transcript(session).await?;
        let found = match header.state {
            State::Live => self.live_turn(&header, turn, &f).await?,
            State::Swept => self.excerpt_turn(&header, turn, &f).await?,
            State::Erased => None,
        };
        found.ok_or_else(|| Error::NotFound(format!("{session}#{turn}")))
    }

    async fn live_turn(
        &self,
        t: &Transcript,
        turn: usize,
        f: &ReadFilter,
    ) -> Result<Option<TurnRead>, Error> {
        if !f.admits(&t.domain, t.classification, TrustTier::Raw, true) {
            return Ok(None);
        }
        let Some(found) = t.turns.iter().find(|x| x.turn == turn) else {
            return Ok(None);
        };
        let cited_by: Vec<String> = self
            .citations(f)
            .await?
            .remove(&t.session)
            .unwrap_or_default()
            .into_iter()
            .filter(|(_, _, turns)| turns.contains(&turn))
            .map(|(id, _, _)| id)
            .collect();
        if cited_by.is_empty() || !lock(&self.session).observe(t.classification) {
            return Ok(None);
        }
        Ok(Some(TurnRead {
            session: t.session.clone(),
            turn,
            role: found.role.clone(),
            text: found.text.clone(),
            classification: t.classification,
            cited_by,
            excerpt: false,
            observed_by: t.observed_by.clone(),
        }))
    }

    async fn excerpt_turn(
        &self,
        t: &Transcript,
        turn: usize,
        f: &ReadFilter,
    ) -> Result<Option<TurnRead>, Error> {
        for entry in t.excerpts.get(&turn).into_iter().flatten() {
            let meta = self.index.lock().await.meta(entry, f)?;
            let Some(meta) = meta else { continue };
            // the record decides, as for a get: an entry demoted or superseded since the
            // index last saw it takes its excerpts out of reach with it
            let owner = self.store.get(&meta.hit.domain, entry).await?;
            if !owner.is_some_and(|e| e.is_active() || f.include_inactive) {
                continue;
            }
            let Some(text) = self
                .store
                .get_object(
                    &meta.hit.domain,
                    Area::Excerpts,
                    &[entry, &excerpt_name(&t.session, turn)],
                )
                .await?
            else {
                continue;
            };
            let x: Excerpt = serde_json::from_str(&text).map_err(|e| bad(entry, &e))?;
            if !f.admits(&meta.hit.domain, x.classification, TrustTier::Raw, true)
                || !lock(&self.session).observe(x.classification)
            {
                continue;
            }
            return Ok(Some(TurnRead {
                session: x.session,
                turn,
                role: x.role,
                text: x.text,
                classification: x.classification,
                cited_by: vec![x.entry],
                excerpt: true,
                observed_by: x.observed_by,
            }));
        }
        Ok(None)
    }

    /// What the active entries `f` admits cite, by transcript, from the record.
    async fn citations(&self, f: &ReadFilter) -> Result<Citations, Error> {
        let mut out = Citations::new();
        for domain in self.domains() {
            for id in self.store.list(&domain).await? {
                let Some(e) = self.store.get(&domain, &id).await? else {
                    continue;
                };
                let front = &e.front;
                if !f.admits(
                    &domain,
                    front.classification,
                    front.trust_tier,
                    e.is_active(),
                ) {
                    continue;
                }
                add_citations(&mut out, &e, &domain);
            }
        }
        Ok(out)
    }

    /// Sweep the transcripts whose clock has run out at `now`: the turns live entries
    /// cite are kept as excerpts, one for each citing entry, carrying the turn's
    /// classification; the rest is dropped with the harness's lines, and the drop is a
    /// line in `state/transcripts.jsonl` naming the session, the turns kept and dropped,
    /// and when. Every live entry counts, whoever may read it: the sweep is the
    /// record's clock, not a session's read.
    ///
    /// # Errors
    ///
    /// A store error, or a header that does not parse.
    pub async fn sweep_transcripts(&self, now: OffsetDateTime) -> Result<Vec<Swept>, Error> {
        let mut everything = ReadFilter::for_domains(self.domains(), Label::Restricted);
        everything.include_inactive = false;
        let mut citations = self.citations(&everything).await?;
        let mut out = Vec::new();
        for domain in self.domains() {
            let days = self
                .grants
                .domains
                .get(&domain)
                .map_or(dm_policy::DEFAULT_TRANSCRIPT_RETENTION_DAYS, |g| {
                    g.transcript_retention_days()
                });
            for t in self.transcripts_in(&domain).await? {
                if t.state != State::Live || now < t.expires(days) {
                    continue;
                }
                let cites = citations.remove(&t.session).unwrap_or_default();
                out.push(self.sweep_one(t, &cites, now).await?);
            }
        }
        Ok(out)
    }

    async fn sweep_one(
        &self,
        mut t: Transcript,
        cites: &[(String, String, BTreeSet<usize>)],
        now: OffsetDateTime,
    ) -> Result<Swept, Error> {
        let mut excerpts = 0;
        for (entry, domain, turns) in cites {
            for turn in t.turns.iter().filter(|x| turns.contains(&x.turn)) {
                let x = Excerpt {
                    session: t.session.clone(),
                    turn: turn.turn,
                    role: turn.role.clone(),
                    text: turn.text.clone(),
                    classification: t.classification,
                    entry: entry.clone(),
                    kept_at: now,
                    observed_by: t.observed_by.clone(),
                };
                let text = serde_json::to_string_pretty(&x).map_err(|e| bad(entry, &e))?;
                self.store
                    .put_object(
                        domain,
                        Area::Excerpts,
                        &[entry, &excerpt_name(&t.session, turn.turn)],
                        text.into_bytes(),
                    )
                    .await?;
                t.excerpts.entry(turn.turn).or_default().push(entry.clone());
                excerpts += 1;
            }
        }
        let kept: Vec<usize> = t.excerpts.keys().copied().collect();
        let dropped: Vec<usize> = t
            .turns
            .iter()
            .map(|x| x.turn)
            .filter(|n| !t.excerpts.contains_key(n))
            .collect();
        t.state = State::Swept;
        t.ended_at = Some(now);
        t.turns.clear();
        self.put_transcript(&t, None).await?;
        self.store
            .remove_object(&t.domain, Area::Transcripts, &[&raw_name(&t.session)])
            .await?;
        self.log_transcript(serde_json::json!({
            "at": rfc3339(now), "action": "sweep", "session": t.session, "domain": t.domain,
            "kept": ranges(&kept), "dropped": ranges(&dropped),
            "kept_count": kept.len(), "dropped_count": dropped.len(), "excerpts": excerpts,
        }));
        Ok(Swept {
            session: t.session,
            domain: t.domain,
            kept,
            dropped,
            excerpts,
        })
    }

    fn log_transcript(&self, line: serde_json::Value) {
        reads::append_line(&self.root.join(LOG), &line.to_string());
    }

    /// Erase a transcript before its clock: its turns, the harness's lines and every
    /// excerpt of it leave the record; a stub, the header with no turns, and a receipt
    /// remain; and the entries that cite it are flagged for review, as an erased
    /// entry's citers are. The owner's act, and only once.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for a session that is not the owner or a transcript already
    /// erased, [`Error::NotFound`] for one in no granted domain, or a store error.
    pub async fn erase_transcript(&self, session: &str, reason: Reason) -> Result<Receipt, Error> {
        let mut t = self.find_transcript(session).await?;
        let d = self.decide(&Input {
            action: Action::Erase,
            principal: self.principal(),
            domain: t.domain.clone(),
            filing: None,
            task: None,
            parent: None,
            write: None,
            promote: None,
            held_task: None,
            spend: None,
            fetch: None,
        })?;
        if !d.allow || self.author != self.grants.owner {
            return Err(Error::Denied(format!(
                "only the owner {} may erase",
                self.grants.owner
            )));
        }
        let mut receipt = self.transcript_receipt(&t, reason).await?;
        self.store
            .remove_object(&t.domain, Area::Transcripts, &[&raw_name(session)])
            .await?;
        t.state = State::Erased;
        t.ended_at = Some(receipt.erased_at);
        t.turns.clear();
        t.excerpts.clear();
        self.put_transcript(&t, None).await?;
        self.remove_session_excerpts(session).await?;
        receipt.flagged = self.flag_citers(session).await?;
        receipt.unresolved = vec![
            "the harness's own copy in its private home under the root's state".into(),
            "exports issued before the erasure".into(),
        ];
        self.put_transcript_receipt(&receipt).await?;
        self.log_transcript(serde_json::json!({
            "at": rfc3339(receipt.erased_at), "action": "erase", "session": session,
            "domain": t.domain, "kept": "", "dropped": ranges(&(1..=t.turn_count).collect::<Vec<_>>()),
            "flagged": receipt.flagged,
        }));
        Ok(receipt)
    }

    /// The receipt to start an early erasure with, written before anything is removed;
    /// an unfinished one from a run that stopped short is picked up.
    async fn transcript_receipt(&self, t: &Transcript, reason: Reason) -> Result<Receipt, Error> {
        if let Some(text) = self
            .store
            .get_object(&t.domain, Area::Transcripts, &[&receipt_name(&t.session)])
            .await?
        {
            let r = Receipt::from_yaml(&text)?;
            if r.is_finished() {
                return Err(Error::Denied(format!("{} is already erased", t.session)));
            }
            return Ok(r);
        }
        let mut removed = Vec::new();
        for (name, version) in [
            (raw_name(&t.session), "transcript"),
            (header_name(&t.session), "turns"),
        ] {
            if let Some(text) = self
                .store
                .get_object(&t.domain, Area::Transcripts, &[&name])
                .await?
            {
                removed.push(Removed {
                    version: version.into(),
                    digest: digest(&text),
                    body_digest: None,
                });
            }
        }
        for (_, entry, x) in self.excerpts_of(&t.session).await? {
            removed.push(excerpt_removed(&entry, &x));
        }
        let pending = Receipt {
            id: t.session.clone(),
            domain: t.domain.clone(),
            erased_at: OffsetDateTime::now_utc(),
            erased_by: self.author.clone(),
            reason,
            removed,
            flagged: Vec::new(),
            unresolved: vec![erasure::UNFINISHED.into()],
        };
        self.put_transcript_receipt(&pending).await?;
        Ok(pending)
    }

    async fn put_transcript_receipt(&self, r: &Receipt) -> Result<(), Error> {
        self.store
            .put_object(
                &r.domain,
                Area::Transcripts,
                &[&receipt_name(&r.id)],
                r.to_yaml().into_bytes(),
            )
            .await?;
        Ok(())
    }

    /// The receipt of a transcript's early erasure, if it was erased.
    ///
    /// # Errors
    ///
    /// A store error, or a receipt that does not parse.
    pub async fn transcript_erasure(&self, session: &str) -> Result<Option<Receipt>, Error> {
        let t = self.find_transcript(session).await?;
        match self
            .store
            .get_object(&t.domain, Area::Transcripts, &[&receipt_name(session)])
            .await?
        {
            None => Ok(None),
            Some(text) => Ok(Some(Receipt::from_yaml(&text)?)),
        }
    }

    /// Every excerpt of a transcript in the granted domains, as `(domain, entry,
    /// excerpt)`.
    async fn excerpts_of(&self, session: &str) -> Result<Vec<(String, String, Excerpt)>, Error> {
        let mut out = Vec::new();
        for domain in self.domains() {
            for entry in self
                .store
                .objects(&domain, Area::Excerpts, &[], true)
                .await?
            {
                for name in self
                    .store
                    .objects(&domain, Area::Excerpts, &[&entry], false)
                    .await?
                {
                    if excerpt_turn(&name, session).is_none() {
                        continue;
                    }
                    if let Some(text) = self
                        .store
                        .get_object(&domain, Area::Excerpts, &[&entry, &name])
                        .await?
                    {
                        let x: Excerpt =
                            serde_json::from_str(&text).map_err(|e| bad(&entry, &e))?;
                        out.push((domain.clone(), entry.clone(), x));
                    }
                }
            }
        }
        Ok(out)
    }

    async fn remove_session_excerpts(&self, session: &str) -> Result<(), Error> {
        for (domain, entry, x) in self.excerpts_of(session).await? {
            self.store
                .remove_object(
                    &domain,
                    Area::Excerpts,
                    &[&entry, &excerpt_name(session, x.turn)],
                )
                .await?;
        }
        Ok(())
    }

    /// Erase an entry's versions in the record with `stub`, and the excerpts it holds
    /// first, so a run that stops between the two finds the entry still to erase.
    /// Returns the excerpts removed, for the receipt.
    pub(crate) async fn erase_with_excerpts(
        &self,
        domain: &str,
        id: &str,
        stub: &Entry,
        body: &str,
    ) -> Result<Vec<Removed>, Error> {
        let mut removed = self.erase_excerpts(domain, id).await?;
        // the decision contexts that carried the body: what a model was sent must not
        // keep what the record no longer holds
        for c in self.erase_from_contexts(body).await? {
            removed.push(Removed {
                version: format!("decision context {c}"),
                digest: digest(body),
                body_digest: Some(erasure::body_digest(body)),
            });
        }
        self.store.erase(domain, id, stub).await?;
        Ok(removed)
    }

    /// Erase the excerpts an entry holds, as its erasure takes them with it; returns
    /// what was removed, for its receipt.
    async fn erase_excerpts(&self, domain: &str, id: &str) -> Result<Vec<Removed>, Error> {
        let mut removed = Vec::new();
        for name in self
            .store
            .objects(domain, Area::Excerpts, &[id], false)
            .await?
        {
            if let Some(text) = self
                .store
                .get_object(domain, Area::Excerpts, &[id, &name])
                .await?
            {
                if let Ok(x) = serde_json::from_str::<Excerpt>(&text) {
                    removed.push(excerpt_removed(id, &x));
                }
            }
            self.store
                .remove_object(domain, Area::Excerpts, &[id, &name])
                .await?;
        }
        Ok(removed)
    }

    /// Write the excerpts of exported entries beside them in a bundle, as
    /// `excerpts/<entry>/<session>.<turn>.json`: those at or below the export's label
    /// and the entry's own, so nothing leaves below its label. Returns how many.
    pub(crate) async fn export_excerpts(
        &self,
        domain: &str,
        exported: &[ManifestEntry],
        max_label: Label,
        dest: &Path,
    ) -> Result<usize, Error> {
        let mut n = 0;
        for ManifestEntry {
            id,
            classification: label,
            ..
        } in exported
        {
            for name in self
                .store
                .objects(domain, Area::Excerpts, &[id], false)
                .await?
            {
                let Some(text) = self
                    .store
                    .get_object(domain, Area::Excerpts, &[id, &name])
                    .await?
                else {
                    continue;
                };
                let x: Excerpt = serde_json::from_str(&text).map_err(|e| bad(id, &e))?;
                if x.classification > max_label || x.classification > *label {
                    continue;
                }
                let dir = dest.join("excerpts").join(id);
                std::fs::create_dir_all(&dir).map_err(crate::io(&dir))?;
                let path = dir.join(&name);
                std::fs::write(&path, text).map_err(crate::io(&path))?;
                n += 1;
            }
        }
        Ok(n)
    }
}

fn excerpt_removed(entry: &str, x: &Excerpt) -> Removed {
    Removed {
        version: format!("excerpt {}#{} held by {entry}", x.session, x.turn),
        digest: digest(&x.text),
        body_digest: Some(erasure::body_digest(&x.text)),
    }
}

fn add_citations(out: &mut Citations, e: &Entry, domain: &str) {
    for s in &e.front.source {
        if let Some((session, turns)) = cited(s) {
            let list = out.entry(session.to_string()).or_default();
            match list.iter_mut().find(|(id, _, _)| *id == e.front.id) {
                Some((_, _, have)) => have.extend(turns),
                None => list.push((e.front.id.clone(), domain.to_string(), turns)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn citations_name_runs_of_turns_and_read_back() {
        assert_eq!(
            cite("sess-a", &[3, 5, 6, 7, 9, 3]),
            vec![
                "transcript:sess-a#3",
                "transcript:sess-a#5-7",
                "transcript:sess-a#9"
            ]
        );
        assert!(cite("sess-a", &[]).is_empty());
        let (s, turns) = cited("transcript:sess-a#5-7").unwrap();
        assert_eq!(s, "sess-a");
        assert_eq!(turns.into_iter().collect::<Vec<_>>(), vec![5, 6, 7]);
        let (_, turns) = cited("transcript:sess-a#2,4-5").unwrap();
        assert_eq!(turns.into_iter().collect::<Vec<_>>(), vec![2, 4, 5]);
        for not in [
            "transcript:/home/u/.claude/p/s.jsonl",
            "transcript:sess-a#0",
            "transcript:sess-a#9-3",
            "transcript:sess-a#1-99999",
            "transcript:#3",
            "https://example.org#3",
        ] {
            assert!(cited(not).is_none(), "{not}");
        }
        assert_eq!(ranges(&[1, 2, 3, 7, 9, 10]), "1-3,7,9-10");
        assert_eq!(excerpt_turn("sess-a.12.json", "sess-a"), Some(12));
        assert_eq!(excerpt_turn("sess-a-2.12.json", "sess-a"), None);
    }

    use crate::WriteRequest;
    use dm_core::{EntryType, Status};

    const TURNS: [(&str, &str); 5] = [
        ("user", "Where does the import spend its time?"),
        (
            "assistant",
            "In the fourth step, the zanzibar pass, which rereads every row.",
        ),
        ("user", "Can the quokka cache skip it?"),
        (
            "assistant",
            "Yes: with the quokka cache the fourth step reads each row once.",
        ),
        ("user", "Thanks, the walrus remark was a joke."),
    ];

    fn mem(dir: &Path) -> Memory {
        Memory::init(dir, "human:test:me", "dom-me").unwrap()
    }

    fn keep(origin: &str) -> Keep {
        Keep {
            domain: None,
            harness: "claude".into(),
            origin: origin.into(),
            raw: TURNS
                .iter()
                .map(|(r, t)| format!("{{\"type\":\"{r}\",\"message\":{{\"content\":\"{t}\"}}}}\n"))
                .collect(),
            turns: TURNS
                .iter()
                .map(|(r, t)| ((*r).to_string(), (*t).to_string()))
                .collect(),
        }
    }

    async fn citing(m: &Memory, session: &str, turns: &[usize], body: &str) -> String {
        m.write(WriteRequest {
            body: body.into(),
            entry_type: EntryType::Handoff,
            source: cite(session, turns),
            ..WriteRequest::default()
        })
        .await
        .unwrap()
        .id
    }

    /// Files under `dir` whose text holds `word`.
    fn holding(dir: &Path, word: &str) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for f in std::fs::read_dir(&d).unwrap().flatten() {
                let p = f.path();
                if p.is_dir() {
                    stack.push(p);
                } else if std::fs::read(&p)
                    .is_ok_and(|b| String::from_utf8_lossy(&b).contains(word))
                {
                    out.push(p);
                }
            }
        }
        out
    }

    fn clock_zero(dir: &Path) {
        let grants = dir.join("grants.toml");
        let text = std::fs::read_to_string(&grants).unwrap();
        let text = text.replace(
            "[domains.dom-me]\n",
            "[domains.dom-me]\ntranscript_retention_days = 0\n",
        );
        assert!(text.contains("transcript_retention_days = 0"), "{text}");
        std::fs::write(&grants, text).unwrap();
    }

    #[tokio::test]
    async fn a_transcript_is_kept_beside_the_entries_and_never_searched_listed_or_in_t0() {
        let dir = tempfile::tempdir().unwrap();
        let m = mem(dir.path());
        let k = m
            .keep_transcript(keep(".claude/projects/p/a.jsonl"))
            .await
            .unwrap();
        assert_eq!(k.session, m.session_id());
        assert_eq!(k.domain, "dom-me");
        assert_eq!(k.turns, 5);
        assert_eq!(
            k.classification,
            Label::Internal,
            "the domain's default at least"
        );
        let t = &m.transcripts().await.unwrap()[0];
        assert_eq!(t.retain_until - t.kept_at, Duration::days(30));
        assert!(dir
            .path()
            .join(format!("record/dom-me/.transcripts/{}.json", k.session))
            .is_file());
        assert!(dir
            .path()
            .join(format!("record/dom-me/.transcripts/{}.jsonl", k.session))
            .is_file());
        // kept again from the same place, the same transcript; from another, a second
        let again = m
            .keep_transcript(keep(".claude/projects/p/a.jsonl"))
            .await
            .unwrap();
        assert_eq!(again.session, k.session);
        let other = m
            .keep_transcript(keep(".codex/sessions/b.jsonl"))
            .await
            .unwrap();
        assert_eq!(other.session, format!("{}-2", k.session));
        citing(
            &m,
            &k.session,
            &[2],
            "Handoff: the import is slow in one step.\n",
        )
        .await;
        for word in ["zanzibar", "quokka", "walrus"] {
            assert!(m.search(word, None, 10).await.unwrap().is_empty(), "{word}");
        }
        assert_eq!(
            m.list(None, None, 100).await.unwrap().len(),
            1,
            "the entry only"
        );
        assert!(!m.t0(None).await.unwrap().contains("zanzibar"));
        m.reindex().await.unwrap();
        assert!(
            m.search("zanzibar", None, 10).await.unwrap().is_empty(),
            "nor after a rebuild"
        );
        assert_eq!(m.list(None, None, 100).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_turn_is_read_by_citation_and_raises_the_label() {
        let dir = tempfile::tempdir().unwrap();
        let m = mem(dir.path());
        let k = m.keep_transcript(keep("a.jsonl")).await.unwrap();
        assert!(
            matches!(
                m.transcript_turn(&k.session, 2).await,
                Err(Error::NotFound(_))
            ),
            "nothing cites it yet"
        );
        let h = citing(&m, &k.session, &[2], "Handoff: the import is slow.\n").await;
        let reader = Memory::open(dir.path()).unwrap();
        assert_eq!(reader.label(), Label::Public);
        let t = reader.transcript_turn(&k.session, 2).await.unwrap();
        assert!(t.text.contains("zanzibar"));
        assert_eq!(t.role, "assistant");
        assert_eq!(t.cited_by, vec![h]);
        assert!(!t.excerpt);
        assert_eq!(
            t.observed_by, OBSERVED_BY_HARNESS,
            "a transcript is a self-report"
        );
        assert_eq!(reader.label(), Label::Internal, "raised as a get raises it");
        for uncited in [1, 3, 9] {
            assert!(
                matches!(
                    reader.transcript_turn(&k.session, uncited).await,
                    Err(Error::NotFound(_))
                ),
                "turn {uncited}"
            );
        }
        let shown = reader.transcript(&k.session).await.unwrap();
        assert_eq!(shown.cited.keys().copied().collect::<Vec<_>>(), vec![2]);
        assert!(
            shown.transcript.turns.is_empty(),
            "show is the header, not the turns"
        );
        assert!(matches!(
            reader.transcript_turn("sess-none", 1).await,
            Err(Error::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn a_reader_whose_filter_excludes_the_classification_finds_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let m = mem(dir.path());
        let k = m.keep_transcript(keep("a.jsonl")).await.unwrap();
        // a public handoff citing an internal transcript: the reader sees the entry,
        // never the turn
        m.write(WriteRequest {
            body: "Handoff: see the transcript.\n".into(),
            entry_type: EntryType::Handoff,
            classification: Some(Label::Public),
            source: cite(&k.session, &[2]),
            ..WriteRequest::default()
        })
        .await
        .unwrap();
        let reader = Memory::open(dir.path())
            .unwrap()
            .with_handling_cap(Label::Public);
        assert_eq!(
            reader.list(None, None, 10).await.unwrap().len(),
            1,
            "the entry is public"
        );
        assert!(matches!(
            reader.transcript_turn(&k.session, 2).await,
            Err(Error::NotFound(_))
        ));
        assert!(matches!(
            reader.transcript(&k.session).await,
            Err(Error::NotFound(_))
        ));
        assert_eq!(
            reader.label(),
            Label::Public,
            "nothing read, nothing raised"
        );
    }

    /// A record whose transcript has been swept: entries A and B cite it, C did and was
    /// superseded; returns the root, the memory, the session, A and B, and the sweep.
    async fn swept() -> (
        tempfile::TempDir,
        Memory,
        String,
        String,
        String,
        Vec<Swept>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let m = mem(dir.path());
        let k = m.keep_transcript(keep("a.jsonl")).await.unwrap();
        let a = citing(&m, &k.session, &[2], "Handoff A.\n").await;
        let b = citing(&m, &k.session, &[2, 3], "Handoff B.\n").await;
        let c = citing(&m, &k.session, &[5], "Handoff C, superseded.\n").await;
        m.write(WriteRequest {
            supersedes: Some(c.clone()),
            ..WriteRequest {
                body: "Handoff C again, citing nothing.\n".into(),
                ..WriteRequest::default()
            }
        })
        .await
        .unwrap();
        assert!(
            m.sweep_transcripts(OffsetDateTime::now_utc())
                .await
                .unwrap()
                .is_empty(),
            "inside its thirty days"
        );
        clock_zero(dir.path());
        let m = Memory::open(dir.path()).unwrap();
        let swept = m
            .sweep_transcripts(OffsetDateTime::now_utc())
            .await
            .unwrap();
        (dir, m, k.session, a, b, swept)
    }

    #[tokio::test]
    async fn the_sweep_keeps_cited_turns_as_excerpts_and_drops_the_rest_with_a_record() {
        let (dir, m, session, a, b, swept) = swept().await;
        assert_eq!(swept.len(), 1);
        assert_eq!(swept[0].kept, vec![2, 3]);
        assert_eq!(
            swept[0].dropped,
            vec![1, 4, 5],
            "a superseded entry keeps nothing"
        );
        assert_eq!(swept[0].excerpts, 3, "turn 2 for A and B, turn 3 for B");
        let rec = dir.path().join("record/dom-me");
        for (entry, turn) in [(&a, 2), (&b, 2), (&b, 3)] {
            let x = rec.join(format!(".excerpts/{entry}/{}.{turn}.json", session));
            assert!(x.is_file(), "{}", x.display());
            assert!(std::fs::read_to_string(&x)
                .unwrap()
                .contains("\"internal\""));
        }
        assert!(!rec.join(format!(".transcripts/{}.jsonl", session)).exists());
        let log = std::fs::read_to_string(dir.path().join(LOG)).unwrap();
        assert!(log.contains("\"action\":\"sweep\""), "{log}");
        assert!(
            log.contains("\"kept\":\"2-3\"") && log.contains("\"dropped\":\"1,4-5\""),
            "{log}"
        );
        // what no entry cited is gone from the record; what one did is in its excerpts
        for dropped in ["walrus", "Where does the import"] {
            assert!(holding(&rec, dropped).is_empty(), "{dropped}");
        }
        assert_eq!(
            holding(&rec, "zanzibar").len(),
            2,
            "one excerpt each for A and B"
        );
        let reader = Memory::open(dir.path()).unwrap();
        let t = reader.transcript_turn(&session, 2).await.unwrap();
        assert!(t.excerpt && t.text.contains("zanzibar"));
        assert_eq!(t.observed_by, OBSERVED_BY_HARNESS);
        assert_eq!(reader.label(), Label::Internal);
        assert!(matches!(
            reader.transcript_turn(&session, 1).await,
            Err(Error::NotFound(_))
        ));
        assert!(
            m.sweep_transcripts(OffsetDateTime::now_utc())
                .await
                .unwrap()
                .is_empty(),
            "once"
        );
    }

    #[tokio::test]
    async fn an_excerpt_is_demoted_and_exported_with_its_entry() {
        let (dir, m, session, a, b, _) = swept().await;
        // an excerpt is demoted with its entry: superseding B takes turn 3 out of reach
        m.write(WriteRequest {
            supersedes: Some(b.clone()),
            body: "Handoff B, rewritten.\n".into(),
            ..WriteRequest::default()
        })
        .await
        .unwrap();
        let reader = Memory::open(dir.path()).unwrap();
        assert!(matches!(
            reader.transcript_turn(&session, 3).await,
            Err(Error::NotFound(_))
        ));
        assert_eq!(
            reader.transcript_turn(&session, 2).await.unwrap().cited_by,
            vec![a]
        );
        // and travels with it in an export
        let out = dir.path().join("bundle");
        m.export(None, Label::Internal, &out).await.unwrap();
        assert!(out.join("excerpts").is_dir());
        assert_eq!(holding(&out.join("excerpts"), "zanzibar").len(), 2);
        let public = dir.path().join("public");
        m.export(None, Label::Public, &public).await.unwrap();
        assert!(
            !public.join("excerpts").exists(),
            "nothing leaves below its label"
        );
    }

    #[tokio::test]
    async fn erasing_an_entry_erases_its_excerpts() {
        let dir = tempfile::tempdir().unwrap();
        clock_zero(&{
            mem(dir.path());
            dir.path().to_path_buf()
        });
        let m = Memory::open(dir.path()).unwrap();
        let k = m.keep_transcript(keep("a.jsonl")).await.unwrap();
        let a = citing(&m, &k.session, &[2], "Handoff A.\n").await;
        let b = citing(&m, &k.session, &[2], "Handoff B.\n").await;
        m.sweep_transcripts(OffsetDateTime::now_utc())
            .await
            .unwrap();
        let receipt = m.erase(&a, Reason::Mistaken).await.unwrap();
        assert!(
            receipt
                .removed
                .iter()
                .any(|r| r.version.starts_with("excerpt ")),
            "{:?}",
            receipt.removed
        );
        let rec = dir.path().join("record/dom-me");
        assert!(!rec
            .join(format!(".excerpts/{a}/{}.2.json", k.session))
            .exists());
        assert_eq!(holding(&rec, "zanzibar").len(), 1, "B's is its own");
        assert_eq!(
            m.transcript_turn(&k.session, 2).await.unwrap().cited_by,
            vec![b.clone()]
        );
        m.erase(&b, Reason::Mistaken).await.unwrap();
        assert!(holding(&rec, "zanzibar").is_empty());
        assert!(matches!(
            m.transcript_turn(&k.session, 2).await,
            Err(Error::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn early_erasure_leaves_a_stub_and_a_receipt_and_flags_the_citers() {
        let dir = tempfile::tempdir().unwrap();
        let m = mem(dir.path());
        let k = m.keep_transcript(keep("a.jsonl")).await.unwrap();
        let h = citing(&m, &k.session, &[2, 4], "Handoff: the cache helps.\n").await;
        let agent = Memory::open(dir.path()).unwrap().with_author("agent:test");
        assert!(matches!(
            agent.erase_transcript(&k.session, Reason::Sensitive).await,
            Err(Error::Denied(_))
        ));
        let r = m
            .erase_transcript(&k.session, Reason::Sensitive)
            .await
            .unwrap();
        assert!(r.is_finished());
        assert_eq!(r.id, k.session);
        assert_eq!(r.flagged, vec![h.clone()]);
        let versions: Vec<&str> = r.removed.iter().map(|v| v.version.as_str()).collect();
        assert_eq!(versions, vec!["transcript", "turns"]);
        assert_eq!(m.transcript_erasure(&k.session).await.unwrap(), Some(r));
        let stub = &m.transcripts().await.unwrap()[0];
        assert_eq!(stub.state, State::Erased);
        assert_eq!(stub.turn_count, 5, "the stub says what there was");
        let rec = dir.path().join("record");
        for word in ["zanzibar", "quokka", "walrus"] {
            assert!(holding(&rec, word).is_empty(), "{word}");
        }
        assert!(matches!(
            m.transcript_turn(&k.session, 2).await,
            Err(Error::NotFound(_))
        ));
        let flagged = m.get(&h).await.unwrap();
        assert_eq!(
            flagged.front.review_by,
            Some(OffsetDateTime::now_utc().date())
        );
        assert_ne!(
            flagged.front.status,
            Some(Status::Erased),
            "flagged, not erased"
        );
        assert!(matches!(
            m.erase_transcript(&k.session, Reason::Sensitive).await,
            Err(Error::Denied(_))
        ));
        let log = std::fs::read_to_string(dir.path().join(LOG)).unwrap();
        assert!(log.contains("\"action\":\"erase\""), "{log}");
        // kept again from the same place, an erased transcript stays erased
        let again = m.keep_transcript(keep("a.jsonl")).await.unwrap();
        assert_eq!(again.session, k.session);
        assert!(holding(&rec, "zanzibar").is_empty());
    }
}
