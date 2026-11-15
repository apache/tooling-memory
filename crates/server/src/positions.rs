//! Positions: the record for a standing role, and the session that fills one.
//!
//! A position is an entry of type `position` whose body holds one fenced block. The
//! info string is `position <name>`, the name is `pos-` and then lower-case letters,
//! digits and hyphens, and the text is TOML:
//!
//! ````text
//! ```position pos-release
//! [charter]
//! orders = "Each release is cut from main, after the mirrors are checked."
//! covers = ["release"]
//! agent = "codex"
//!
//! [envelope]
//! tools = ["tool:fetch"]
//! handling = "internal"
//! ```
//! ````
//!
//! The name is the position's identifier for good, and the entry's id is its version.
//! The charter gives the standing orders, the task definitions the position takes, and
//! the agent definition that fills it. The authority envelope gives the tools a session
//! in the position may use and the most it reads. A retiring version says
//! `status = "retired"` above `[charter]`.
//!
//! The owner retires a position through [`Memory::retire_position`]: the tasks pinned
//! to it then return to matching, and its history stays under its name.
//! [`Memory::positions`] lists the positions with what waits on each, and
//! [`Memory::wake_list`] names the ones that need a session started.
//!
//! A position is in force only from an active entry a person verified, the newest one,
//! as a task definition is. A session fills a position through
//! [`Memory::fill_position`]: it then claims the tasks pinned to the position and the
//! unpinned ones its charter covers, uses no tool and reads no entry outside the
//! envelope, and gets the position's slice in T0. The position's history is the
//! entries whose `cites` hold its name, and only a session that fills it under the
//! charter's agent definition writes one. A session under another agent definition is
//! seconded: it reads what the position holds and writes to its task.
//!
//! A commitment of a position is a task pinned to it that is admitted and not closed.
//! Any session files a task pinned to a position it may read, and that task stays
//! proposed until a person admits it, or a session that fills the position under the
//! charter's agent definition does: what the next occupant reads first is never the
//! word of a session outside the position alone.
//!
//! A session acts under the version of the position it filled. Once the record
//! resolves the name to another entry, or to none, the session's claims, reads and
//! tool calls are refused as stale, and what is left to it is to write its handoff.
//!
//! Two limits, both of scale and of import: an imported bundle lands its entries with
//! their citations, so it can bring entries that cite a position; and the handoff
//! cache walks the newest [`CACHE_WALK`] handoffs of the domain at each read.

use crate::{lock, Error, Memory, WriteRequest};
use dm_core::{Entry, EntryType, Label, ReadFilter, TrustTier};
use dm_index::Hit;
use dm_ledger::{Status, Task, TaskSpec};
use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
use time::OffsetDateTime;

/// What the name of every position starts with. A cite that starts with it names a
/// position.
pub const PREFIX: &str = "pos-";

/// How many handoffs the cache holds.
pub const CACHE_SIZE: usize = 5;

/// How many of the domain's newest handoffs a read of the cache walks.
pub const CACHE_WALK: usize = 200;

/// How many open commitments a slice or a view lists.
pub const COMMITMENTS: usize = 10;

/// How many characters of the charter's orders the T0 slice carries.
const ORDERS_IN_T0: usize = 1200;

/// How many characters of a commitment's title the T0 slice carries.
const TITLE_IN_T0: usize = 160;

/// A position's charter: its standing orders, what it covers, and who fills it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Charter {
    /// The standing orders.
    #[serde(default)]
    pub orders: String,
    /// The names of the task definitions the position takes when a task is not pinned.
    #[serde(default)]
    pub covers: Vec<String>,
    /// The agent definition that fills the position by default.
    pub agent: String,
}

/// A position's authority envelope: the most a session in the position may do.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    /// The tools a session in the position may use, as a task's requirements name them
    /// (`tool:fetch`).
    #[serde(default)]
    pub tools: Vec<String>,
    /// The most a session in the position reads.
    pub handling: Label,
}

/// Whether a position takes sessions and pins.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// In force.
    #[default]
    Active,
    /// Retired by its owner: its history stays, and nothing new goes to it.
    Retired,
}

impl State {
    fn is_active(&self) -> bool {
        *self == State::Active
    }
}

/// The block as it is written.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Block {
    #[serde(default, skip_serializing_if = "State::is_active")]
    status: State,
    charter: Charter,
    envelope: Envelope,
}

/// A position, as one entry defines it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Position {
    /// Its name: its identifier for good.
    pub name: String,
    /// The entry it is in: its version.
    pub entry: String,
    /// The domain of that entry.
    pub domain: String,
    /// The classification of that entry.
    pub classification: Label,
    /// Whether it is in force or retired.
    pub state: State,
    /// The charter.
    pub charter: Charter,
    /// The authority envelope.
    pub envelope: Envelope,
}

impl Position {
    /// The fenced block that defines this position, as an entry's body holds it.
    ///
    /// # Errors
    ///
    /// When the block does not serialize.
    pub fn block(&self) -> Result<String, Error> {
        let text = toml::to_string(&Block {
            status: self.state,
            charter: self.charter.clone(),
            envelope: self.envelope.clone(),
        })
        .map_err(|e| Error::Denied(format!("position {} does not render: {e}", self.name)))?;
        Ok(format!("```position {}\n{text}```\n", self.name))
    }
}

/// Whether `name` is the name of a position: `pos-` and then one or more lower-case
/// letters, digits or hyphens.
#[must_use]
pub fn is_name(name: &str) -> bool {
    name.strip_prefix(PREFIX).is_some_and(|rest| {
        !rest.is_empty()
            && rest
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    })
}

/// The agent definition a session runs under: the name in its author, `agent:<name>`
/// up to the first white space. A person has none.
#[must_use]
pub fn agent_definition(author: &str) -> Option<&str> {
    author
        .strip_prefix("agent:")
        .and_then(|rest| rest.split_whitespace().next())
}

/// A `position <name>` block of a body.
struct Fenced {
    /// The name on its fence.
    name: String,
    /// Its text: its lines, as [`str::lines`] gives them.
    text: String,
    /// Where its lines are in the body: from its first line to its closing fence.
    at: std::ops::Range<usize>,
}

/// A line of a body without its line ending, as [`str::lines`] gives it.
fn without_ending(raw: &str) -> &str {
    match raw.strip_suffix('\n') {
        Some(line) => line.strip_suffix('\r').unwrap_or(line),
        None => raw,
    }
}

/// The `position <name>` blocks of a body. A fence inside a longer fence is text of
/// that one, and opens nothing.
fn blocks_in(body: &str) -> Vec<Fenced> {
    let mut out = Vec::new();
    // the fence that is open: its length, the name when it is a position's, where its
    // first line starts, its lines
    let mut open: Option<(usize, Option<String>, usize, Vec<&str>)> = None;
    let mut next = 0;
    for raw in body.split_inclusive('\n') {
        let start = next;
        next += raw.len();
        let line = without_ending(raw);
        let trimmed = line.trim_start();
        let ticks = trimmed.chars().take_while(|c| *c == '`').count();
        let rest = &trimmed[ticks..];
        match &mut open {
            None if ticks >= 3 => {
                let mut words = rest.split_whitespace();
                let name = match (words.next(), words.next(), words.next()) {
                    (Some("position"), Some(name), None) => Some(name.to_string()),
                    _ => None,
                };
                open = Some((ticks, name, next, Vec::new()));
            }
            None => {}
            Some((fence, name, first, lines)) => {
                if ticks >= *fence && rest.trim().is_empty() {
                    if let Some(name) = name.take() {
                        out.push(Fenced {
                            name,
                            text: lines.join("\n"),
                            at: *first..start,
                        });
                    }
                    open = None;
                } else {
                    lines.push(line);
                }
            }
        }
    }
    out
}

/// The one `position` block of a body, when it reads. The block reads when its name is
/// a position's, it has no key this code does not know, its agent is one word, and
/// every tool is `tool:<name>`.
fn block_in(body: &str) -> Option<(Fenced, Block)> {
    let mut blocks = blocks_in(body);
    let fenced = blocks.pop().filter(|_| blocks.is_empty())?;
    let block: Block = toml::from_str(&fenced.text).ok()?;
    let agent = &block.charter.agent;
    let reads = is_name(&fenced.name)
        && !agent.is_empty()
        && !agent.contains(char::is_whitespace)
        && block
            .envelope
            .tools
            .iter()
            .all(|t| t.starts_with("tool:") && t.len() > 5);
    reads.then_some((fenced, block))
}

/// The position an entry defines: an entry of type `position` with one `position`
/// block that reads. Whether the entry is in force is the caller's to ask.
#[must_use]
pub fn position_in(e: &Entry) -> Option<Position> {
    if e.front.entry_type != EntryType::Position {
        return None;
    }
    let (fenced, block) = block_in(&e.body)?;
    Some(Position {
        name: fenced.name,
        entry: e.front.id.clone(),
        domain: e.front.domain.clone(),
        classification: e.front.classification,
        state: block.status,
        charter: block.charter,
        envelope: block.envelope,
    })
}

/// The body of a position's entry with the `status` of its block set, and everything
/// else of the body byte for byte. The status is a line above the block's first table:
/// a retiring version gets `status = "retired"` as the block's first line, and an
/// active one has no such line.
///
/// # Errors
///
/// A body with no one position block that reads; and a result that does not read back
/// as the same record with the new status, which is how a block whose status is not
/// one line of its own is refused.
pub fn with_status(body: &str, status: State) -> Result<String, String> {
    let (fenced, old) = block_in(body).ok_or("the body holds no one position block that reads")?;
    if old.status == status {
        return Ok(body.to_string());
    }
    let mut kept = String::new();
    let mut above_tables = true;
    for line in body[fenced.at.clone()].split_inclusive('\n') {
        let key = line.trim_start();
        above_tables = above_tables && !key.starts_with('[');
        let sets_status = key
            .strip_prefix("status")
            .is_some_and(|rest| rest.trim_start().starts_with('='));
        if !(above_tables && sets_status) {
            kept.push_str(line);
        }
    }
    let set = match status {
        State::Active => "",
        State::Retired => "status = \"retired\"\n",
    };
    let (before, after) = (&body[..fenced.at.start], &body[fenced.at.end..]);
    let out = format!("{before}{set}{kept}{after}");
    match block_in(&out) {
        Some((now, block))
            if now.name == fenced.name
                && block.status == status
                && block.charter == old.charter
                && block.envelope == old.envelope =>
        {
            Ok(out)
        }
        _ => Err(
            "the position block does not read back as the same record with the new status".into(),
        ),
    }
}

/// The record among the active entries that define one name, newest first: the newest
/// verified one, active or retired as its block says; when none is verified, the
/// newest retiring one the owner wrote, since only the owner replaces a verified entry.
fn in_force<'a>(candidates: &[(&'a Entry, &'a Position)], owner: &str) -> Option<&'a Position> {
    let verified = candidates
        .iter()
        .find(|(e, _)| e.front.trust_tier == TrustTier::Verified);
    if let Some((_, p)) = verified {
        return Some(p);
    }
    candidates
        .iter()
        .find(|(e, p)| p.state == State::Retired && e.front.author == owner)
        .map(|(_, p)| *p)
}

/// The entries of `entries` that define the position `name`, in the order given.
fn versions_of<'a>(
    entries: &'a [(Entry, Option<Position>)],
    name: &str,
) -> Vec<(&'a Entry, &'a Position)> {
    entries
        .iter()
        .filter_map(|(e, p)| Some((e, p.as_ref().filter(|p| p.name == name)?)))
        .collect()
}

/// The position a session fills: a copy, taken when the session filled it.
#[derive(Clone, Debug)]
pub(crate) struct Seated {
    /// The position as it was in force then.
    pub(crate) position: Position,
    /// Whether the session runs under another agent definition than the charter's.
    pub(crate) seconded: bool,
}

impl Seated {
    /// The seat as the ledger matches a claim against it.
    pub(crate) fn seat(&self) -> dm_ledger::Seat<'_> {
        dm_ledger::Seat {
            position: &self.position.name,
            covers: &self.position.charter.covers,
        }
    }

    fn filled(&self) -> Filled {
        Filled {
            position: self.position.name.clone(),
            domain: self.position.domain.clone(),
            entry: self.position.entry.clone(),
            agent: self.position.charter.agent.clone(),
            seconded: self.seconded,
        }
    }

    /// Whether this is the seat a write in `domain` goes to the history of.
    fn writes_history_in(&self, domain: &str) -> bool {
        !self.seconded && self.position.domain == domain
    }
}

/// What filling a position gave a session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Filled {
    /// The position's name.
    pub position: String,
    /// Its domain.
    pub domain: String,
    /// The entry that defines it: the version the session filled.
    pub entry: String,
    /// The agent definition its charter names.
    pub agent: String,
    /// Whether the session runs under another agent definition, and so reads the
    /// position and writes to its task only.
    pub seconded: bool,
}

/// A handoff in a position's cache.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CachedHandoff {
    /// The entry.
    pub id: String,
    /// Its title.
    pub title: String,
    /// Its summary.
    pub summary: String,
}

/// This session's own place in a position.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct SeatView {
    /// Whether it is seconded.
    pub seconded: bool,
}

/// A position as a session reads it.
#[derive(Clone, Debug, Serialize)]
pub struct PositionView {
    /// The record.
    #[serde(flatten)]
    pub position: Position,
    /// Its open commitments: the tasks pinned to it that are admitted and not closed,
    /// and that this session may see.
    pub commitments: Vec<Task>,
    /// Its handoff cache, newest first.
    pub handoffs: Vec<CachedHandoff>,
    /// The sessions that hold a task they claimed from it, under a live lease, of the
    /// tasks this session may see.
    pub sessions: Vec<String>,
    /// This session's seat, when it fills the position.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seat: Option<SeatView>,
}

/// What a retirement did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Retired {
    /// The position's name.
    pub position: String,
    /// Its domain.
    pub domain: String,
    /// The version that was retired: the entry the retiring version replaced, or the
    /// retiring version itself when it replaced none.
    pub entry: String,
    /// The retiring version: the entry the name resolves to now.
    pub retired_by: String,
    /// The tasks this call took the pin off, in queue order.
    pub unpinned: Vec<String>,
}

/// What a listed entry is to the position it names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Standing {
    /// It is the record in force of its name, and the position takes sessions and pins.
    Active,
    /// It is the record in force of its name, and the position is retired.
    Retired,
    /// It is not the record in force of its name.
    #[serde(rename = "none")]
    NotInForce,
}

/// An entry of type `position`, as [`Memory::positions`] lists it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ListedPosition {
    /// The position's name; empty when the entry's block does not read.
    pub name: String,
    /// The entry.
    pub entry: String,
    /// Its domain.
    pub domain: String,
    /// What the entry is to its name.
    pub state: Standing,
    /// The agent definition its charter names; empty when the block does not read.
    pub agent: String,
    /// How many tasks that are not closed are pinned to the position, a proposed one
    /// included, of those this session may see.
    pub pinned: usize,
    /// Those of them a session could claim now, in queue order: an open one, and a
    /// claimed one whose holder has no live lease on a task from the position.
    pub waiting: Vec<String>,
    /// The sessions that hold a task they claimed from the position, under a live
    /// lease, of the tasks this session may see.
    pub sessions: Vec<String>,
    /// Whether the position needs a session started: it is active, a session can fill
    /// it, a task waits, and no session is live in it.
    pub wake: bool,
    /// Why the entry is not the record in force of its name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why_not: Option<String>,
}

impl ListedPosition {
    /// The row of an entry that is not the record in force of a name.
    fn not_in_force(e: &Entry, p: Option<&Position>, why: String) -> ListedPosition {
        ListedPosition {
            name: p.map(|p| p.name.clone()).unwrap_or_default(),
            entry: e.front.id.clone(),
            domain: e.front.domain.clone(),
            state: Standing::NotInForce,
            agent: p.map(|p| p.charter.agent.clone()).unwrap_or_default(),
            pinned: 0,
            waiting: Vec::new(),
            sessions: Vec::new(),
            wake: false,
            why_not: Some(why),
        }
    }
}

/// A position that needs a session started.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Wake {
    /// The position's name.
    pub position: String,
    /// Its domain.
    pub domain: String,
    /// The entry that defines it.
    pub entry: String,
    /// The agent definition its charter names: what to start the session under.
    pub agent: String,
    /// The tasks that wait for the session, in queue order.
    pub tasks: Vec<String>,
}

/// Whether a session could claim a task pinned to a position now, when `sessions` hold
/// a live lease on a task from that position: the task is open, or it is claimed and
/// its holder is none of them. Only a session in the position claims a pinned task, so
/// the holder of a live lease on it is one of `sessions`.
fn waits(task: &Task, sessions: &[String]) -> bool {
    match task.status {
        Status::Open => true,
        Status::Claimed => !task.holder.as_ref().is_some_and(|h| sessions.contains(h)),
        _ => false,
    }
}

fn none_in_force(name: &str) -> Error {
    Error::Denied(format!(
        "no position {name} is in force: a verified entry of type position defines one"
    ))
}

fn retired(name: &str) -> Error {
    Error::Denied(format!(
        "position {name} is retired: its history stays in the record, and it takes no session and no pin"
    ))
}

/// Why no session can fill a position, when none can: its envelope reads lower than its
/// own entry is classified, so a session in it could not read its own charter.
fn unfillable(p: &Position) -> Option<String> {
    (p.envelope.handling < p.classification).then(|| {
        format!(
            "position {} is classified {}, above the handling {} of its envelope: a session in it could not read its own charter",
            p.name, p.classification, p.envelope.handling
        )
    })
}

/// The holders of `tasks`, sorted, each once.
fn holders(tasks: Vec<Task>) -> Vec<String> {
    let mut out: Vec<String> = tasks.into_iter().filter_map(|t| t.holder).collect();
    out.sort();
    out.dedup();
    out
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// The position's section of T0.
fn slice(seat: &Seated, commitments: &[Task], handoffs: &[Hit]) -> String {
    let p = &seat.position;
    let list = |items: &[String]| {
        if items.is_empty() {
            "none".to_string()
        } else {
            items.join(", ")
        }
    };
    let mut out = format!("## Position {} ({}, verified)\n\n", p.name, p.entry);
    if !p.charter.orders.trim().is_empty() {
        out.push_str(&clip(p.charter.orders.trim(), ORDERS_IN_T0));
        out.push_str("\n\n");
    }
    out.push_str(&format!(
        "- Covers the task definitions: {}\n- Filled by the agent definition: {}\n- Authority envelope: tools {}; handling {}\n",
        list(&p.charter.covers),
        p.charter.agent,
        list(&p.envelope.tools),
        p.envelope.handling
    ));
    out.push_str("\n### Open commitments\n\n");
    if commitments.is_empty() {
        out.push_str("None on the ledger.\n");
    }
    for t in commitments {
        let reason = t
            .spec
            .pin_reason
            .map_or("pinned", dm_ledger::PinReason::as_str);
        out.push_str(&format!(
            "- `{}` {}, {reason}: {}\n",
            t.id,
            t.status.as_str(),
            clip(t.spec.title.trim(), TITLE_IN_T0)
        ));
    }
    out.push_str("\n### Handoff cache, read first\n\n");
    if handoffs.is_empty() {
        out.push_str("No handoff cites this position yet.\n");
    }
    for h in handoffs {
        out.push_str(&crate::t0::line(h));
    }
    if seat.seconded {
        out.push_str(
            "\nThis session is seconded to the position: it reads what is here, and it writes to its task only.\n",
        );
    }
    out.push('\n');
    out
}

impl Memory {
    /// The position named `name` in a granted domain, as the record holds it for
    /// everyone: from the active entries of type `position` that define the name, the
    /// newest verified one, or the owner's retiring one when none is verified. The tier
    /// and the author come from the record's entry.
    ///
    /// # Errors
    ///
    /// An index or store error.
    pub(crate) async fn position_record(
        &self,
        domain: &str,
        name: &str,
    ) -> Result<Option<Position>, Error> {
        if !self.grants.may_write(domain) || !is_name(name) {
            return Ok(None);
        }
        let entries = self.position_entries(domain).await?;
        Ok(in_force(&versions_of(&entries, name), &self.grants.owner).cloned())
    }

    /// The active entries of type `position` in a domain, newest first, each with the
    /// position it defines when its block reads. This is the whole record of the
    /// domain, whoever asks: which version is in force is one answer, and what a
    /// session may read of it is asked after.
    async fn position_entries(
        &self,
        domain: &str,
    ) -> Result<Vec<(Entry, Option<Position>)>, Error> {
        let all = ReadFilter::for_domains([domain], Label::Restricted);
        let hits = self
            .index
            .lock()
            .await
            .list_every(&all, Some(EntryType::Position))?;
        let mut out = Vec::new();
        for h in hits {
            let Some(e) = self.store.get(domain, &h.id).await? else {
                continue;
            };
            if e.is_active() {
                let position = position_in(&e);
                out.push((e, position));
            }
        }
        out.sort_by_key(|(e, _)| Reverse(e.front.created));
        Ok(out)
    }

    /// [`Memory::position_record`] where no lock can be awaited: through an index handle
    /// of its own and the files the record keeps, as the policy is loaded again. The
    /// check of a seat runs inside every read, and a read does not wait for the index.
    ///
    /// # Errors
    ///
    /// An index error; [`Error::Stale`] for an entry of type `position` whose text is
    /// not an entry, since which version is in force then cannot be shown.
    fn position_record_now(&self, domain: &str, name: &str) -> Result<Option<Position>, Error> {
        if !self.grants.may_write(domain) || !is_name(name) {
            return Ok(None);
        }
        let index = dm_index::Index::open_reader(&self.root.join(crate::INDEX_FILE))?;
        let all = ReadFilter::for_domains([domain], Label::Restricted);
        let mut entries = Vec::new();
        for h in index.list_every(&all, Some(EntryType::Position))? {
            let path = self.root.join("record").join(&h.domain);
            let Ok(text) = std::fs::read_to_string(path.join(format!("{}.md", h.id))) else {
                continue;
            };
            let e = Entry::parse(&text).map_err(|e| {
                Error::Stale(format!(
                    "the position entry {} cannot be read ({e}), so this session's position cannot be shown",
                    h.id
                ))
            })?;
            if e.is_active() {
                let position = position_in(&e);
                entries.push((e, position));
            }
        }
        entries.sort_by_key(|(e, _)| Reverse(e.front.created));
        Ok(in_force(&versions_of(&entries, name), &self.grants.owner).cloned())
    }

    /// The position named `name` that this session may read: in the domain named, or in
    /// the one granted domain that holds one for this session.
    async fn readable_position(
        &self,
        name: &str,
        domain: Option<&str>,
    ) -> Result<Option<Position>, Error> {
        let domains = match domain {
            Some(d) => vec![self.resolve_domain(Some(d))?],
            None => self.domains(),
        };
        let filter = self.session_filter()?;
        let mut found = Vec::new();
        for d in domains {
            let Some(p) = self.position_record(&d, name).await? else {
                continue;
            };
            if self.index.lock().await.meta(&p.entry, &filter)?.is_some() {
                found.push(p);
            }
        }
        if found.len() > 1 {
            return Err(Error::Denied(format!(
                "name a domain: more than one holds a position {name}"
            )));
        }
        Ok(found.pop())
    }

    /// The position named `name` that takes a session or a pin: in force, active, and
    /// one this session may read. A position the session may not read is refused in
    /// the same words as one that does not exist.
    async fn position_in_force(&self, name: &str, domain: Option<&str>) -> Result<Position, Error> {
        match self.readable_position(name, domain).await? {
            None => Err(none_in_force(name)),
            Some(p) if p.state == State::Retired => Err(retired(name)),
            Some(p) => Ok(p),
        }
    }

    /// Fill a position: this session, and every view of it, then claims the position's
    /// pinned tasks and the unpinned ones its charter covers, uses no tool and reads no
    /// entry outside its authority envelope, and gets its slice in T0. A launcher fills
    /// the position when the session starts. The session has then read the position's
    /// entry, and its label is no lower.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] when no position of that name is in force for this session,
    /// in the same words whether it does not exist, no person verified it, or the
    /// session may not read it; when it is retired; when more than one granted domain
    /// holds one and none is named; when its envelope reads lower than its own entry
    /// is classified; and when the session fills a position already.
    pub async fn fill_position(&self, name: &str, domain: Option<&str>) -> Result<Filled, Error> {
        if let Some(now) = self.seat() {
            return Err(Error::Denied(format!(
                "this session fills position {} already: a session fills one position",
                now.position
            )));
        }
        let position = self.position_in_force(name, domain).await?;
        if let Some(why) = unfillable(&position) {
            return Err(Error::Denied(why));
        }
        let seconded = agent_definition(&self.author) != Some(position.charter.agent.as_str());
        lock(&self.session).observe(position.classification);
        let seated = Seated { position, seconded };
        let filled = seated.filled();
        *lock(&self.seat) = Some(seated);
        Ok(filled)
    }

    /// Leave the position this session fills: the session is then held to its grants
    /// alone, as one that fills none.
    pub fn vacate(&self) {
        *lock(&self.seat) = None;
    }

    /// The position this session fills, when it fills one.
    #[must_use]
    pub fn seat(&self) -> Option<Filled> {
        lock(&self.seat).as_ref().map(Seated::filled)
    }

    /// What of the seat the read scope is computed under: the entry filled, and the
    /// handling of its envelope.
    pub(crate) fn seat_scope(&self) -> Option<(String, Label)> {
        lock(&self.seat)
            .as_ref()
            .map(|s| (s.position.entry.clone(), s.position.envelope.handling))
    }

    /// The seat, while the position is still the one this session filled: the record
    /// resolves the name to the same entry. That entry was in force and not retired
    /// when the session filled it, and an entry's text does not change. Every claim,
    /// read, tool call and T0 of a session in a position asks this first, so the
    /// envelope a session acts under is the one in force.
    ///
    /// # Errors
    ///
    /// [`Error::Stale`] when the position was replaced, retired, demoted or erased
    /// since.
    pub(crate) fn seat_in_force(&self) -> Result<Option<Seated>, Error> {
        let Some(seat) = lock(&self.seat).clone() else {
            return Ok(None);
        };
        let p = &seat.position;
        let now = self.position_record_now(&p.domain, &p.name)?;
        if now.is_some_and(|n| n.entry == p.entry) {
            Ok(Some(seat))
        } else {
            Err(Error::Stale(
                "the position changed after this session filled it: start a new session".into(),
            ))
        }
    }

    /// Drop from `capabilities` every tool the envelope of this session's position
    /// does not hold. Other capabilities pass.
    ///
    /// # Errors
    ///
    /// [`Error::Stale`] when the position changed after this session filled it.
    pub(crate) fn narrow_to_envelope(&self, capabilities: &mut Vec<String>) -> Result<(), Error> {
        if let Some(seat) = self.seat_in_force()? {
            let tools = &seat.position.envelope.tools;
            capabilities.retain(|c| !c.starts_with("tool:") || tools.contains(c));
        }
        Ok(())
    }

    /// Whether the position this session fills lets it call `tool`. A session that
    /// fills none is held to its grants and its task definition.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] when the envelope does not hold the tool; [`Error::Stale`]
    /// when the position changed after this session filled it.
    pub(crate) fn seat_allows(&self, tool: &str) -> Result<(), Error> {
        let named = format!("tool:{tool}");
        match self.seat_in_force()? {
            Some(s) if !s.position.envelope.tools.contains(&named) => Err(Error::Denied(format!(
                "position {} does not hold {named} in its authority envelope",
                s.position.name
            ))),
            _ => Ok(()),
        }
    }

    /// Bind a task to the position it names. A pin names a position and gives a
    /// reason. The position must be in force in the task's domain, and one this
    /// session may read. The session has then read its entry, and the task is
    /// classified no lower.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for half a pin, for a position that is not in force for this
    /// session, in the words [`Memory::fill_position`] uses, and for a retired one.
    pub(crate) async fn bind_position(&self, spec: &mut TaskSpec) -> Result<(), Error> {
        let name = match (&spec.position, spec.pin_reason) {
            (None, None) => return Ok(()),
            (Some(name), Some(_)) => name.clone(),
            _ => return Err(Error::Denied(
                "a pin names a position and gives a reason: continuity, relationship or separation"
                    .into(),
            )),
        };
        let position = self.position_in_force(&name, Some(&spec.domain)).await?;
        lock(&self.session).observe(position.classification);
        spec.classification = spec.classification.max(position.classification);
        Ok(())
    }

    /// Give a continuation that names no position the pin of the task it continues,
    /// before the pin is checked: [`Memory::bind_position`] then holds that pin to the
    /// rules of a pin the filer names, and the task is classified as the position is.
    /// The ledger gives a continuation the same pin; here the filer answers for it.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for a continuation of a task this session may not see, which
    /// names nothing of that task, and for one of a pinned task by a session that
    /// [`Memory::may_continue`] refuses.
    pub(crate) async fn continued_pin(
        &self,
        l: &dm_ledger::Ledger,
        spec: &mut TaskSpec,
    ) -> Result<(), Error> {
        let continued = spec.origin.as_deref().map(|o| o.strip_prefix("continues:"));
        let Some(Some(parent)) = continued else {
            return Ok(());
        };
        let parent = match l.get(parent).await {
            Ok(parent) => parent,
            Err(dm_ledger::Error::NotFound(_)) => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        if !self.sees(&parent, self.handling()?) {
            return Err(Error::Denied(
                "this task continues a task this session may not read; it is not filed here".into(),
            ));
        }
        self.may_continue(&parent)?;
        if spec.position.is_none() && parent.spec.domain == spec.domain {
            spec.position = parent.spec.position;
            spec.pin_reason = parent.spec.pin_reason;
        }
        Ok(())
    }

    /// Whether this session's word makes a task a commitment of the position it is
    /// pinned to: the session is a person's or the system's, or it fills that position
    /// in the task's domain under the agent definition the charter names. A task with
    /// no pin is no position's, and anyone's word does. Admission is that word: a
    /// pinned task another session files stays proposed until one of these admits it.
    ///
    /// # Errors
    ///
    /// [`Error::Stale`] when the position changed after this session filled it.
    pub(crate) fn commits(&self, task: &Task) -> Result<bool, Error> {
        use dm_policy::decide::Class;
        let Some(name) = task.spec.position.as_deref() else {
            return Ok(true);
        };
        let class = Class::of(&self.author, &self.grants.owner);
        if !matches!(class, Class::Agent | Class::Tap) {
            return Ok(true);
        }
        let seat = self.seat_in_force()?;
        Ok(seat.is_some_and(|s| s.position.name == name && s.writes_history_in(&task.spec.domain)))
    }

    /// Whether this session works `task`: it holds the task under the claim it made,
    /// or under the claim its launcher made for it.
    fn works(&self, task: &Task) -> bool {
        let claimed = lock(&self.claims).get(&task.id) == Some(&task.generation);
        let held = self.held_task().as_deref() == Some(task.id.as_str());
        task.status == Status::Claimed && (claimed || held)
    }

    /// Whether this session may file into the chain of `parent`: fan it out, or file
    /// its continuation. A task waits on its continuation, and nobody claims it
    /// meanwhile; the continuation of a pinned task is a commitment of the position.
    /// So the chain of a pinned task is for the session that works the task, which a
    /// seconded session does too, and for a session that may commit the position. A
    /// task with no pin is held to no such rule here.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for any other session; [`Error::Stale`] when the position
    /// changed after this session filled it.
    pub(crate) fn may_continue(&self, parent: &Task) -> Result<(), Error> {
        if self.works(parent) || self.commits(parent)? {
            return Ok(());
        }
        Err(Error::Denied(format!(
            "{} is pinned to position {}: only the session that holds it, or one that may admit a task of the position, files its subagent tasks or its continuation",
            parent.id,
            parent.spec.position.as_deref().unwrap_or_default()
        )))
    }

    /// The citations of a write, as the record takes them. A cite that starts `pos-`
    /// names a position, and only a session that fills that position in the write's
    /// domain, under the agent definition its charter names, writes one. Such a
    /// session's handoff cites the position whether it says so or not, which is how
    /// the handoff cache fills. A session whose position changed under it writes its
    /// handoff to its task, with no such cite.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for a cite of a position by any other session;
    /// [`Error::Stale`] for one by a session whose position changed after it filled it.
    pub(crate) async fn cites_for_write(
        &self,
        domain: &str,
        kind: EntryType,
        mut cites: Vec<String>,
    ) -> Result<Vec<String>, Error> {
        let own = lock(&self.seat)
            .clone()
            .filter(|s| s.writes_history_in(domain));
        let mine = own.as_ref().map(|s| s.position.name.as_str());
        if let Some(other) = cites
            .iter()
            .find(|c| c.starts_with(PREFIX) && Some(c.as_str()) != mine)
        {
            return Err(Error::Denied(format!(
                "{other} is a position: only a session that fills it under the agent definition its charter names writes its history, commitments or handoff cache"
            )));
        }
        let Some(name) = mine else {
            return Ok(cites);
        };
        if cites.iter().any(|c| c == name) {
            self.seat_in_force()?;
        } else if kind == EntryType::Handoff {
            match self.seat_in_force() {
                Ok(_) => cites.push(name.to_string()),
                Err(Error::Stale(_)) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(cites)
    }

    /// The open commitments of a position that this session may see, read: the tasks
    /// pinned to it that are admitted and not closed, in queue order. A proposed task
    /// is not one: nobody who may commit the position has admitted it.
    async fn commitments(&self, domain: &str, name: &str) -> Result<Vec<Task>, Error> {
        let pinned = self.ledger.lock().await.pinned_to(domain, name).await?;
        let handling = self.handling()?;
        let seen: Vec<Task> = pinned
            .into_iter()
            .filter(|t| t.status != Status::Proposed && self.sees(t, handling))
            .take(COMMITMENTS)
            .collect();
        self.read_tasks(seen)
    }

    /// The handoff cache of a position: the newest handoffs of its domain, within this
    /// session's filter, whose citations hold its name. Each is read.
    async fn handoff_cache(&self, domain: &str, name: &str) -> Result<Vec<Hit>, Error> {
        let f = self.filter_for(Some(domain))?;
        let newest = self
            .index
            .lock()
            .await
            .list(&f, Some(EntryType::Handoff), CACHE_WALK)?;
        let mut out = Vec::new();
        for h in newest {
            if out.len() == CACHE_SIZE {
                break;
            }
            let entry = self.store.get(domain, &h.id).await?;
            if entry.is_some_and(|e| e.front.cites.iter().any(|c| c == name)) {
                out.push(h);
            }
        }
        self.observe(&out);
        Ok(out)
    }

    /// The position's section of the T0 of `domain`: empty for a session that fills no
    /// position there.
    ///
    /// # Errors
    ///
    /// [`Error::Stale`] when the position changed after this session filled it; an
    /// index, store or ledger error.
    pub(crate) async fn position_slice(&self, domain: &str) -> Result<String, Error> {
        let here = lock(&self.seat)
            .as_ref()
            .is_some_and(|s| s.position.domain == domain);
        if !here {
            return Ok(String::new());
        }
        let Some(seat) = self.seat_in_force()? else {
            return Ok(String::new());
        };
        let name = &seat.position.name;
        lock(&self.session).observe(seat.position.classification);
        let commitments = self.commitments(domain, name).await?;
        let handoffs = self.handoff_cache(domain, name).await?;
        Ok(slice(&seat, &commitments, &handoffs))
    }

    /// A position as this session reads it: the record, in force or retired, with its
    /// open commitments, its handoff cache, and the sessions that hold a task from it.
    /// Any session that may read the position's entry reads this, seconded or in no
    /// position.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] when no such position is there for this session;
    /// [`Error::Denied`] when more than one granted domain holds one and none is named.
    pub async fn position(&self, name: &str, domain: Option<&str>) -> Result<PositionView, Error> {
        let Some(position) = self.readable_position(name, domain).await? else {
            return Err(Error::NotFound(format!("position {name}")));
        };
        lock(&self.session).observe(position.classification);
        let commitments = self.commitments(&position.domain, name).await?;
        let cache = self.handoff_cache(&position.domain, name).await?;
        self.reads.hits("position", Some(name), &cache);
        let sessions = self.live_in(&position.domain, name).await?;
        let seat = lock(&self.seat)
            .as_ref()
            .filter(|s| s.position.name == name && s.position.domain == position.domain)
            .map(|s| SeatView {
                seconded: s.seconded,
            });
        Ok(PositionView {
            position,
            commitments,
            handoffs: cache
                .into_iter()
                .map(|h| CachedHandoff {
                    id: h.id,
                    title: h.title,
                    summary: h.summary,
                })
                .collect(),
            sessions,
            seat,
        })
    }

    /// The sessions that are live in a position, as this session may know them: the
    /// holders of the tasks claimed from it under a live lease, of the tasks this
    /// session may see. Each such task is read. The holder of a task above this
    /// session's handling is not named, as the task is not.
    async fn live_in(&self, domain: &str, name: &str) -> Result<Vec<String>, Error> {
        let held = self.ledger.lock().await.held_from(domain, name).await?;
        Ok(holders(self.read_tasks(held)?))
    }

    /// Whether this session may retire a position for the reason `why`. The reason
    /// goes into an entry, so it is words only.
    fn may_retire(&self, why: &str) -> Result<(), Error> {
        if self.author != self.grants.owner {
            return Err(Error::Denied(format!(
                "only the owner {} retires a position",
                self.grants.owner
            )));
        }
        let marked = ["```", "~~~", "\n", "\r", "<"]
            .iter()
            .any(|mark| why.contains(mark));
        if marked || why.trim().is_empty() {
            return Err(Error::Denied(
                "the reason to retire a position is words on one line, with no code fence and \
                 no markup"
                    .into(),
            ));
        }
        Ok(())
    }

    /// Replace each verified version that defines `name` as active by a retiring
    /// version: the same body with the block's status set, and the reason as its last
    /// paragraph. The oldest goes first, so the newest retiring version is the one
    /// that replaces the version in force, and no older verified version comes back
    /// into force.
    async fn write_retiring(&self, domain: &str, name: &str, why: &str) -> Result<(), Error> {
        let entries = self.position_entries(domain).await?;
        let in_force_once = versions_of(&entries, name)
            .into_iter()
            .rev()
            .filter(|(e, p)| e.front.trust_tier == TrustTier::Verified && p.state == State::Active);
        for (old, _) in in_force_once {
            let body = with_status(&old.body, State::Retired).map_err(Error::Denied)?;
            self.write(WriteRequest {
                domain: Some(domain.to_string()),
                entry_type: EntryType::Position,
                title: old.front.title.clone(),
                body: format!("{}\n\nRetired: {why}\n", body.trim_end()),
                supersedes: Some(old.front.id.clone()),
                classification: Some(old.front.classification),
                ..WriteRequest::default()
            })
            .await?;
        }
        Ok(())
    }

    /// The tasks pinned to a position, when this session may see each of them.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] when one is above this session's handling: a retirement takes
    /// the pin off every task, and this session would change a task it may not read.
    async fn pins_in_reach(&self, domain: &str, name: &str) -> Result<(), Error> {
        let pinned = self.ledger.lock().await.pinned_to(domain, name).await?;
        let handling = self.handling()?;
        if pinned.iter().all(|t| self.sees(t, handling)) {
            Ok(())
        } else {
            Err(Error::Denied(format!(
                "position {name} has pinned tasks this session may not read; it is not retired here"
            )))
        }
    }

    /// Take the pin off every open task of a position, so each returns to matching.
    /// The reason the ledger keeps is a fixed sentence. Gives the tasks, read.
    async fn unpin_all(&self, domain: &str, name: &str) -> Result<Vec<String>, Error> {
        let why = format!("position {name} was retired");
        let mut unpinned = Vec::new();
        {
            let mut l = self.ledger.lock().await;
            for task in l.pinned_to(domain, name).await? {
                unpinned.push(l.unpin(&task.id, &why).await?);
            }
        }
        let read = self.read_tasks(unpinned)?;
        Ok(read.into_iter().map(|t| t.id).collect())
    }

    /// Retire a position: the owner's step down, as the retirement of a rule is. The
    /// version in force is replaced by one whose block says `status = "retired"`, with
    /// the reason as its last paragraph. That version counts at once, since only the
    /// owner replaces a verified entry. Each task pinned to the position then loses
    /// its pin and returns to matching. The position's history stays under its name.
    ///
    /// Asked again for a retired position, it finds no verified version that says
    /// active, so it writes no second version. It takes the pin off what is still
    /// pinned, so a second call finishes a retirement that failed half way.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for anyone but the owner of the record, for a reason that is
    /// empty or is not words on one line, and when a task pinned to the position is
    /// above this session's handling; [`Error::NotFound`] when no such position is
    /// there for this session; and as [`Memory::write`].
    pub async fn retire_position(
        &self,
        name: &str,
        why: &str,
        domain: Option<&str>,
    ) -> Result<Retired, Error> {
        self.may_retire(why)?;
        let Some(found) = self.readable_position(name, domain).await? else {
            return Err(Error::NotFound(format!("position {name}")));
        };
        let domain = found.domain;
        self.pins_in_reach(&domain, name).await?;
        self.write_retiring(&domain, name, why).await?;
        let now = self.position_record(&domain, name).await?;
        let Some(now) = now.filter(|p| p.state == State::Retired) else {
            return Err(Error::Denied(format!(
                "position {name} does not resolve as retired: ask again"
            )));
        };
        // the owner's session that fills the position leaves it: no position is there
        // to fill, and a session in a position that changed reads nothing more
        let fills = |s: &Seated| s.position.name == name && s.position.domain == domain;
        if lock(&self.seat).as_ref().is_some_and(fills) {
            self.vacate();
        }
        let retiring = self.store.get(&domain, &now.entry).await?;
        let replaced = retiring.and_then(|e| e.front.supersedes);
        let unpinned = self.unpin_all(&domain, name).await?;
        Ok(Retired {
            position: name.to_string(),
            entry: replaced.unwrap_or_else(|| now.entry.clone()),
            retired_by: now.entry,
            domain,
            unpinned,
        })
    }

    /// The row of the record in force of a name: what is pinned to the position, what
    /// of it waits, and who is live in it.
    async fn in_force_row(&self, p: &Position) -> Result<ListedPosition, Error> {
        let (pinned, held) = {
            let l = self.ledger.lock().await;
            let pinned = l.pinned_to(&p.domain, &p.name).await?;
            (pinned, l.held_from(&p.domain, &p.name).await?)
        };
        let pinned = self.read_tasks(pinned)?;
        // the holder of a task this session may not see is not named, as the task is not
        let sessions = holders(self.read_tasks(held)?);
        let waiting: Vec<String> = pinned
            .iter()
            .filter(|t| waits(t, &sessions))
            .map(|t| t.id.clone())
            .collect();
        let active = p.state == State::Active;
        let takes_a_session = active && unfillable(p).is_none();
        Ok(ListedPosition {
            name: p.name.clone(),
            entry: p.entry.clone(),
            domain: p.domain.clone(),
            state: if active {
                Standing::Active
            } else {
                Standing::Retired
            },
            agent: p.charter.agent.clone(),
            pinned: pinned.len(),
            wake: takes_a_session && !waiting.is_empty() && sessions.is_empty(),
            waiting,
            sessions,
            why_not: None,
        })
    }

    /// The row of an entry that defines `p`: the record in force of its name, or why
    /// the entry is not that record. A verified entry that a version this session may
    /// not read replaced has no row: to this session that position does not exist.
    async fn listed(
        &self,
        e: &Entry,
        p: &Position,
        entries: &[(Entry, Option<Position>)],
        readable: &[Hit],
    ) -> Result<Option<ListedPosition>, Error> {
        let record = in_force(&versions_of(entries, &p.name), &self.grants.owner);
        if record.is_some_and(|r| r.entry == p.entry) {
            return Ok(Some(self.in_force_row(p).await?));
        }
        let why = if e.front.trust_tier != TrustTier::Verified {
            format!(
                "its entry is {}; a position is in force once a person verifies it",
                e.front.trust_tier
            )
        } else if record.is_some_and(|r| readable.iter().any(|h| h.id == r.entry)) {
            format!("a newer verified entry defines {}", p.name)
        } else {
            return Ok(None);
        };
        Ok(Some(ListedPosition::not_in_force(e, Some(p), why)))
    }

    /// The rows of one domain within `filter`, each with its entry's index row and the
    /// time of the entry.
    async fn listed_in(
        &self,
        domain: &str,
        filter: &ReadFilter,
    ) -> Result<Vec<(OffsetDateTime, Hit, ListedPosition)>, Error> {
        let mut f = filter.clone();
        f.domains = vec![domain.to_string()];
        let readable = self
            .index
            .lock()
            .await
            .list_every(&f, Some(EntryType::Position))?;
        let entries = self.position_entries(domain).await?;
        let mut rows = Vec::new();
        for (e, p) in &entries {
            let Some(hit) = readable.iter().find(|h| h.id == e.front.id) else {
                continue;
            };
            let row = match p {
                Some(p) => self.listed(e, p, &entries, &readable).await?,
                None => Some(ListedPosition::not_in_force(
                    e,
                    None,
                    "its block does not read as a position".into(),
                )),
            };
            if let Some(row) = row {
                rows.push((e.front.created, hit.clone(), row));
            }
        }
        Ok(rows)
    }

    /// The positions of one granted domain, or of all when none is named: every active
    /// entry of type `position` this session may read, newest first. An entry that is
    /// the record in force of its name says whether the position is active or
    /// retired, how many open tasks are pinned to it, which of them wait, which
    /// sessions are live in it, and whether it needs a session started. Any other
    /// entry says why it is not in force. Each entry listed is read, and so is each
    /// task counted.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for a domain outside the grants or outside what this session
    /// reads; an index, store or ledger error.
    pub async fn positions(&self, domain: Option<&str>) -> Result<Vec<ListedPosition>, Error> {
        let f = self.filter_for(domain)?;
        let mut rows = Vec::new();
        for d in self.domains() {
            if f.domains.contains(&d) {
                rows.extend(self.listed_in(&d, &f).await?);
            }
        }
        rows.sort_by_key(|(created, _, _)| Reverse(*created));
        let hits: Vec<Hit> = rows.iter().map(|(_, hit, _)| hit.clone()).collect();
        self.observe(&hits);
        self.reads.hits("positions", None, &hits);
        Ok(rows.into_iter().map(|(_, _, row)| row).collect())
    }

    /// The positions that need a session started, of one granted domain or of all:
    /// each is active, is one a session can fill, has a task that waits, and has no
    /// live session. Each names the agent definition of its charter and the tasks that
    /// wait, in queue order. A launcher reads this list and starts the sessions; the
    /// service starts none.
    ///
    /// # Errors
    ///
    /// As [`Memory::positions`].
    pub async fn wake_list(&self, domain: Option<&str>) -> Result<Vec<Wake>, Error> {
        let listed = self.positions(domain).await?;
        Ok(listed
            .into_iter()
            .filter(|p| p.wake)
            .map(|p| Wake {
                position: p.name,
                domain: p.domain,
                entry: p.entry,
                agent: p.agent,
                tasks: p.waiting,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Promotion, WriteRequest};
    use dm_ledger::PinReason;
    use std::path::Path;
    use std::sync::Arc;

    const RELEASE: &str = "pos-release";
    const ORDERS: &str = "Each release is cut from main, after the mirrors are checked.";
    const ME: &str = "dom-me";
    const PLAT: &str = "dom-plat";
    const DEFINITIONS: &str = "# Kinds of work\n\n```platform task release\npurpose = \"Cut a release.\"\nacceptance = [\"The tag is on main.\"]\n```\n\n```platform task triage\npurpose = \"Sort new issues.\"\nacceptance = [\"Each issue has a label.\"]\n```\n";

    /// A root with two domains, `dom-me` and the platform's `dom-plat`, and two tools
    /// the grants give every session, `fetch` and `x`.
    fn root(dir: &Path) -> Memory {
        Memory::init(dir, "human:test:me", ME).unwrap();
        let path = dir.join(crate::GRANTS_FILE);
        let mut g = dm_policy::Grants::load(&path).unwrap();
        let d = g.domains[ME].clone();
        g.domains.insert(PLAT.into(), d);
        g.platform = Some(PLAT.into());
        let mut text = g.render().unwrap();
        for tool in ["fetch", "x"] {
            text.push_str(&format!(
                "\n[taps.{tool}]\ndomain = \"{ME}\"\nhosts = []\nceiling = \"public\"\ntool = true\n"
            ));
        }
        std::fs::write(&path, text).unwrap();
        Memory::open(dir).unwrap()
    }

    /// A session of the root under the agent definition `agent`.
    fn session(dir: &Path, agent: &str) -> Memory {
        Memory::open(dir)
            .unwrap()
            .with_author(format!("agent:{agent} (model: test-1)"))
    }

    /// The release coordinator: it covers `release`, codex fills it, and its envelope
    /// holds `tool:fetch` and reads everything.
    fn release() -> Position {
        Position {
            name: RELEASE.into(),
            entry: String::new(),
            domain: ME.into(),
            classification: Label::Internal,
            state: State::Active,
            charter: Charter {
                orders: ORDERS.into(),
                covers: vec!["release".into()],
                agent: "codex".into(),
            },
            envelope: Envelope {
                tools: vec!["tool:fetch".into()],
                handling: Label::Restricted,
            },
        }
    }

    fn body(p: &Position) -> String {
        format!("# The release coordinator\n\n{}", p.block().unwrap())
    }

    /// The write of an entry that defines `p`, in `p`'s domain.
    fn entry(p: &Position) -> WriteRequest {
        WriteRequest {
            body: body(p),
            domain: Some(p.domain.clone()),
            entry_type: EntryType::Position,
            ..WriteRequest::default()
        }
    }

    fn handoff(body: &str) -> WriteRequest {
        WriteRequest {
            body: body.into(),
            domain: Some(ME.into()),
            entry_type: EntryType::Handoff,
            ..WriteRequest::default()
        }
    }

    fn citing(mut req: WriteRequest, cite: &str) -> WriteRequest {
        req.cites.push(cite.into());
        req
    }

    async fn write(m: &Memory, req: WriteRequest) -> String {
        m.write(req).await.unwrap().id
    }

    /// As a person other than its proposer would leave it.
    async fn verify(m: &Memory, domain: &str, id: &str) {
        let mut e = m.store.get(domain, id).await.unwrap().unwrap();
        e.front.trust_tier = TrustTier::Verified;
        m.store.put(&e).await.unwrap();
    }

    /// Write the entry that defines `p`, verified. Returns its id.
    async fn in_force(m: &Memory, p: &Position) -> String {
        let id = write(m, entry(p)).await;
        verify(m, &p.domain, &id).await;
        id
    }

    async fn cites_of(m: &Memory, id: &str) -> Vec<String> {
        m.store.get(ME, id).await.unwrap().unwrap().front.cites
    }

    fn task(title: &str) -> TaskSpec {
        TaskSpec {
            domain: ME.into(),
            title: title.into(),
            body: "Do it.".into(),
            ..TaskSpec::default()
        }
    }

    /// A task pinned to the release coordinator, for continuity.
    fn pinned(title: &str) -> TaskSpec {
        TaskSpec {
            position: Some(RELEASE.into()),
            pin_reason: Some(PinReason::Continuity),
            ..task(title)
        }
    }

    /// File a task as the owner's own, and show that it is admitted.
    async fn admitted(owner: &Memory, spec: TaskSpec) -> Task {
        let t = owner.file_own(spec).await.unwrap().task;
        assert_eq!(t.status, Status::Open, "{}", t.spec.title);
        t
    }

    fn denied<T>(r: Result<T, Error>) -> String {
        match r {
            Err(Error::Denied(why)) => why,
            Err(other) => panic!("refused, but not as denied: {other}"),
            Ok(_) => panic!("it was allowed"),
        }
    }

    fn stale<T>(r: Result<T, Error>) {
        match r {
            Err(Error::Stale(why)) => assert!(why.contains("the position changed"), "{why}"),
            Err(other) => panic!("refused, but not as stale: {other}"),
            Ok(_) => panic!("it was allowed"),
        }
    }

    fn lease() -> time::Duration {
        time::Duration::minutes(5)
    }

    async fn next(m: &Memory) -> Option<String> {
        let claim = m.claim_next(Some(ME), lease()).await.unwrap();
        claim.map(|c| c.task.id)
    }

    /// The ids of the facts of `dom-me` that a session lists.
    async fn facts(m: &Memory) -> Vec<String> {
        let hits = m.list(Some(ME), Some(EntryType::Fact), 20).await.unwrap();
        hits.into_iter().map(|h| h.id).collect()
    }

    #[test]
    fn a_name_and_an_agent_definition_read_as_the_paper_writes_them() {
        assert!(is_name("pos-a-qa") && is_name("pos-release-2"));
        for bad in ["pos-", "release", "pos-Release", "pos-a qa", "xpos-a"] {
            assert!(!is_name(bad), "{bad}");
        }
        assert_eq!(agent_definition("agent:codex (model: x)"), Some("codex"));
        assert_eq!(agent_definition("agent:codex"), Some("codex"));
        assert_eq!(agent_definition("agent:"), None);
        assert_eq!(agent_definition("human:test:me"), None);
    }

    fn entry_of(kind: &str, body: &str) -> Entry {
        Entry::parse(&format!(
            "---\nid: mem-me-1\ntype: {kind}\ndomain: {ME}\nauthor: human:test:me\n\
             created: 2026-10-01T09:00:00Z\ntrust_tier: verified\nclassification: internal\n---\n{body}"
        ))
        .unwrap()
    }

    /// `text` with `from` replaced, which it must hold.
    fn with(text: &str, from: &str, to: &str) -> String {
        assert!(text.contains(from), "{from:?} is not in {text}");
        text.replace(from, to)
    }

    #[test]
    fn a_position_reads_from_the_one_block_of_an_entry_of_its_type() {
        let mut p = release();
        p.entry = "mem-me-1".into();
        let good = body(&p);
        assert_eq!(position_in(&entry_of("position", &good)), Some(p.clone()));
        // beside a block of another kind, and as a retiring version
        let beside = format!("```toml\nx = 1\n```\n\n{good}");
        assert_eq!(position_in(&entry_of("position", &beside)), Some(p.clone()));
        p.state = State::Retired;
        let retiring = body(&p);
        assert!(retiring.contains("status = \"retired\""), "{retiring}");
        assert_eq!(position_in(&entry_of("position", &retiring)), Some(p));
        assert_eq!(position_in(&entry_of("convention", &good)), None);
        let two = format!("{good}\n{good}");
        let quoted = format!("````text\n{good}````\n");
        let none = [
            with(&good, "[envelope]", "[envelope]\nbudget = 3"),
            with(&good, "[charter]", "owner = \"me\"\n\n[charter]"),
            with(&good, "position pos-release", "position release"),
            with(&good, "position pos-release", "position pos-release now"),
            with(&good, "agent = \"codex\"", "agent = \"\""),
            with(&good, "agent = \"codex\"", "agent = \"codex two\""),
            with(&good, "\"tool:fetch\"", "\"fetch\""),
            with(&good, "handling = \"restricted\"", ""),
            two,
            quoted,
        ];
        for text in none {
            assert_eq!(position_in(&entry_of("position", &text)), None, "{text}");
        }
    }

    #[tokio::test]
    async fn a_position_is_in_force_once_verified_and_the_newest_verified_entry_is() {
        let dir = tempfile::tempdir().unwrap();
        let owner = root(dir.path());
        let first = write(&owner, entry(&release())).await;
        let codex = session(dir.path(), "codex");
        // no person verified it: to a session and to a pin it is a name nothing defines
        let unverified = denied(codex.fill_position(RELEASE, None).await);
        let unknown = denied(codex.fill_position("pos-nothere", None).await);
        assert_eq!(unverified, unknown.replace("pos-nothere", RELEASE));
        assert!(unverified.contains("no position pos-release is in force"));
        assert_eq!(denied(owner.file_own(pinned("Cut 1.2")).await), unverified);
        // a session proposes it and a person approves it: the review gate
        let proposed = codex.promote(&first, TrustTier::Verified, "read against the notes");
        let Promotion::Proposed { task } = proposed.await.unwrap() else {
            panic!("proposed")
        };
        owner.approve(&task.id).await.unwrap();
        let filled = codex.fill_position(RELEASE, None).await.unwrap();
        let expected = Filled {
            position: RELEASE.into(),
            domain: ME.into(),
            entry: first,
            agent: "codex".into(),
            seconded: false,
        };
        assert_eq!(filled, expected);
        assert_eq!(codex.seat(), Some(expected));
        // a second verified entry is the one in force, and a later one that no person
        // verified is not
        let mut wider = release();
        wider.charter.covers.push("triage".into());
        let second = in_force(&owner, &wider).await;
        write(&owner, entry(&release())).await;
        let later = session(dir.path(), "codex");
        let filled = later.fill_position(RELEASE, Some(ME)).await.unwrap();
        assert_eq!(filled.entry, second);
    }

    #[tokio::test]
    async fn a_position_above_a_session_s_handling_is_one_that_does_not_exist() {
        let dir = tempfile::tempdir().unwrap();
        let owner = root(dir.path());
        let mut req = entry(&release());
        req.classification = Some(Label::Restricted);
        let id = write(&owner, req).await;
        verify(&owner, ME, &id).await;
        let low = session(dir.path(), "codex").with_handling_cap(Label::Internal);
        let hidden = denied(low.fill_position(RELEASE, None).await);
        let missing = denied(low.fill_position("pos-nothere", None).await);
        assert_eq!(hidden, missing.replace("pos-nothere", RELEASE));
        assert_eq!(denied(low.create_task(pinned("Cut 1.2")).await), hidden);
        let read = low.position(RELEASE, None).await;
        assert!(matches!(read, Err(Error::NotFound(_))));
        assert_eq!(low.label(), Label::Public, "nothing of it was read");
        // a session that may read the entry fills the position, and has then read it
        let high = session(dir.path(), "codex");
        high.fill_position(RELEASE, None).await.unwrap();
        assert_eq!(high.label(), Label::Restricted);
    }

    #[tokio::test]
    async fn a_task_is_pinned_only_to_a_position_in_force_and_with_a_reason() {
        let dir = tempfile::tempdir().unwrap();
        let owner = root(dir.path());
        let mut req = entry(&release());
        req.classification = Some(Label::Restricted);
        let id = write(&owner, req).await;
        verify(&owner, ME, &id).await;
        let filer = session(dir.path(), "claude");
        let mut no_reason = pinned("No reason");
        no_reason.pin_reason = None;
        let mut reason_alone = pinned("A reason alone");
        reason_alone.position = None;
        for half in [no_reason, reason_alone] {
            let why = denied(filer.create_task(half).await);
            assert!(
                why.contains("continuity, relationship or separation"),
                "{why}"
            );
        }
        let mut unknown = pinned("No such position");
        unknown.position = Some("pos-nothere".into());
        let why = denied(filer.create_task(unknown).await);
        assert!(why.contains("no position pos-nothere is in force"), "{why}");
        for status in [Status::Proposed, Status::Open] {
            let filed = owner.ledger.lock().await.list(ME, status).await.unwrap();
            assert!(filed.is_empty(), "nothing was filed");
        }
        // with both, the task is pinned, and it is classified as the position's entry is
        let filed = filer.create_task(pinned("Cut 1.2")).await.unwrap().task;
        assert_eq!(filed.spec.position.as_deref(), Some(RELEASE));
        assert_eq!(filed.spec.pin_reason, Some(PinReason::Continuity));
        assert_eq!(filed.spec.classification, Label::Restricted);
        assert_eq!(filer.label(), Label::Restricted, "the filer read the entry");
    }

    #[tokio::test]
    async fn a_pinned_task_goes_only_to_a_session_that_fills_its_position() {
        let dir = tempfile::tempdir().unwrap();
        let owner = root(dir.path());
        in_force(&owner, &release()).await;
        let first = admitted(&owner, pinned("Cut 1.2")).await;
        let second = admitted(&owner, pinned("Cut 1.3")).await;
        let outside = session(dir.path(), "claude");
        assert_eq!(
            next(&outside).await,
            None,
            "it passes a session in no position"
        );
        let refused = outside.claim(&first.id, lease()).await.unwrap_err();
        assert!(
            matches!(&refused, Error::Ledger(dm_ledger::Error::Pinned { position, .. }) if position == RELEASE),
            "{refused}"
        );
        let seated = session(dir.path(), "codex");
        seated.fill_position(RELEASE, None).await.unwrap();
        assert_eq!(next(&seated).await, Some(first.id));
        let named = seated.claim(&second.id, lease()).await.unwrap();
        assert_eq!(named.task.id, second.id);
        // and the ledger then shows who fills the position
        let view = owner.position(RELEASE, None).await.unwrap();
        assert_eq!(view.sessions, [seated.author()]);
    }

    #[tokio::test]
    async fn a_session_in_a_position_claims_in_the_position_s_domain_only() {
        let dir = tempfile::tempdir().unwrap();
        let owner = root(dir.path());
        in_force(&owner, &release()).await;
        let mut abroad = release();
        abroad.domain = PLAT.into();
        let other = in_force(&owner, &abroad).await;
        let here = admitted(&owner, task("Tidy")).await;
        let codex = session(dir.path(), "codex");
        let which = denied(codex.fill_position(RELEASE, None).await);
        assert!(which.contains("name a domain"), "{which}");
        let filled = codex.fill_position(RELEASE, Some(PLAT)).await.unwrap();
        assert_eq!((filled.domain.as_str(), filled.entry), (PLAT, other));
        let by_rank = denied(codex.claim_next(Some(ME), lease()).await);
        assert!(by_rank.contains("it claims no task of dom-me"), "{by_rank}");
        let by_name = denied(codex.claim(&here.id, lease()).await);
        assert_eq!(by_name, by_rank);
        // one session fills one position
        let again = denied(codex.fill_position(RELEASE, Some(ME)).await);
        assert!(
            again.contains("fills position pos-release already"),
            "{again}"
        );
        assert_eq!(codex.seat().unwrap().domain, PLAT);
    }

    #[tokio::test]
    async fn a_session_in_a_position_takes_an_unpinned_task_only_when_its_charter_covers_it() {
        let dir = tempfile::tempdir().unwrap();
        let owner = root(dir.path());
        let mut defs = entry(&release());
        defs.body = DEFINITIONS.into();
        defs.domain = Some(PLAT.into());
        defs.entry_type = EntryType::Convention;
        let defs = write(&owner, defs).await;
        verify(&owner, PLAT, &defs).await;
        in_force(&owner, &release()).await;
        let defined = |title: &str, definition: &str| TaskSpec {
            definition: Some(definition.into()),
            ..task(title)
        };
        // the covered task is filed last, so the queue's order alone does not give it
        let bare = admitted(&owner, task("Tidy")).await;
        let other = admitted(&owner, defined("Sort the inbox", "triage")).await;
        let covered = admitted(&owner, defined("Cut 1.2", "release")).await;
        let seated = session(dir.path(), "codex");
        seated.fill_position(RELEASE, None).await.unwrap();
        assert_eq!(next(&seated).await, Some(covered.id));
        assert_eq!(next(&seated).await, None, "nothing else is the position's");
        let refused = seated.claim(&other.id, lease()).await;
        assert!(matches!(
            refused,
            Err(Error::Ledger(dm_ledger::Error::WrongState { .. }))
        ));
        // a session in no position takes the rest
        let free = session(dir.path(), "claude");
        assert_eq!(next(&free).await, Some(bare.id));
        assert_eq!(next(&free).await, Some(other.id));
    }

    #[tokio::test]
    async fn a_session_in_a_position_presents_and_calls_no_tool_outside_the_envelope() {
        let dir = tempfile::tempdir().unwrap();
        let owner = root(dir.path());
        in_force(&owner, &release()).await;
        let requiring = |title: &str, tool: &str| TaskSpec {
            requirements: vec![tool.into()],
            ..pinned(title)
        };
        let needs_x = admitted(&owner, requiring("Uses x", "tool:x")).await;
        let needs_fetch = admitted(&owner, requiring("Uses fetch", "tool:fetch")).await;
        let seated = Arc::new(session(dir.path(), "codex").with_manifest(vec!["human".into()]));
        assert_eq!(
            seated.capabilities().unwrap(),
            ["human", "tool:fetch", "tool:x"]
        );
        seated.fill_position(RELEASE, None).await.unwrap();
        assert_eq!(seated.capabilities().unwrap(), ["human", "tool:fetch"]);
        // the grants give it tool:x, and the task is its position's: the envelope does not
        assert_eq!(next(&seated).await, Some(needs_fetch.id));
        assert_eq!(next(&seated).await, None);
        let refused = seated.claim(&needs_x.id, lease()).await;
        assert!(matches!(
            refused,
            Err(Error::Ledger(dm_ledger::Error::WrongState { .. }))
        ));
        let why = denied(seated.seat_allows("x"));
        assert_eq!(
            why,
            "position pos-release does not hold tool:x in its authority envelope"
        );
        seated.seat_allows("fetch").unwrap();
        // through the call a tool goes through: refused for the envelope, and a tool
        // inside it goes on to the next check
        let called = denied(seated.use_tool("x", serde_json::json!({})).await);
        assert_eq!(called, why);
        let inside = denied(seated.use_tool("fetch", serde_json::json!({})).await);
        assert!(inside.contains("runs no tools"), "{inside}");
    }

    /// A root with a position whose envelope reads public entries only, in a public
    /// entry, and an internal fact. Returns the owner's session and the fact's id.
    async fn public_seat(dir: &Path) -> (Memory, String) {
        let owner = root(dir);
        let mut p = release();
        p.envelope.handling = Label::Public;
        p.envelope.tools.clear();
        p.charter.covers.clear();
        let mut req = entry(&p);
        req.classification = Some(Label::Public);
        let id = write(&owner, req).await;
        verify(&owner, ME, &id).await;
        let mut fact = handoff("The staging host is alder.\n");
        fact.entry_type = EntryType::Fact;
        let fact = write(&owner, fact).await;
        (owner, fact)
    }

    #[tokio::test]
    async fn a_session_in_a_position_reads_no_higher_than_the_envelope_s_handling() {
        let dir = tempfile::tempdir().unwrap();
        let (owner, fact) = public_seat(dir.path()).await;
        let free = session(dir.path(), "codex");
        assert_eq!(facts(&free).await, std::slice::from_ref(&fact));
        free.get(&fact).await.unwrap();
        let seated = session(dir.path(), "codex");
        seated.fill_position(RELEASE, None).await.unwrap();
        assert!(facts(&seated).await.is_empty(), "it does not find it");
        let got = seated.get(&fact).await;
        assert!(matches!(got, Err(Error::NotFound(_))), "nor get it");
        // an envelope that reads lower than its own entry is classified takes no session
        let mut low = release();
        low.name = "pos-low".into();
        low.envelope.handling = Label::Public;
        in_force(&owner, &low).await;
        let why = denied(free.fill_position("pos-low", None).await);
        assert!(
            why.contains("above the handling public of its envelope"),
            "{why}"
        );
    }

    #[tokio::test]
    async fn the_views_of_a_session_share_its_seat() {
        let dir = tempfile::tempdir().unwrap();
        let (_owner, fact) = public_seat(dir.path()).await;
        let whole = session(dir.path(), "codex");
        // each view has its read scope from before the seat
        let view = whole.for_request().unwrap();
        assert_eq!(facts(&whole).await, std::slice::from_ref(&fact));
        view.fill_position(RELEASE, None).await.unwrap();
        assert_eq!(whole.seat().map(|f| f.position).as_deref(), Some(RELEASE));
        assert!(facts(&whole).await.is_empty());
        assert!(facts(&whole.for_request().unwrap()).await.is_empty());
        whole.vacate();
        assert_eq!(view.seat(), None);
        assert_eq!(facts(&view).await, [fact]);
    }

    /// The parts of a T0 that hold the position's slice: its commitments and its cache.
    fn slice_of<'a>(t0: &'a str, entry: &str) -> (&'a str, &'a str) {
        let at = |needle: &str| {
            t0.find(needle)
                .unwrap_or_else(|| panic!("{needle:?} is not in {t0}"))
        };
        let head = at(&format!("## Position {RELEASE} ({entry}, verified)"));
        let open = at("### Open commitments");
        let cache = at("### Handoff cache, read first");
        let now = at("## Now");
        assert!(t0[..head].lines().count() == 2, "right after the title");
        assert!(head < at(ORDERS) && at(ORDERS) < open);
        (&t0[open..cache], &t0[cache..now])
    }

    #[tokio::test]
    async fn the_occupant_s_handoff_cites_the_position_and_the_next_occupant_reads_it_first() {
        let dir = tempfile::tempdir().unwrap();
        let owner = root(dir.path());
        let entry = in_force(&owner, &release()).await;
        let commitment = admitted(&owner, pinned("Cut 1.2")).await;
        let first = session(dir.path(), "codex");
        first.fill_position(RELEASE, None).await.unwrap();
        let earlier = write(&first, handoff("The mirrors are checked.\n")).await;
        assert_eq!(cites_of(&owner, &earlier).await, [RELEASE]);
        // one that names the position is taken as it is, and other kinds get no cite
        let said = citing(handoff("The tag is not cut yet.\n"), RELEASE);
        let latest = write(&first, said).await;
        assert_eq!(cites_of(&owner, &latest).await, [RELEASE]);
        let mut fact = handoff("The mirror list has four hosts.\n");
        fact.entry_type = EntryType::Fact;
        let fact = write(&first, fact).await;
        assert!(cites_of(&owner, &fact).await.is_empty());
        let stray = session(dir.path(), "claude");
        let stray = write(&stray, handoff("Other work stopped here.\n")).await;
        assert!(cites_of(&owner, &stray).await.is_empty());

        let next = session(dir.path(), "codex");
        next.fill_position(RELEASE, None).await.unwrap();
        let view = next.position(RELEASE, None).await.unwrap();
        let cached: Vec<&str> = view.handoffs.iter().map(|h| h.id.as_str()).collect();
        assert_eq!(cached, [latest.as_str(), earlier.as_str()]);
        let owed: Vec<&str> = view.commitments.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(owed, [commitment.id.as_str()]);
        assert_eq!(view.seat, Some(SeatView { seconded: false }));
        let t0 = next.t0(Some(ME)).await.unwrap();
        let (open, cache) = slice_of(&t0, &entry);
        let line = format!("- `{}` open, continuity: Cut 1.2\n", commitment.id);
        assert!(open.contains(&line), "{open}");
        assert!(
            cache.contains(&latest) && cache.contains(&earlier),
            "{cache}"
        );
        assert!(!cache.contains(&stray), "{cache}");
        assert!(!t0.contains("seconded"));
        // a session in no position has no such section
        let plain = session(dir.path(), "claude").t0(Some(ME)).await.unwrap();
        assert!(!plain.contains("## Position") && !plain.contains("Open commitments"));
    }

    #[tokio::test]
    async fn a_position_shows_its_newest_five_handoffs_and_its_first_ten_commitments() {
        let dir = tempfile::tempdir().unwrap();
        let owner = root(dir.path());
        in_force(&owner, &release()).await;
        let occupant = session(dir.path(), "codex");
        occupant.fill_position(RELEASE, None).await.unwrap();
        let mut written = Vec::new();
        for n in 0..=CACHE_SIZE {
            written.push(write(&occupant, handoff(&format!("Stopped at step {n}.\n"))).await);
        }
        let mut filed = Vec::new();
        for n in 0..=COMMITMENTS {
            filed.push(admitted(&owner, pinned(&format!("Cut 1.{n}"))).await.id);
        }
        let view = occupant.position(RELEASE, None).await.unwrap();
        let cached: Vec<String> = view.handoffs.into_iter().map(|h| h.id).collect();
        written.reverse();
        assert_eq!(
            cached,
            written[..CACHE_SIZE],
            "newest first, the oldest left out"
        );
        let owed: Vec<String> = view.commitments.into_iter().map(|t| t.id).collect();
        assert_eq!(
            owed,
            filed[..COMMITMENTS],
            "in queue order, the last left out"
        );
    }

    #[test]
    fn the_slice_clips_long_orders_and_says_when_nothing_is_owed() {
        let mut p = release();
        p.charter.orders = "order ".repeat(400);
        let seat = Seated {
            position: p,
            seconded: false,
        };
        let text = slice(&seat, &[], &[]);
        let orders = text.lines().nth(2).unwrap();
        assert_eq!(orders.chars().count(), ORDERS_IN_T0, "{orders}");
        assert!(orders.ends_with('…'));
        assert!(text.contains("### Open commitments\n\nNone on the ledger.\n"));
        assert!(text.contains("read first\n\nNo handoff cites this position yet.\n"));
        assert!(text.contains("- Covers the task definitions: release\n"));
        assert!(text.contains("- Filled by the agent definition: codex\n"));
        assert!(text.contains("- Authority envelope: tools tool:fetch; handling restricted\n"));
        assert!(
            text.ends_with("\n\n"),
            "the next section starts on its own line"
        );
    }

    #[tokio::test]
    async fn a_seconded_session_reads_the_position_and_writes_to_its_task_only() {
        let dir = tempfile::tempdir().unwrap();
        let owner = root(dir.path());
        in_force(&owner, &release()).await;
        let commitment = admitted(&owner, pinned("Cut 1.2")).await;
        let substitute = session(dir.path(), "claude");
        let filled = substitute.fill_position(RELEASE, None).await.unwrap();
        assert!(
            filled.seconded,
            "another agent definition than the charter's"
        );
        let person = Memory::open(dir.path()).unwrap();
        assert!(person.fill_position(RELEASE, None).await.unwrap().seconded);
        // it takes the position's task and reads what the position holds
        let claim = substitute.claim_next(Some(ME), lease()).await.unwrap();
        let claim = claim.unwrap();
        assert_eq!(claim.task.id, commitment.id);
        let view = substitute.position(RELEASE, None).await.unwrap();
        assert_eq!(view.seat, Some(SeatView { seconded: true }));
        assert_eq!(view.sessions, [substitute.author()]);
        let t0 = substitute.t0(Some(ME)).await.unwrap();
        assert!(
            t0.contains("This session is seconded to the position"),
            "{t0}"
        );
        // its handoff goes to its task, and nothing of it to the position
        let report = write(&substitute, handoff("The tag is cut.\n")).await;
        assert!(cites_of(&owner, &report).await.is_empty());
        let why = denied(substitute.write(citing(handoff("More.\n"), RELEASE)).await);
        assert!(
            why.starts_with("pos-release is a position: only a session that fills it under the agent definition its charter names"),
            "{why}"
        );
        let generation = Some(claim.generation);
        let done = substitute
            .complete(&commitment.id, generation, &report)
            .await;
        assert_eq!(done.unwrap().handoff.as_deref(), Some(report.as_str()));
        let occupant = session(dir.path(), "codex");
        occupant.fill_position(RELEASE, None).await.unwrap();
        let view = occupant.position(RELEASE, None).await.unwrap();
        assert!(view.handoffs.is_empty(), "{:?}", view.handoffs);
    }

    #[tokio::test]
    async fn a_session_that_fills_no_position_writes_no_entry_that_cites_one() {
        let dir = tempfile::tempdir().unwrap();
        let owner = root(dir.path());
        in_force(&owner, &release()).await;
        let mut qa = release();
        qa.name = "pos-qa".into();
        in_force(&owner, &qa).await;
        let fact = |cite: &str| {
            let mut req = citing(handoff("The mirror list has four hosts.\n"), cite);
            req.entry_type = EntryType::Fact;
            req
        };
        let codex = session(dir.path(), "codex");
        let elsewhere = session(dir.path(), "codex");
        elsewhere.fill_position("pos-qa", None).await.unwrap();
        let read = elsewhere.position(RELEASE, None).await.unwrap();
        assert_eq!(read.seat, None, "its seat is in another position");
        for writer in [&owner, &codex, &elsewhere] {
            let why = denied(writer.write(fact(RELEASE)).await);
            assert!(why.starts_with("pos-release is a position"), "{why}");
        }
        // a name that no entry defines is a position's name all the same
        let why = denied(codex.write(fact("pos-nothere")).await);
        assert!(why.starts_with("pos-nothere is a position"), "{why}");
        assert!(facts(&owner).await.is_empty(), "nothing was written");
        // and the occupant writes its position's history in the position's domain only
        let mut abroad = fact("pos-qa");
        abroad.domain = Some(PLAT.into());
        denied(elsewhere.write(abroad).await);
        let id = write(&codex, fact("https://example.org/pos-1")).await;
        assert_eq!(facts(&owner).await, [id]);
    }

    #[tokio::test]
    async fn a_session_whose_position_changed_no_longer_acts_for_it() {
        let dir = tempfile::tempdir().unwrap();
        let owner = root(dir.path());
        let first = in_force(&owner, &release()).await;
        let commitment = admitted(&owner, pinned("Cut 1.2")).await;
        let seated = session(dir.path(), "codex");
        seated.fill_position(RELEASE, None).await.unwrap();
        // the owner replaces the entry: the next version waits for review
        let mut replacement = entry(&release());
        replacement.supersedes = Some(first);
        let second = write(&owner, replacement).await;
        stale(seated.claim_next(Some(ME), lease()).await);
        stale(seated.claim(&commitment.id, lease()).await);
        stale(seated.t0(Some(ME)).await);
        stale(seated.write(citing(handoff("More.\n"), RELEASE)).await);
        // its handoff is still written, to its task and not to the position
        let report = write(&seated, handoff("Stopped: the charter changed.\n")).await;
        assert!(cites_of(&owner, &report).await.is_empty());
        // the new version in force is another entry than the one this session filled
        verify(&owner, ME, &second).await;
        stale(seated.claim_next(Some(ME), lease()).await);
        let fresh = session(dir.path(), "codex");
        assert_eq!(
            fresh.fill_position(RELEASE, None).await.unwrap().entry,
            second
        );
        assert_eq!(next(&fresh).await, Some(commitment.id));
    }

    #[tokio::test]
    async fn a_session_that_leaves_its_position_is_held_to_its_grants_again() {
        let dir = tempfile::tempdir().unwrap();
        let (owner, fact) = public_seat(dir.path()).await;
        let bare = admitted(&owner, task("Tidy")).await;
        let seated = session(dir.path(), "codex");
        seated.fill_position(RELEASE, None).await.unwrap();
        assert_eq!(next(&seated).await, None, "its charter covers nothing");
        assert!(seated.capabilities().unwrap().is_empty());
        assert!(facts(&seated).await.is_empty());
        seated.vacate();
        assert_eq!(seated.seat(), None);
        assert_eq!(seated.capabilities().unwrap(), ["tool:fetch", "tool:x"]);
        assert_eq!(facts(&seated).await, [fact]);
        assert_eq!(next(&seated).await, Some(bare.id));
        seated.seat_allows("x").unwrap();
        let why = denied(seated.write(citing(handoff("More.\n"), RELEASE)).await);
        assert!(why.starts_with("pos-release is a position"), "{why}");
    }

    #[tokio::test]
    async fn a_retired_position_takes_no_session_and_no_pin() {
        let dir = tempfile::tempdir().unwrap();
        let owner = root(dir.path());
        let first = in_force(&owner, &release()).await;
        let mut retiring = release();
        retiring.state = State::Retired;
        retiring.charter.orders = "The release work has ended.".into();
        // a retiring entry by anyone but the owner retires nothing
        let codex = session(dir.path(), "codex");
        write(&codex, entry(&retiring)).await;
        codex.fill_position(RELEASE, None).await.unwrap();
        let mut ghost = retiring.clone();
        ghost.name = "pos-ghost".into();
        write(&codex, entry(&ghost)).await;
        let why = denied(
            owner
                .file_own(TaskSpec {
                    position: Some("pos-ghost".into()),
                    ..pinned("Nothing")
                })
                .await,
        );
        assert!(why.contains("no position pos-ghost is in force"), "{why}");
        // the owner's retiring version replaces the verified one, and counts at once
        let mut last = entry(&retiring);
        last.supersedes = Some(first);
        let last = write(&owner, last).await;
        let later = session(dir.path(), "codex");
        let fill = denied(later.fill_position(RELEASE, None).await);
        assert!(fill.contains("position pos-release is retired"), "{fill}");
        assert_eq!(denied(owner.file_own(pinned("Cut 1.3")).await), fill);
        stale(codex.claim_next(Some(ME), lease()).await);
        stale(codex.seat_allows("fetch"));
        let view = later.position(RELEASE, None).await.unwrap();
        assert_eq!(
            (view.position.state, view.position.entry),
            (State::Retired, last.clone())
        );
        // and it stays retired once a person verified that version
        verify(&owner, ME, &last).await;
        assert_eq!(denied(later.fill_position(RELEASE, None).await), fill);
    }

    const WHY: &str = "The release work moved to the build service.";

    /// The ids of every version of a position entry that `dom-me` holds, in force or
    /// replaced.
    async fn versions(m: &Memory) -> Vec<String> {
        let all = m.list_archive(Some(ME), 100).await.unwrap();
        all.into_iter()
            .filter(|h| h.entry_type == EntryType::Position)
            .map(|h| h.id)
            .collect()
    }

    /// The position a task is pinned to, as the ledger holds the task now.
    async fn pin_of(m: &Memory, id: &str) -> Option<String> {
        let task = m.ledger.lock().await.get(id).await.unwrap();
        assert_eq!(task.spec.position.is_some(), task.spec.pin_reason.is_some());
        task.spec.position
    }

    async fn state_of(m: &Memory) -> State {
        m.position(RELEASE, None).await.unwrap().position.state
    }

    #[test]
    fn a_new_status_changes_only_the_status_line_of_the_block() {
        let mut p = release();
        p.charter.orders = "Cut it.\nstatus = \"as the orders say\"\n[then] rest.".into();
        let block = p.block().unwrap();
        assert!(
            block.contains("\nstatus = \"as the orders say\"\n"),
            "{block}"
        );
        let fence = "```position pos-release\n";
        let commented = with(
            &block,
            fence,
            "```position pos-release\n# who cuts a release\n",
        );
        let body = format!("# The coordinator\r\n\n```toml\nstatus = \"x\"\n```\n\n{commented}\nLast words, no line end");
        let retired = with_status(&body, State::Retired).unwrap();
        let expected = with(
            &body,
            fence,
            "```position pos-release\nstatus = \"retired\"\n",
        );
        assert_eq!(retired, expected, "one line more, and nothing else");
        let read = |text: &str| position_in(&entry_of("position", text)).unwrap();
        assert_eq!(read(&retired).state, State::Retired);
        assert_eq!(read(&retired).charter, p.charter);
        // asked for the status it has, the body is the same; and the line goes again
        assert_eq!(with_status(&retired, State::Retired).unwrap(), retired);
        let comment = "# who cuts a release\n";
        let below = with(
            &body,
            comment,
            "# who cuts a release\nstatus = \"retired\"\n",
        );
        assert_eq!(with_status(&below, State::Retired).unwrap(), below);
        assert_eq!(with_status(&retired, State::Active).unwrap(), body);
        assert_eq!(with_status(&below, State::Active).unwrap(), body);
        // an active status that is written out is replaced
        let written = with(
            &body,
            fence,
            "```position pos-release\nstatus = \"active\"\n",
        );
        assert_eq!(with_status(&written, State::Retired).unwrap(), expected);
        // no block, two blocks, a block that does not read, and a status this cannot
        // set as one line of its own
        let quoted = with(
            &body,
            fence,
            "```position pos-release\n\"status\" = \"active\"\n",
        );
        assert_eq!(read(&quoted).state, State::Active, "it reads as a position");
        let dotted = "```position pos-release\ncharter.agent = \"codex\"\ncharter.orders = \"\"\"\nCut it.\nstatus = \"as the orders say\"\n\"\"\"\n\n[envelope]\nhandling = \"restricted\"\n```\n";
        assert!(
            read(dotted).charter.orders.contains("status = "),
            "it reads too"
        );
        let none = [
            "# The coordinator\n\nNo block.\n".to_string(),
            format!("{body}\n{block}"),
            with(&body, "agent = \"codex\"", "agent = \"\""),
            quoted,
            dotted.to_string(),
        ];
        for text in none {
            let refused = with_status(&text, State::Retired);
            assert!(refused.is_err(), "{refused:?}");
        }
    }

    #[tokio::test]
    async fn only_the_owner_retires_a_position_and_gives_a_reason_in_words() {
        let dir = tempfile::tempdir().unwrap();
        let owner = root(dir.path());
        let first = in_force(&owner, &release()).await;
        let commitment = admitted(&owner, pinned("Cut 1.2")).await;
        let codex = session(dir.path(), "codex");
        let why = denied(codex.retire_position(RELEASE, WHY, None).await);
        assert_eq!(why, "only the owner human:test:me retires a position");
        for words in [
            "",
            "  ",
            "Done.\nstatus",
            "```",
            "~~~",
            "<b>Done</b>",
            "a\rb",
        ] {
            let why = denied(owner.retire_position(RELEASE, words, None).await);
            assert!(
                why.starts_with("the reason to retire a position is words"),
                "{why}"
            );
        }
        // every task it takes the pin off is one the session may read
        let high = TaskSpec {
            classification: Label::Restricted,
            ..pinned("Cut 2.0")
        };
        let high = admitted(&owner, high).await;
        let capped = Memory::open(dir.path()).unwrap();
        let capped = capped.with_handling_cap(Label::Internal);
        let why = denied(capped.retire_position(RELEASE, WHY, None).await);
        assert_eq!(
            why,
            "position pos-release has pinned tasks this session may not read; it is not retired here"
        );
        // nothing was written and nothing was unpinned
        assert_eq!(versions(&owner).await, [first]);
        assert_eq!(state_of(&owner).await, State::Active);
        for id in [&commitment.id, &high.id] {
            assert_eq!(pin_of(&owner, id).await.as_deref(), Some(RELEASE));
        }
        // a name with no record in force is not there to retire
        let unverified = Position {
            name: "pos-qa".into(),
            ..release()
        };
        write(&owner, entry(&unverified)).await;
        for name in ["pos-qa", "pos-nothere"] {
            let missing = owner.retire_position(name, WHY, None).await;
            assert!(matches!(missing, Err(Error::NotFound(_))), "{name}");
        }
    }

    #[tokio::test]
    async fn a_retired_position_takes_nothing_new_and_its_history_stays_under_its_name() {
        let dir = tempfile::tempdir().unwrap();
        let owner = root(dir.path());
        let mut req = entry(&release());
        req.classification = Some(Label::Restricted);
        req.title = Some("The release coordinator, as chartered".into());
        let first = write(&owner, req).await;
        verify(&owner, ME, &first).await;
        let seated = session(dir.path(), "codex");
        seated.fill_position(RELEASE, None).await.unwrap();
        let report = write(&seated, handoff("The mirrors are checked.\n")).await;
        let old = owner.store.get(ME, &first).await.unwrap().unwrap();

        let fresh = Memory::open(dir.path()).unwrap();
        let retired = fresh.retire_position(RELEASE, WHY, None).await.unwrap();
        assert_eq!(
            (retired.position.as_str(), retired.domain.as_str()),
            (RELEASE, ME)
        );
        assert_eq!(retired.entry, first);
        assert!(retired.unpinned.is_empty());
        // the retiring version: the same entry with the status set and the reason
        let last = owner.store.get(ME, &retired.retired_by).await.unwrap();
        let last = last.unwrap();
        let body = with_status(&old.body, State::Retired).unwrap();
        assert_eq!(
            last.body,
            format!("{}\n\nRetired: {WHY}\n", body.trim_end())
        );
        assert_eq!(last.front.supersedes.as_deref(), Some(first.as_str()));
        assert_eq!(last.front.entry_type, EntryType::Position);
        assert_eq!(last.front.title, old.front.title);
        assert_eq!(last.front.classification, Label::Restricted);
        assert_eq!(versions(&owner).await.len(), 2);
        // it takes no session and no pin, and the session in it stops
        let later = session(dir.path(), "codex");
        let fill = denied(later.fill_position(RELEASE, None).await);
        assert!(fill.contains("position pos-release is retired"), "{fill}");
        assert_eq!(denied(owner.file_own(pinned("Cut 1.3")).await), fill);
        stale(seated.claim_next(Some(ME), lease()).await);
        // and what it was stays under its name
        let view = later.position(RELEASE, None).await.unwrap();
        assert_eq!(view.position.state, State::Retired);
        assert_eq!(view.position.entry, retired.retired_by);
        assert_eq!(view.position.charter.orders, ORDERS);
        let cached: Vec<&str> = view.handoffs.iter().map(|h| h.id.as_str()).collect();
        assert_eq!(cached, [report.as_str()]);
    }

    #[tokio::test]
    async fn the_open_tasks_of_a_retired_position_return_to_matching() {
        let dir = tempfile::tempdir().unwrap();
        let owner = root(dir.path());
        in_force(&owner, &release()).await;
        let seated = session(dir.path(), "codex");
        seated.fill_position(RELEASE, None).await.unwrap();
        let done = admitted(&owner, pinned("Cut 1.0")).await;
        let claim = seated.claim(&done.id, lease()).await.unwrap();
        let report = write(&seated, handoff("The tag is cut.\n")).await;
        let closed = seated.complete(&done.id, Some(claim.generation), &report);
        closed.await.unwrap();
        let held = admitted(&owner, pinned("Cut 1.1")).await;
        seated.claim(&held.id, lease()).await.unwrap();
        let open = admitted(&owner, pinned("Cut 1.2")).await;
        let waits_on_open = TaskSpec {
            depends_on: vec![open.id.clone()],
            ..pinned("Announce 1.2")
        };
        let blocked = owner.file_own(waits_on_open).await.unwrap().task;
        let plain = session(dir.path(), "claude");
        let proposed = plain.create_task(pinned("Cut 1.3")).await.unwrap().task;
        let before = [
            (&held, Status::Claimed),
            (&open, Status::Open),
            (&blocked, Status::Blocked),
            (&proposed, Status::Proposed),
        ];
        for (task, status) in before {
            let now = owner.ledger.lock().await.get(&task.id).await.unwrap();
            assert_eq!(now.status, status, "{}", task.spec.title);
        }
        assert!(
            plain.claim(&open.id, lease()).await.is_err(),
            "it is pinned"
        );

        let retired = owner.retire_position(RELEASE, WHY, None).await.unwrap();
        // in queue order: the task that another one waits on ranks first
        let expected = [&open.id, &held.id, &blocked.id, &proposed.id];
        assert_eq!(retired.unpinned.iter().collect::<Vec<_>>(), expected);
        for (task, status) in before {
            let now = owner.ledger.lock().await.get(&task.id).await.unwrap();
            assert_eq!(now.status, status, "{}", task.spec.title);
            assert_eq!(pin_of(&owner, &task.id).await, None, "{}", task.spec.title);
        }
        // the ledger keeps one fixed sentence for each, and none of the owner's words
        let log = rusqlite::Connection::open(dir.path().join(crate::LEDGER_FILE)).unwrap();
        let mut unpinned = log
            .prepare("SELECT payload FROM events WHERE kind = 'unpinned'")
            .unwrap();
        let whys: Vec<String> = unpinned
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(whys.len(), 4, "{whys:?}");
        for payload in &whys {
            assert!(
                payload.contains("\"position pos-release was retired\"") && !payload.contains(WHY),
                "{payload}"
            );
        }
        // what the position finished is its history: that pin stays on the record
        assert_eq!(pin_of(&owner, &done.id).await.as_deref(), Some(RELEASE));
        let taken = plain.claim(&open.id, lease()).await.unwrap();
        assert_eq!(taken.task.id, open.id);
        let view = owner.position(RELEASE, None).await.unwrap();
        assert!(view.commitments.is_empty(), "{:?}", view.commitments);
    }

    #[tokio::test]
    async fn a_retirement_asked_again_writes_no_second_version_and_finishes_the_first() {
        let dir = tempfile::tempdir().unwrap();
        let owner = root(dir.path());
        // two verified versions, neither of which replaced the other
        let older = in_force(&owner, &release()).await;
        let mut wider = release();
        wider.charter.covers.push("triage".into());
        let newer = in_force(&owner, &wider).await;
        // and a version that no person verified, which a retirement leaves as it is
        let draft = write(&owner, entry(&release())).await;
        let first = owner.retire_position(RELEASE, WHY, None).await.unwrap();
        assert_eq!(
            first.entry, newer,
            "the version in force is the one retired"
        );
        assert_eq!(state_of(&owner).await, State::Retired);
        let written = versions(&owner).await;
        assert_eq!(written.len(), 5, "a retiring version for each: {written:?}");
        let replaced_older = owner.store.get(ME, &older).await.unwrap().unwrap();
        assert!(!replaced_older.is_active(), "no older version comes back");
        let draft = owner.store.get(ME, &draft).await.unwrap().unwrap();
        assert!(draft.is_active(), "nobody replaced the draft");
        // a task that is still pinned, as after a retirement that stopped half way
        let left = owner.ledger.lock().await.create(pinned("Cut 1.4")).await;
        let left = left.unwrap().task;
        let again = owner.retire_position(RELEASE, "Asked again.", None).await;
        let again = again.unwrap();
        assert_eq!(again.unpinned, [left.id.as_str()]);
        assert_eq!(pin_of(&owner, &left.id).await, None);
        assert_eq!(
            (&again.entry, &again.retired_by),
            (&first.entry, &first.retired_by)
        );
        assert_eq!(versions(&owner).await, written);
        // nor once a person verified the retiring version
        verify(&owner, ME, &first.retired_by).await;
        let third = owner.retire_position(RELEASE, "And again.", None).await;
        let third = third.unwrap();
        assert_eq!(third.retired_by, first.retired_by);
        assert!(third.unpinned.is_empty());
        assert_eq!(versions(&owner).await, written);
    }

    #[tokio::test]
    async fn a_retiring_entry_an_agent_writes_for_a_verified_position_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let owner = root(dir.path());
        let first = in_force(&owner, &release()).await;
        let commitment = admitted(&owner, pinned("Cut 1.2")).await;
        let mut retiring = release();
        retiring.state = State::Retired;
        retiring.charter.orders = "The release work has ended.".into();
        let codex = session(dir.path(), "codex");
        let stray = write(&codex, entry(&retiring)).await;
        assert_eq!(state_of(&owner).await, State::Active);
        let rows = owner.positions(Some(ME)).await.unwrap();
        let states: Vec<(&str, Standing, usize)> = rows
            .iter()
            .map(|r| (r.entry.as_str(), r.state, r.pinned))
            .collect();
        let expected = [
            (stray.as_str(), Standing::NotInForce, 0),
            (first.as_str(), Standing::Active, 1),
        ];
        assert_eq!(states, expected);
        assert_eq!(
            pin_of(&owner, &commitment.id).await.as_deref(),
            Some(RELEASE)
        );
        let filled = codex.fill_position(RELEASE, None).await.unwrap();
        assert_eq!(filled.entry, first);
        assert_eq!(next(&codex).await, Some(commitment.id));
    }

    fn wake(entry: &str, tasks: &[&Task]) -> Wake {
        Wake {
            position: RELEASE.into(),
            domain: ME.into(),
            entry: entry.into(),
            agent: "codex".into(),
            tasks: tasks.iter().map(|t| t.id.clone()).collect(),
        }
    }

    #[tokio::test]
    async fn the_wake_list_names_a_position_with_an_admitted_task_and_no_session() {
        let dir = tempfile::tempdir().unwrap();
        let owner = root(dir.path());
        let entry = in_force(&owner, &release()).await;
        assert!(owner.wake_list(None).await.unwrap().is_empty());
        // a task that is only proposed, and one that waits for another, wake nobody
        let plain = session(dir.path(), "claude");
        let proposed = plain.create_task(pinned("Cut 1.3")).await.unwrap().task;
        assert_eq!(proposed.status, Status::Proposed);
        let other = admitted(&owner, task("Tidy")).await;
        let waits_on_other = TaskSpec {
            depends_on: vec![other.id.clone()],
            ..pinned("Announce 1.2")
        };
        let blocked = owner.file_own(waits_on_other).await.unwrap().task;
        assert_eq!(blocked.status, Status::Blocked);
        assert!(owner.wake_list(None).await.unwrap().is_empty());
        let rows = owner.positions(None).await.unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!((row.pinned, row.wake), (2, false));
        assert!(row.waiting.is_empty(), "{:?}", row.waiting);
        // an admitted one does, and the list says under which agent definition
        let second = TaskSpec {
            priority: -1,
            ..pinned("Cut 1.2, the notes")
        };
        let second = admitted(&owner, second).await;
        let cut = admitted(&owner, pinned("Cut 1.2")).await;
        let list = owner.wake_list(None).await.unwrap();
        assert_eq!(list, [wake(&entry, &[&cut, &second])], "in queue order");
        assert_eq!(owner.wake_list(Some(ME)).await.unwrap(), list);
        assert!(owner.wake_list(Some(PLAT)).await.unwrap().is_empty());
        // a retired position is not woken for a task that is still pinned to it
        owner.retire_position(RELEASE, WHY, None).await.unwrap();
        let left = {
            let mut l = owner.ledger.lock().await;
            let left = l.create(pinned("Cut 1.4")).await.unwrap().task;
            l.admit(&left.id).await.unwrap()
        };
        let rows = owner.positions(None).await.unwrap();
        let row = &rows[0];
        assert_eq!((row.state, row.pinned), (Standing::Retired, 1));
        assert_eq!(row.waiting, [left.id]);
        assert!(!row.wake);
        assert!(owner.wake_list(None).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_entries_a_listing_shows_and_the_tasks_it_counts_are_read() {
        let dir = tempfile::tempdir().unwrap();
        let owner = root(dir.path());
        let mut public = entry(&release());
        public.classification = Some(Label::Public);
        let id = write(&owner, public).await;
        verify(&owner, ME, &id).await;
        let reader = || session(dir.path(), "claude");
        let (first, second, third) = (reader(), reader(), reader());
        assert_eq!(first.positions(None).await.unwrap().len(), 1);
        assert_eq!(first.label(), Label::Public);
        // a task that is counted is read
        let internal = TaskSpec {
            classification: Label::Internal,
            ..pinned("Cut 1.2")
        };
        admitted(&owner, internal).await;
        assert_eq!(second.positions(None).await.unwrap()[0].pinned, 1);
        assert_eq!(second.label(), Label::Internal);
        // and so is an entry that is listed, in force or not
        let elsewhere = Position {
            domain: PLAT.into(),
            ..release()
        };
        write(&owner, entry(&elsewhere)).await;
        assert_eq!(third.positions(Some(PLAT)).await.unwrap().len(), 1);
        assert_eq!(third.label(), Label::Internal);
    }

    #[tokio::test]
    async fn the_wake_list_leaves_out_a_position_while_a_session_in_it_holds_a_live_lease() {
        let dir = tempfile::tempdir().unwrap();
        let owner = root(dir.path());
        let entry = in_force(&owner, &release()).await;
        let first = admitted(&owner, pinned("Cut 1.2")).await;
        let second = admitted(&owner, pinned("Cut 1.3")).await;
        let seated = session(dir.path(), "codex");
        seated.fill_position(RELEASE, None).await.unwrap();
        assert_eq!(next(&seated).await, Some(first.id.clone()));
        // a task waits, and the session that is live in the position takes it next
        let rows = owner.positions(None).await.unwrap();
        let row = &rows[0];
        assert_eq!(row.sessions, [seated.author()]);
        assert_eq!(row.waiting, [second.id.as_str()]);
        assert_eq!((row.pinned, row.wake), (2, false));
        assert!(owner.wake_list(None).await.unwrap().is_empty());
        // the lease runs out: nobody is live in the position, and both tasks wait
        let later = OffsetDateTime::now_utc() + lease() + time::Duration::minutes(1);
        owner.ledger.lock().await.set_clock(move || later);
        let rows = owner.positions(None).await.unwrap();
        let row = &rows[0];
        assert!(row.sessions.is_empty(), "{:?}", row.sessions);
        assert_eq!(row.waiting, [first.id.as_str(), second.id.as_str()]);
        assert!(row.wake);
        let list = owner.wake_list(None).await.unwrap();
        assert_eq!(list, [wake(&entry, &[&first, &second])]);
    }

    #[tokio::test]
    async fn the_listing_says_why_an_entry_is_not_in_force_and_shows_what_the_session_may_read() {
        let dir = tempfile::tempdir().unwrap();
        let owner = root(dir.path());
        let first = in_force(&owner, &release()).await;
        let second = in_force(&owner, &release()).await;
        let qa = Position {
            name: "pos-qa".into(),
            ..release()
        };
        let unverified = write(&owner, entry(&qa)).await;
        let mut broken = entry(&release());
        broken.body = with(&broken.body, "agent = \"codex\"", "agent = \"\"");
        let broken = write(&owner, broken).await;
        let seen = admitted(&owner, pinned("Cut 1.2")).await;
        let high = TaskSpec {
            classification: Label::Restricted,
            ..pinned("Cut 2.0")
        };
        let high = admitted(&owner, high).await;

        let low = session(dir.path(), "claude").with_handling_cap(Label::Internal);
        let rows = low.positions(None).await.unwrap();
        let none = |entry: &str, name: &str, agent: &str, why: &str| ListedPosition {
            name: name.into(),
            entry: entry.into(),
            domain: ME.into(),
            state: Standing::NotInForce,
            agent: agent.into(),
            pinned: 0,
            waiting: Vec::new(),
            sessions: Vec::new(),
            wake: false,
            why_not: Some(why.into()),
        };
        let active = |tasks: &[&Task]| ListedPosition {
            name: RELEASE.into(),
            entry: second.clone(),
            domain: ME.into(),
            state: Standing::Active,
            agent: "codex".into(),
            pinned: tasks.len(),
            waiting: tasks.iter().map(|t| t.id.clone()).collect(),
            sessions: Vec::new(),
            wake: true,
            why_not: None,
        };
        let raw = "its entry is raw; a position is in force once a person verifies it";
        let expected = [
            none(&broken, "", "", "its block does not read as a position"),
            none(&unverified, "pos-qa", "codex", raw),
            active(&[&seen]),
            none(
                &first,
                RELEASE,
                "codex",
                "a newer verified entry defines pos-release",
            ),
        ];
        assert_eq!(rows, expected, "newest first, and one task is out of sight");
        let json = serde_json::to_value(&rows).unwrap();
        assert_eq!(json[0]["state"], "none");
        assert_eq!(json[2]["state"], "active");
        assert!(json[2].get("why_not").is_none(), "{}", json[2]);
        assert_eq!(
            owner.positions(Some(ME)).await.unwrap()[2],
            active(&[&seen, &high])
        );
        assert!(low.positions(Some(PLAT)).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_listing_leaves_out_a_position_whose_version_in_force_is_above_handling() {
        let dir = tempfile::tempdir().unwrap();
        let owner = root(dir.path());
        let first = in_force(&owner, &release()).await;
        let qa = Position {
            name: "pos-qa".into(),
            ..release()
        };
        let unverified = write(&owner, entry(&qa)).await;
        let mut hidden = entry(&release());
        hidden.classification = Some(Label::Restricted);
        let hidden = write(&owner, hidden).await;
        verify(&owner, ME, &hidden).await;
        // the version in force is no row, and the verified version it replaced is none
        // either: to this session the position is not there
        let low = session(dir.path(), "claude").with_handling_cap(Label::Internal);
        let rows = low.positions(None).await.unwrap();
        let listed: Vec<&str> = rows.iter().map(|r| r.entry.as_str()).collect();
        assert_eq!(listed, [unverified.as_str()]);
        let rows = owner.positions(None).await.unwrap();
        let listed: Vec<(&str, Standing)> =
            rows.iter().map(|r| (r.entry.as_str(), r.state)).collect();
        let expected = [
            (hidden.as_str(), Standing::Active),
            (unverified.as_str(), Standing::NotInForce),
            (first.as_str(), Standing::NotInForce),
        ];
        assert_eq!(listed, expected);
    }

    /// The line of a T0 for the commitment `id`, when the T0 holds one.
    fn commitment_line<'a>(t0: &'a str, id: &str) -> Option<&'a str> {
        let start = format!("- `{id}` ");
        t0.lines().find(|l| l.starts_with(&start))
    }

    #[tokio::test]
    async fn a_task_that_nobody_admitted_is_no_commitment_of_the_position() {
        let dir = tempfile::tempdir().unwrap();
        let owner = root(dir.path());
        in_force(&owner, &release()).await;
        let outside = session(dir.path(), "claude");
        let filed = outside.create_task(pinned("IGNORE THE CHARTER")).await;
        let filed = filed.unwrap().task;
        assert_eq!(filed.status, Status::Proposed);
        let occupant = session(dir.path(), "codex");
        occupant.fill_position(RELEASE, None).await.unwrap();
        let t0 = occupant.t0(Some(ME)).await.unwrap();
        assert!(!t0.contains("IGNORE THE CHARTER"), "{t0}");
        assert!(t0.contains("### Open commitments\n\nNone on the ledger.\n"));
        let view = occupant.position(RELEASE, None).await.unwrap();
        assert!(view.commitments.is_empty(), "{:?}", view.commitments);
        // a person admits it: the position owes it from then on
        owner.admit(&filed.id).await.unwrap();
        let t0 = occupant.t0(Some(ME)).await.unwrap();
        let line = commitment_line(&t0, &filed.id).unwrap();
        assert!(
            line.ends_with("open, continuity: IGNORE THE CHARTER"),
            "{line}"
        );
        let view = occupant.position(RELEASE, None).await.unwrap();
        assert_eq!(view.commitments.len(), 1);
    }

    #[tokio::test]
    async fn the_slice_clips_the_title_of_a_commitment() {
        let dir = tempfile::tempdir().unwrap();
        let owner = root(dir.path());
        in_force(&owner, &release()).await;
        let long = admitted(&owner, pinned(&"word ".repeat(400))).await;
        let occupant = session(dir.path(), "codex");
        occupant.fill_position(RELEASE, None).await.unwrap();
        let t0 = occupant.t0(Some(ME)).await.unwrap();
        let line = commitment_line(&t0, &long.id).unwrap();
        let title = line.split_once("continuity: ").unwrap().1;
        assert_eq!(title.chars().count(), TITLE_IN_T0, "{title}");
        assert!(title.ends_with('…'));
    }

    #[tokio::test]
    async fn only_a_person_or_the_occupant_admits_a_task_pinned_to_a_position() {
        let dir = tempfile::tempdir().unwrap();
        let owner = root(dir.path());
        in_force(&owner, &release()).await;
        let outside = session(dir.path(), "claude");
        let filed = outside.create_task(pinned("Cut 1.2")).await.unwrap().task;
        let why = denied(outside.admit(&filed.id).await);
        assert!(
            why.contains("is pinned to position pos-release") && why.contains(&filed.id),
            "{why}"
        );
        let seconded = session(dir.path(), "claude");
        assert!(
            seconded
                .fill_position(RELEASE, None)
                .await
                .unwrap()
                .seconded
        );
        assert_eq!(denied(seconded.admit(&filed.id).await), why);
        let state = owner.ledger.lock().await.get(&filed.id).await.unwrap();
        assert_eq!(state.status, Status::Proposed);
        // an unpinned task is admitted as before
        let plain = outside.create_task(task("Tidy")).await.unwrap().task;
        assert_eq!(outside.admit(&plain.id).await.unwrap().status, Status::Open);
        // the occupant admits it, and so does a person
        let occupant = session(dir.path(), "codex");
        occupant.fill_position(RELEASE, None).await.unwrap();
        assert_eq!(
            occupant.admit(&filed.id).await.unwrap().status,
            Status::Open
        );
        let second = outside.create_task(pinned("Cut 1.3")).await.unwrap().task;
        assert_eq!(owner.admit(&second.id).await.unwrap().status, Status::Open);
    }

    #[tokio::test]
    async fn a_pinned_task_is_admitted_as_filed_only_for_the_occupant() {
        let dir = tempfile::tempdir().unwrap();
        let owner = root(dir.path());
        in_force(&owner, &release()).await;
        // a session in no position holds a task: the domain admits what it files from it
        let tidy = admitted(&owner, task("Tidy")).await;
        let outside = session(dir.path(), "claude");
        outside.claim(&tidy.id, lease()).await.unwrap();
        outside.set_held_task(&tidy.id);
        let plain = outside.create_task(task("Tidy more")).await.unwrap().task;
        assert_eq!(plain.status, Status::Open, "the domain admits its own");
        let slipped = outside.create_task(pinned("Slip in")).await.unwrap().task;
        assert_eq!(slipped.spec.position.as_deref(), Some(RELEASE));
        assert_eq!(slipped.status, Status::Proposed);
        // the occupant files a commitment of its own position from the task it holds
        let commitment = admitted(&owner, pinned("Cut 1.2")).await;
        let occupant = session(dir.path(), "codex");
        occupant.fill_position(RELEASE, None).await.unwrap();
        occupant.claim(&commitment.id, lease()).await.unwrap();
        occupant.set_held_task(&commitment.id);
        let own = occupant
            .create_task(pinned("Announce 1.2"))
            .await
            .unwrap()
            .task;
        assert_eq!(own.status, Status::Open);
    }

    fn sub(title: &str) -> Vec<(String, String)> {
        vec![(title.to_string(), format!("{title}, and report."))]
    }

    #[tokio::test]
    async fn only_a_session_that_works_a_pinned_task_fans_it_out() {
        let dir = tempfile::tempdir().unwrap();
        let owner = root(dir.path());
        in_force(&owner, &release()).await;
        let outside = session(dir.path(), "claude");
        let filed = outside.create_task(pinned("Slip in")).await.unwrap().task;
        let items = sub("Check the mirrors");
        let why = denied(outside.fan_out(&filed.id, &items).await);
        assert!(why.contains("is pinned to position pos-release"), "{why}");
        // nor a commitment that it does not hold, which stays the position's to claim
        let commitment = admitted(&owner, pinned("Cut 1.2")).await;
        let other = denied(outside.fan_out(&commitment.id, &items).await);
        assert_eq!(other, why.replace(&filed.id, &commitment.id));
        for parent in [&filed.id, &commitment.id] {
            let filed = owner.ledger.lock().await.filed_from(parent).await;
            assert!(filed.unwrap().is_empty(), "nothing was filed");
        }
        // a seconded session works the position's task: it writes to the task chain
        let seconded = session(dir.path(), "claude");
        seconded.fill_position(RELEASE, None).await.unwrap();
        seconded.claim(&commitment.id, lease()).await.unwrap();
        let fanned = seconded.fan_out(&commitment.id, &items).await.unwrap();
        let resume = fanned.continuation.unwrap();
        assert_eq!(resume.spec.position.as_deref(), Some(RELEASE));
        assert_eq!(resume.status, Status::Blocked, "admitted, and it waits");
    }

    #[tokio::test]
    async fn only_a_session_that_works_a_pinned_task_files_its_continuation() {
        let dir = tempfile::tempdir().unwrap();
        let owner = root(dir.path());
        in_force(&owner, &release()).await;
        let commitment = admitted(&owner, pinned("Cut 1.2")).await;
        let outside = session(dir.path(), "claude");
        let why = denied(outside.create_task(resumes(&commitment.id)).await);
        assert!(why.contains("is pinned to position pos-release"), "{why}");
        let filed = owner.ledger.lock().await.filed_from(&commitment.id).await;
        assert!(filed.unwrap().is_empty(), "nothing was filed");
        // the session that works the task files one: a commitment once it is admitted
        let seconded = session(dir.path(), "claude");
        seconded.fill_position(RELEASE, None).await.unwrap();
        seconded.claim(&commitment.id, lease()).await.unwrap();
        let resume = seconded.create_task(resumes(&commitment.id)).await;
        let resume = resume.unwrap().task;
        assert_eq!(resume.spec.position.as_deref(), Some(RELEASE));
        assert_eq!(resume.status, Status::Proposed);
    }

    #[tokio::test]
    async fn a_session_whose_position_changed_reads_nothing_and_calls_no_tool() {
        let dir = tempfile::tempdir().unwrap();
        let owner = root(dir.path());
        in_force(&owner, &release()).await;
        let mut fact = handoff("The staging host is alder.\n");
        fact.entry_type = EntryType::Fact;
        let fact = write(&owner, fact).await;
        let seated = Arc::new(session(dir.path(), "codex"));
        seated.fill_position(RELEASE, None).await.unwrap();
        assert_eq!(facts(&seated).await, std::slice::from_ref(&fact));
        seated.seat_allows("fetch").unwrap();
        // the owner narrows the envelope, and a person verifies the new version
        let mut narrow = release();
        narrow.envelope.tools.clear();
        narrow.envelope.handling = Label::Internal;
        let second = in_force(&owner, &narrow).await;
        stale(seated.list(Some(ME), Some(EntryType::Fact), 20).await);
        stale(seated.get(&fact).await);
        stale(seated.seat_allows("fetch"));
        stale(seated.use_tool("fetch", serde_json::json!({})).await);
        stale(seated.capabilities());
        // it leaves the position, and is held to its grants again
        seated.vacate();
        assert_eq!(facts(&seated).await, std::slice::from_ref(&fact));
        // a new session fills the version in force, under the envelope of that version
        let fresh = session(dir.path(), "codex");
        let filled = fresh.fill_position(RELEASE, None).await.unwrap();
        assert_eq!(filled.entry, second);
        let why = denied(fresh.seat_allows("fetch"));
        assert!(why.contains("does not hold tool:fetch"), "{why}");
    }

    /// A position with one restricted task, which a session in the position holds.
    /// Returns the owner's session and the author of the holder.
    async fn held_out_of_sight(dir: &Path) -> (Memory, String) {
        let owner = root(dir);
        in_force(&owner, &release()).await;
        let high = TaskSpec {
            classification: Label::Restricted,
            ..pinned("Cut 2.0")
        };
        let high = admitted(&owner, high).await;
        let seated = session(dir, "codex");
        seated.fill_position(RELEASE, None).await.unwrap();
        seated.claim(&high.id, lease()).await.unwrap();
        (owner, seated.author().to_string())
    }

    #[tokio::test]
    async fn a_listing_names_no_holder_of_a_task_that_its_reader_may_not_see() {
        let dir = tempfile::tempdir().unwrap();
        let (owner, holder) = held_out_of_sight(dir.path()).await;
        let low = session(dir.path(), "claude").with_handling_cap(Label::Internal);
        let rows = low.positions(None).await.unwrap();
        assert!(rows[0].sessions.is_empty(), "{:?}", rows[0].sessions);
        assert_eq!((rows[0].pinned, rows[0].wake), (0, false));
        assert_eq!(
            low.label(),
            Label::Internal,
            "it read the entry and no task"
        );
        let rows = owner.positions(None).await.unwrap();
        assert_eq!(rows[0].sessions, [holder]);
    }

    #[tokio::test]
    async fn a_position_names_no_holder_of_a_task_that_its_reader_may_not_see() {
        let dir = tempfile::tempdir().unwrap();
        let (owner, holder) = held_out_of_sight(dir.path()).await;
        let low = session(dir.path(), "claude").with_handling_cap(Label::Internal);
        let view = low.position(RELEASE, None).await.unwrap();
        assert!(view.sessions.is_empty(), "{:?}", view.sessions);
        assert!(view.commitments.is_empty());
        let view = owner.position(RELEASE, None).await.unwrap();
        assert_eq!(view.sessions, [holder]);
    }

    /// A position whose version in force is restricted, with two tasks pinned to it:
    /// one from before that version, which is classified lower, and one from after.
    /// Returns the owner's session and the two tasks.
    async fn reclassified(dir: &Path) -> (Memory, Task, Task) {
        let owner = root(dir);
        in_force(&owner, &release()).await;
        let earlier = admitted(&owner, pinned("Cut 1.2")).await;
        assert!(earlier.spec.classification < Label::Restricted);
        let mut high = entry(&release());
        high.classification = Some(Label::Restricted);
        let high = write(&owner, high).await;
        verify(&owner, ME, &high).await;
        let later = admitted(&owner, pinned("Cut 1.3")).await;
        assert_eq!(later.spec.classification, Label::Restricted);
        (owner, earlier, later)
    }

    /// A task that continues `parent` and names no position.
    fn resumes(parent: &str) -> TaskSpec {
        TaskSpec {
            origin: Some(dm_ledger::continues(parent)),
            ..task("Resume the cut")
        }
    }

    #[tokio::test]
    async fn a_continuation_takes_a_pin_only_to_a_position_that_its_filer_may_read() {
        let dir = tempfile::tempdir().unwrap();
        let (owner, earlier, _) = reclassified(dir.path()).await;
        // a person who reads no higher than internal: the position's entry is out of sight
        let low = Memory::open(dir.path()).unwrap();
        let low = low.with_handling_cap(Label::Internal);
        let inherited = denied(low.create_task(resumes(&earlier.id)).await);
        assert_eq!(inherited, denied(low.create_task(pinned("Direct")).await));
        assert!(inherited.contains("no position pos-release is in force"));
        let filed = owner.ledger.lock().await.filed_from(&earlier.id).await;
        assert!(filed.unwrap().is_empty(), "nothing was filed");
    }

    #[tokio::test]
    async fn a_continuation_that_takes_a_pin_is_classified_as_the_position_is() {
        let dir = tempfile::tempdir().unwrap();
        let (_owner, earlier, _) = reclassified(dir.path()).await;
        let high = Memory::open(dir.path()).unwrap();
        assert_eq!(high.label(), Label::Public);
        let filed = high.create_task(resumes(&earlier.id)).await.unwrap().task;
        assert_eq!(filed.spec.classification, Label::Restricted);
        assert_eq!(filed.spec.position.as_deref(), Some(RELEASE));
        assert_eq!(filed.spec.pin_reason, Some(PinReason::Continuity));
        assert_eq!(high.label(), Label::Restricted, "the filer read the entry");
    }

    #[tokio::test]
    async fn a_session_continues_no_task_that_it_may_not_read() {
        let dir = tempfile::tempdir().unwrap();
        let (owner, _, later) = reclassified(dir.path()).await;
        let low = session(dir.path(), "claude").with_handling_cap(Label::Internal);
        let why = denied(low.create_task(resumes(&later.id)).await);
        assert!(
            !why.contains("pos-") && !why.contains("Cut 1.3"),
            "it names nothing of the task: {why}"
        );
        let filed = owner.ledger.lock().await.filed_from(&later.id).await;
        assert!(filed.unwrap().is_empty(), "nothing was filed");
        // a task that the ledger does not hold is continued as before, with no pin
        let loose = low.create_task(resumes("task-me-none")).await.unwrap().task;
        assert_eq!(loose.spec.position, None);
    }

    #[tokio::test]
    async fn the_wake_list_leaves_out_a_position_that_no_session_can_fill() {
        let dir = tempfile::tempdir().unwrap();
        let owner = root(dir.path());
        let mut low = release();
        low.envelope.handling = Label::Public;
        in_force(&owner, &low).await;
        let waits = admitted(&owner, pinned("Cut 1.2")).await;
        assert!(owner.wake_list(None).await.unwrap().is_empty());
        let rows = owner.positions(None).await.unwrap();
        assert_eq!(rows[0].waiting, [waits.id.as_str()]);
        assert!(!rows[0].wake);
        let why = denied(
            session(dir.path(), "codex")
                .fill_position(RELEASE, None)
                .await,
        );
        assert!(why.contains("above the handling public of its envelope"));
    }

    #[tokio::test]
    async fn the_owner_that_retires_the_position_it_fills_leaves_it() {
        let dir = tempfile::tempdir().unwrap();
        let owner = root(dir.path());
        in_force(&owner, &release()).await;
        let commitment = admitted(&owner, pinned("Cut 1.2")).await;
        assert!(owner.fill_position(RELEASE, None).await.unwrap().seconded);
        let retired = owner.retire_position(RELEASE, WHY, None).await.unwrap();
        assert_eq!(retired.unpinned, [commitment.id.as_str()]);
        assert_eq!(owner.seat(), None, "no position is there to fill");
        let rows = owner.positions(None).await.unwrap();
        assert_eq!(rows[0].state, Standing::Retired);
    }
}
