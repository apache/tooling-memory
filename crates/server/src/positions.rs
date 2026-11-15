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
//! A position is in force only from an active entry a person verified, the newest one,
//! as a task definition is. A session fills a position through
//! [`Memory::fill_position`]: it then claims the tasks pinned to the position and the
//! unpinned ones its charter covers, uses no tool and reads no entry outside the
//! envelope, and gets the position's slice in T0. The position's history is the
//! entries whose `cites` hold its name, and only a session that fills it under the
//! charter's agent definition writes one. A session under another agent definition is
//! seconded: it reads what the position holds and writes to its task.
//!
//! Two limits, both of scale and of import: an imported bundle lands its entries with
//! their citations, so it can bring entries that cite a position; and the handoff
//! cache walks the newest [`CACHE_WALK`] handoffs of the domain at each read.

use crate::{lock, Error, Memory};
use dm_core::{Entry, EntryType, Label, ReadFilter, TrustTier};
use dm_index::Hit;
use dm_ledger::{Task, TaskSpec};
use serde::{Deserialize, Serialize};

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

/// The `position <name>` blocks of a body, each with its text. A fence inside a longer
/// fence is text of that one, and opens nothing.
fn blocks_in(body: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    // the fence that is open: its length, the name when it is a position's, its lines
    let mut open: Option<(usize, Option<String>, Vec<&str>)> = None;
    for line in body.lines() {
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
                open = Some((ticks, name, Vec::new()));
            }
            None => {}
            Some((fence, name, lines)) => {
                if ticks >= *fence && rest.trim().is_empty() {
                    if let Some(name) = name.take() {
                        out.push((name, lines.join("\n")));
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

/// The position an entry defines: an entry of type `position` with one `position`
/// block that reads. The block reads when its name is a position's, it has no key this
/// code does not know, its agent is one word, and every tool is `tool:<name>`. Whether
/// the entry is in force is the caller's to ask.
#[must_use]
pub fn position_in(e: &Entry) -> Option<Position> {
    if e.front.entry_type != EntryType::Position {
        return None;
    }
    let mut blocks = blocks_in(&e.body);
    let (name, text) = blocks.pop().filter(|_| blocks.is_empty())?;
    let block: Block = toml::from_str(&text).ok()?;
    let agent = &block.charter.agent;
    let reads = is_name(&name)
        && !agent.is_empty()
        && !agent.contains(char::is_whitespace)
        && block
            .envelope
            .tools
            .iter()
            .all(|t| t.starts_with("tool:") && t.len() > 5);
    reads.then(|| Position {
        name,
        entry: e.front.id.clone(),
        domain: e.front.domain.clone(),
        classification: e.front.classification,
        state: block.status,
        charter: block.charter,
        envelope: block.envelope,
    })
}

/// The record among the active entries that define one name, newest first: the newest
/// verified one, active or retired as its block says; when none is verified, the
/// newest retiring one the owner wrote, since only the owner replaces a verified entry.
fn in_force(candidates: Vec<(Entry, Position)>, owner: &str) -> Option<Position> {
    let verified = candidates
        .iter()
        .find(|(e, _)| e.front.trust_tier == TrustTier::Verified);
    if let Some((_, p)) = verified {
        return Some(p.clone());
    }
    candidates
        .into_iter()
        .find(|(e, p)| p.state == State::Retired && e.front.author == owner)
        .map(|(_, p)| p)
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
    /// Its open commitments: the tasks pinned to it that are not closed, and that this
    /// session may see.
    pub commitments: Vec<Task>,
    /// Its handoff cache, newest first.
    pub handoffs: Vec<CachedHandoff>,
    /// The sessions that hold a task they claimed from it, under a live lease.
    pub sessions: Vec<String>,
    /// This session's seat, when it fills the position.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seat: Option<SeatView>,
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
            t.spec.title
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
        // the whole record of the domain, whoever asks: which version is in force is
        // one answer, and what a session may read of it is asked after
        let all = ReadFilter::for_domains([domain], Label::Restricted);
        let hits = self
            .index
            .lock()
            .await
            .list_every(&all, Some(EntryType::Position))?;
        let mut candidates = Vec::new();
        for h in hits {
            let Some(e) = self.store.get(domain, &h.id).await? else {
                continue;
            };
            if let Some(p) = position_in(&e).filter(|p| e.is_active() && p.name == name) {
                candidates.push((e, p));
            }
        }
        candidates.sort_by_key(|(e, _)| std::cmp::Reverse(e.front.created));
        Ok(in_force(candidates, &self.grants.owner))
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
        if position.envelope.handling < position.classification {
            return Err(Error::Denied(format!(
                "position {name} is classified {}, above the handling {} of its envelope: a session in it could not read its own charter",
                position.classification, position.envelope.handling
            )));
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
    /// when the session filled it, and an entry's text does not change.
    ///
    /// # Errors
    ///
    /// [`Error::Stale`] when the position was replaced, retired, demoted or erased
    /// since.
    pub(crate) async fn seat_in_force(&self) -> Result<Option<Seated>, Error> {
        let Some(seat) = lock(&self.seat).clone() else {
            return Ok(None);
        };
        let p = &seat.position;
        let now = self.position_record(&p.domain, &p.name).await?;
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
    pub(crate) fn narrow_to_envelope(&self, capabilities: &mut Vec<String>) {
        if let Some(seat) = lock(&self.seat).as_ref() {
            let tools = &seat.position.envelope.tools;
            capabilities.retain(|c| !c.starts_with("tool:") || tools.contains(c));
        }
    }

    /// Whether the position this session fills lets it call `tool`. A session that
    /// fills none is held to its grants and its task definition.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] when the envelope does not hold the tool.
    pub(crate) fn seat_allows(&self, tool: &str) -> Result<(), Error> {
        let named = format!("tool:{tool}");
        match lock(&self.seat).as_ref() {
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
            self.seat_in_force().await?;
        } else if kind == EntryType::Handoff {
            match self.seat_in_force().await {
                Ok(_) => cites.push(name.to_string()),
                Err(Error::Stale(_)) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(cites)
    }

    /// The open commitments of a position that this session may see, read: the tasks
    /// pinned to it that are not closed, in queue order.
    async fn commitments(&self, domain: &str, name: &str) -> Result<Vec<Task>, Error> {
        let pinned = self.ledger.lock().await.pinned_to(domain, name).await?;
        let handling = self.handling()?;
        let seen: Vec<Task> = pinned
            .into_iter()
            .filter(|t| self.sees(t, handling))
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
        let Some(seat) = self.seat_in_force().await? else {
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
        let sessions = self
            .ledger
            .lock()
            .await
            .seated(&position.domain, name)
            .await?;
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Promotion, WriteRequest};
    use dm_ledger::{PinReason, Status};
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
        assert_eq!(seated.capabilities(), ["human", "tool:fetch", "tool:x"]);
        seated.fill_position(RELEASE, None).await.unwrap();
        assert_eq!(seated.capabilities(), ["human", "tool:fetch"]);
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
        assert!(seated.capabilities().is_empty());
        assert!(facts(&seated).await.is_empty());
        seated.vacate();
        assert_eq!(seated.seat(), None);
        assert_eq!(seated.capabilities(), ["tool:fetch", "tool:x"]);
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
        let view = later.position(RELEASE, None).await.unwrap();
        assert_eq!(
            (view.position.state, view.position.entry),
            (State::Retired, last.clone())
        );
        // and it stays retired once a person verified that version
        verify(&owner, ME, &last).await;
        assert_eq!(denied(later.fill_position(RELEASE, None).await), fill);
    }
}
