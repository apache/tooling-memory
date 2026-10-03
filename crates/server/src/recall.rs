//! Delegated recall: a question in, a cited brief out, or the pack the brief would be
//! written from. The pack is built with no model: a batch of queries against the index,
//! the top hits with their sections and what they link to, inside a token budget. It is
//! a message, so the same object can come from a local index or over a wire. Synthesis
//! is the one part that needs a model, and who runs it is the session's tier: a
//! [`Deputy`] the launcher handed the server, or the caller in its own turn.
//!
//! Push against pull is observed rather than asserted. Two gets from one session inside
//! [`FAN_OUT_WINDOW`] are a fan-out; a session that has shown one gets summaries by
//! default and opens what it wants in one turn, while a serial caller, which pays a
//! turn per section, gets sections pushed. The bodies a pack fetched stay in a
//! per-session cache that `get` serves from once the index has admitted the id, so what
//! was pushed and later asked for shows up in the read log.

use crate::{lock, Error, Memory};
use dm_core::{approx_tokens, Entry, EntryType, Label, ReadFilter, TrustTier};
use dm_index::Hit;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

/// Two gets closer together than this came from one turn: a fan-out.
pub const FAN_OUT_WINDOW: Duration = Duration::from_millis(500);
/// The deputy's second hop is its last.
pub const MAX_HOPS: usize = 2;
/// Queries the deputy may name as missing; the rest of a longer list is ignored.
pub const MAX_MISSING: usize = 3;
/// Bodies the session cache holds; the oldest leaves when a newer one arrives.
pub const CACHE_MAX: usize = 256;

/// How much of each hit a pack carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Depth {
    /// Title and summary; the caller opens what it wants.
    Summaries,
    /// Bodies too, best hits first, until the budget is spent.
    #[default]
    Sections,
}

impl std::str::FromStr for Depth {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "summaries" => Ok(Depth::Summaries),
            "sections" => Ok(Depth::Sections),
            other => Err(format!("depth {other:?} is not summaries or sections")),
        }
    }
}

impl std::fmt::Display for Depth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Depth::Summaries => "summaries",
            Depth::Sections => "sections",
        })
    }
}

/// What a pack may cost.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Budget {
    /// Hits in the pack, across its queries.
    pub hits: usize,
    /// Tokens of bodies, approximately, over the whole recall.
    pub tokens: usize,
    /// Entries the hits cite that come along at summary depth. Zero for none.
    pub links: usize,
    /// Wall clock for the whole recall, deputy calls included.
    pub wall: Duration,
}

impl Default for Budget {
    fn default() -> Self {
        Budget {
            hits: 8,
            tokens: 6_000,
            links: 4,
            wall: Duration::from_secs(90),
        }
    }
}

impl Budget {
    /// What is left after a pack: the body tokens it already holds come off, so a second
    /// hop spends the remainder and not a fresh budget. Hits are a page size per round,
    /// not a total: a first page that came back full is the case a second hop is for.
    #[must_use]
    pub fn remaining(&self, pack: &Pack) -> Budget {
        Budget {
            tokens: self.tokens.saturating_sub(pack.body_tokens()),
            ..*self
        }
    }
}

/// Why a piece is in the pack.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Via {
    /// A query found it.
    Query,
    /// A piece the queries found cites it.
    Linked,
}

/// One entry in a pack: a hit, and its body when the depth and the budget allow.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Piece {
    /// The entry's id.
    pub id: String,
    /// Its domain.
    pub domain: String,
    /// Its kind.
    #[serde(rename = "type")]
    pub entry_type: EntryType,
    /// Its trust tier.
    pub trust_tier: TrustTier,
    /// Its classification.
    pub classification: Label,
    /// Its title.
    pub title: String,
    /// Its one-line summary.
    pub summary: String,
    /// The body, at section depth and within budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Entries this one cites, by id. Known once the body has been read.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<String>,
    /// Why it is here.
    pub via: Via,
}

impl Piece {
    fn from_hit(h: &Hit, via: Via) -> Piece {
        Piece {
            id: h.id.clone(),
            domain: h.domain.clone(),
            entry_type: h.entry_type,
            trust_tier: h.trust_tier,
            classification: h.classification,
            title: h.title.clone(),
            summary: h.summary.clone(),
            body: None,
            links: Vec::new(),
            via,
        }
    }

    /// What reading this piece costs, approximately.
    #[must_use]
    pub fn tokens(&self) -> usize {
        approx_tokens(&self.title)
            + approx_tokens(&self.summary)
            + 12
            + self.body.as_deref().map_or(0, approx_tokens)
    }
}

/// What a recall's reads yield, built with no model.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Pack {
    /// The queries that were run, in order.
    pub queries: Vec<String>,
    /// The depth it was built at.
    pub depth: Depth,
    /// Best first; linked pieces after the hits.
    pub pieces: Vec<Piece>,
    /// What reading the whole pack costs, approximately.
    pub tokens: usize,
    /// Whether the body budget ran out before every hit had its section.
    pub truncated: bool,
}

impl Pack {
    fn empty(queries: &[String], depth: Depth) -> Pack {
        Pack {
            queries: queries.to_vec(),
            depth,
            pieces: Vec::new(),
            tokens: 0,
            truncated: false,
        }
    }

    /// The ids in the pack, in order.
    #[must_use]
    pub fn ids(&self) -> Vec<String> {
        self.pieces.iter().map(|p| p.id.clone()).collect()
    }

    fn recount(&mut self) {
        self.tokens = self.pieces.iter().map(Piece::tokens).sum();
    }

    /// Tokens of the bodies it carries, approximately: what the body budget has spent.
    #[must_use]
    pub fn body_tokens(&self) -> usize {
        self.pieces
            .iter()
            .filter_map(|p| p.body.as_deref())
            .map(approx_tokens)
            .sum()
    }

    /// Add another pack's pieces that this one lacks.
    pub fn extend(&mut self, other: Pack) {
        let have: HashSet<String> = self.ids().into_iter().collect();
        self.queries.extend(other.queries);
        self.pieces
            .extend(other.pieces.into_iter().filter(|p| !have.contains(&p.id)));
        self.truncated |= other.truncated;
        self.recount();
    }

    /// The same pack at summary depth: what a caller gets beside a brief.
    #[must_use]
    pub fn summaries(mut self) -> Pack {
        for p in &mut self.pieces {
            p.body = None;
        }
        self.depth = Depth::Summaries;
        self.recount();
        self
    }
}

/// Who synthesized the brief.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "by", rename_all = "lowercase")]
pub enum Synthesis {
    /// The server's deputy, on this model.
    Server {
        /// The model that wrote the brief.
        model: String,
    },
    /// Nobody yet: the caller writes it from the pack, in its own turn.
    Owed,
}

/// What `recall` returns.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Recall {
    /// The question asked.
    pub question: String,
    /// Who wrote the brief, if anyone.
    pub synthesis: Synthesis,
    /// The cited brief, when the server wrote one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brief: Option<String>,
    /// Entry ids the brief rests on; every id in the pack when synthesis is owed.
    pub citations: Vec<String>,
    /// The pack: at the depth asked for when synthesis is owed, summaries beside a brief.
    pub pack: Pack,
    /// Search rounds spent.
    pub hops: usize,
}

/// A small model the service may spend on a session's behalf. The launcher provides
/// one when the session's route gives the server a credential of its own; a
/// subscription login's credential is the harness's alone, and the server has none.
pub trait Deputy: Send + Sync {
    /// The model, for provenance.
    fn model(&self) -> String;
    /// One completion: a prompt in, the model's text out.
    fn complete<'a>(
        &'a self,
        prompt: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>>;
}

/// Whether this session has fanned out: issued two gets inside one turn.
#[derive(Debug, Default)]
pub(crate) struct FanOut {
    last_get: Option<Instant>,
    observed: bool,
}

/// What packs pushed to one session: each entry's id and the digest of the version
/// pushed, newest [`CACHE_MAX`] kept. A note for the funnel, not a serving path: the
/// record serves every `get`, and a get whose body is the version a pack pushed is
/// counted as pushed and then opened.
#[derive(Debug, Default)]
pub(crate) struct Cache {
    map: std::collections::HashMap<String, String>,
    order: VecDeque<String>,
}

impl Cache {
    /// The digest pushed for `id`, if any.
    pub(crate) fn get(&self, id: &str) -> Option<&str> {
        self.map.get(id).map(String::as_str)
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.map.len()
    }

    /// Note a push; an id pushed again moves to the newest end.
    pub(crate) fn insert(&mut self, id: &str, digest: String) {
        if self.map.insert(id.to_string(), digest).is_some() {
            self.order.retain(|o| o != id);
        }
        self.order.push_back(id.to_string());
        while self.map.len() > CACHE_MAX {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            }
        }
    }

    pub(crate) fn remove(&mut self, id: &str) {
        if self.map.remove(id).is_some() {
            self.order.retain(|o| o != id);
        }
    }

    pub(crate) fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
    }
}

impl FanOut {
    fn note(&mut self, now: Instant) {
        if let Some(last) = self.last_get {
            if now.duration_since(last) < FAN_OUT_WINDOW {
                self.observed = true;
            }
        }
        self.last_get = Some(now);
    }
}

/// What the deputy answers with.
#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
pub(crate) struct Answer {
    #[serde(default)]
    pub brief: String,
    #[serde(default)]
    pub citations: Vec<String>,
    #[serde(default)]
    pub missing: Vec<String>,
}

const INSTRUCTIONS: &str = "You are the recall deputy of a shared memory record used by agents \
and people. Answer the question from the entries below and from nothing else. Write a brief in \
Markdown, a paragraph or a short list, and cite the entry an assertion rests on by its id in \
square brackets after the sentence, like [mem-a-1234]. Where the entries do not answer part of \
the question, say so in one sentence and name up to three short search queries that might, \
under \"missing\"; leave it empty when nothing is missing or another search would not help. \
Answer with JSON only, no prose around it: {\"brief\": Markdown, \"citations\": [ids you cited], \
\"missing\": [queries]}.";

/// The deputy's prompt over a pack.
#[must_use]
pub fn prompt_for(question: &str, pack: &Pack) -> String {
    let mut s = format!("{INSTRUCTIONS}\n\n## Question\n\n{question}\n\n## Entries\n");
    for p in &pack.pieces {
        s.push_str(&format!(
            "\n### {} ({}, {}, {}) {}\n\n{}\n",
            p.id, p.entry_type, p.trust_tier, p.classification, p.title, p.summary
        ));
        if let Some(b) = &p.body {
            s.push('\n');
            s.push_str(b.trim());
            s.push('\n');
        }
    }
    s
}

/// The deputy's answer out of its text, tolerant of a code fence and of prose around
/// the JSON. Text that is not JSON at all is taken as the brief itself.
#[must_use]
pub(crate) fn parse_answer(text: &str) -> Answer {
    let t = text.trim();
    let t = t
        .strip_prefix("```json")
        .or_else(|| t.strip_prefix("```"))
        .unwrap_or(t);
    let t = t.strip_suffix("```").unwrap_or(t).trim();
    let start = t.find('{').unwrap_or(0);
    let end = t.rfind('}').map_or(t.len(), |i| i + 1);
    let slice = t.get(start..end).unwrap_or(t);
    let mut a = serde_json::from_str::<Answer>(slice).unwrap_or_default();
    if a.brief.trim().is_empty() {
        a.brief = t.to_string();
    }
    a.missing.retain(|q| !q.trim().is_empty());
    a.missing.truncate(MAX_MISSING);
    a
}

/// Entry ids mentioned in square brackets, in order of first mention.
fn cited_in(brief: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in brief.split('[').skip(1) {
        let Some(inner) = raw.split(']').next() else {
            continue;
        };
        for id in inner.split(',').map(str::trim) {
            if dm_core::id::is_entry_id(id) && !out.iter().any(|o| o == id) {
                out.push(id.to_string());
            }
        }
    }
    out
}

fn mem_ids(sources: &[String]) -> Vec<String> {
    sources
        .iter()
        .filter(|s| dm_core::id::is_entry_id(s))
        .cloned()
        .collect()
}

/// Round-robin over the per-query hit lists, each id once, best first within a query.
fn interleave(lists: Vec<Vec<Hit>>, limit: usize) -> Vec<Hit> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let longest = lists.iter().map(Vec::len).max().unwrap_or(0);
    for i in 0..longest {
        for l in &lists {
            if let Some(h) = l.get(i) {
                if out.len() < limit && seen.insert(h.id.clone()) {
                    out.push(h.clone());
                }
            }
        }
    }
    out
}

impl Memory {
    /// Give this session a deputy. The server keeps a weak handle, so the deputy lives
    /// as long as whoever built it, the launcher, keeps it.
    pub fn set_deputy(&self, deputy: &Arc<dyn Deputy>) {
        let _ = self.deputy.set(Arc::downgrade(deputy));
    }

    /// The deputy, if the session has one.
    #[must_use]
    pub fn deputy(&self) -> Option<Arc<dyn Deputy>> {
        self.deputy.get().and_then(Weak::upgrade)
    }

    /// Whether this session has fanned out: two gets inside one turn.
    #[must_use]
    pub fn fans_out(&self) -> bool {
        lock(&self.fan_out).observed
    }

    /// The depth a recall gets when the caller names none: sections for a serial
    /// caller, summaries once the session has shown it fans out.
    #[must_use]
    pub fn default_depth(&self) -> Depth {
        if self.fans_out() {
            Depth::Summaries
        } else {
            Depth::Sections
        }
    }

    /// A get happened now, for the fan-out rule.
    pub(crate) fn note_get(&self) {
        lock(&self.fan_out).note(Instant::now());
    }

    /// The harness is about to start: forget what the fan-out rule has seen and what
    /// packs have pushed, so the launcher's own reads while building a prompt count for
    /// neither. Task mode calls this per task.
    pub fn harness_starts(&self) {
        *lock(&self.fan_out) = FanOut::default();
        lock(&self.cache).clear();
    }

    /// The digest a pack pushed for `id` to this session, if any.
    #[cfg(test)]
    pub(crate) fn cached(&self, id: &str) -> Option<String> {
        lock(&self.cache).get(id).map(str::to_string)
    }

    /// Whether `served` is the version a pack pushed for its id; a push the record has
    /// moved past is forgotten.
    pub(crate) fn was_pushed(&self, served: &Entry) -> bool {
        let mut cache = lock(&self.cache);
        let id = served.front.id.as_str();
        match cache.get(id) {
            Some(d) if d == dm_core::digest(&served.render()) => true,
            Some(_) => {
                cache.remove(id);
                false
            }
            None => false,
        }
    }

    /// An id the index refused: whatever was pushed for it no longer counts.
    pub(crate) fn forget_pushed(&self, id: &str) {
        lock(&self.cache).remove(id);
    }

    /// The pack for a batch of queries: hits interleaved across queries, each id once,
    /// bodies at section depth while they fit the budget, then up to `budget.links`
    /// entries those bodies cite at summary depth. Every read is under the session's
    /// filter and raises its label; fetched bodies go to the session cache. Not logged:
    /// the caller logs the operation it served.
    ///
    /// # Errors
    ///
    /// An ungranted domain, or an index or store error.
    pub async fn pack(
        &self,
        queries: &[String],
        domain: Option<&str>,
        depth: Depth,
        budget: Budget,
    ) -> Result<Pack, Error> {
        let f = self.filter_for(domain)?;
        let hits = {
            let ix = self.index.lock().await;
            let mut lists = Vec::with_capacity(queries.len());
            for q in queries {
                lists.push(ix.search(q, &f, budget.hits.max(1))?);
            }
            interleave(lists, budget.hits.max(1))
        };
        self.observe(&hits);
        let mut pack = Pack::empty(queries, depth);
        pack.pieces = hits
            .iter()
            .map(|h| Piece::from_hit(h, Via::Query))
            .collect();
        if depth == Depth::Sections {
            self.fill_sections(&mut pack, budget.tokens).await?;
            self.link(&mut pack, &f, budget.links).await?;
        }
        pack.recount();
        Ok(pack)
    }

    /// Bodies, best hit first, each only if it fits in what is left of the budget; a
    /// body that does not fit is skipped, not clipped, and the pack says so. The body is
    /// the record's current version, so its classification is what the session has now
    /// read, whatever the index row said; a version above the session's handling is
    /// refused. A hit the record has no current version for leaves the pack: the index
    /// is ahead of the record, and `get` would refuse it too.
    async fn fill_sections(&self, pack: &mut Pack, tokens: usize) -> Result<(), Error> {
        let mut spent = 0;
        let mut gone = Vec::new();
        for p in &mut pack.pieces {
            let Some(entry) = self.store.get(&p.domain, &p.id).await? else {
                gone.push(p.id.clone());
                continue;
            };
            if !lock(&self.session).observe(entry.front.classification) {
                gone.push(p.id.clone());
                continue;
            }
            let cost = approx_tokens(&entry.body);
            if spent + cost > tokens {
                pack.truncated = true;
                continue;
            }
            spent += cost;
            p.links = mem_ids(&entry.front.source);
            p.body = Some(entry.body.clone());
            lock(&self.cache).insert(&p.id, dm_core::digest(&entry.render()));
        }
        pack.pieces.retain(|p| !gone.contains(&p.id));
        Ok(())
    }

    async fn link(&self, pack: &mut Pack, f: &ReadFilter, max: usize) -> Result<(), Error> {
        if max == 0 {
            return Ok(());
        }
        let have: HashSet<String> = pack.ids().into_iter().collect();
        let wanted: Vec<String> = pack
            .pieces
            .iter()
            .flat_map(|p| p.links.iter().cloned())
            .filter(|id| !have.contains(id))
            .collect::<Vec<_>>();
        let mut added = HashSet::new();
        let ix = self.index.lock().await;
        for id in wanted {
            if added.len() >= max || !added.insert(id.clone()) {
                continue;
            }
            if let Some(meta) = ix.meta(&id, f)? {
                lock(&self.session).observe(meta.hit.classification);
                pack.pieces.push(Piece::from_hit(&meta.hit, Via::Linked));
            }
        }
        Ok(())
    }

    /// Search with sections: the pack for one query, logged as a search. What a caller
    /// that would open the top hits anyway asks for, to spend one turn instead of several.
    ///
    /// # Errors
    ///
    /// An ungranted domain, or an index or store error.
    pub async fn search_sections(
        &self,
        query: &str,
        domain: Option<&str>,
        limit: usize,
    ) -> Result<Pack, Error> {
        // a search costs what it hit and nothing more; linking is recall's
        let budget = Budget {
            hits: limit,
            links: 0,
            ..Budget::default()
        };
        let pack = self
            .pack(&[query.to_string()], domain, Depth::Sections, budget)
            .await?;
        self.reads
            .served("search", Some(query), pack.pieces.len(), pack.tokens, false);
        Ok(pack)
    }

    /// Delegated recall. With a deputy: the pack at section depth, one synthesis call,
    /// a second search round only when the deputy names what is missing, spending the
    /// body tokens the first round left, and the brief back with the pack at summary
    /// depth beside it; a second round that fails leaves the first brief standing. The
    /// caller's label has risen to everything the deputy read. The budget's wall clock
    /// bounds the whole of it. Without a deputy:
    /// the pack at the depth asked for, or the session's default, with synthesis owed
    /// to the caller.
    ///
    /// # Errors
    ///
    /// An ungranted domain, an index or store error, or a deputy that fails or runs out
    /// of wall clock.
    pub async fn recall(
        &self,
        question: &str,
        domain: Option<&str>,
        depth: Option<Depth>,
        budget: Budget,
    ) -> Result<Recall, Error> {
        let Some(deputy) = self.deputy() else {
            return self.recall_owed(question, domain, depth, budget).await;
        };
        let deadline = Instant::now() + budget.wall;
        let mut pack = self
            .pack(&[question.to_string()], domain, Depth::Sections, budget)
            .await?;
        let mut hops = 1;
        let mut answer = self.ask(deputy.as_ref(), question, &pack, deadline).await?;
        while !answer.missing.is_empty() && hops < MAX_HOPS {
            let more = self
                .pack(
                    &answer.missing,
                    domain,
                    Depth::Sections,
                    budget.remaining(&pack),
                )
                .await?;
            hops += 1;
            if more.pieces.iter().all(|p| pack.ids().contains(&p.id)) {
                break;
            }
            pack.extend(more);
            // a second hop that fails, out of clock or refused by the egress, costs
            // the caller nothing it already had: the first brief stands
            match self.ask(deputy.as_ref(), question, &pack, deadline).await {
                Ok(a) => answer = a,
                Err(_) => break,
            }
        }
        let in_pack: HashSet<String> = pack.ids().into_iter().collect();
        let mut citations: Vec<String> = answer
            .citations
            .iter()
            .filter(|c| in_pack.contains(*c))
            .cloned()
            .collect();
        if citations.is_empty() {
            citations = cited_in(&answer.brief)
                .into_iter()
                .filter(|c| in_pack.contains(c))
                .collect();
        }
        let pack = pack.summaries();
        self.reads.served_via(
            "recall",
            Some(question),
            pack.pieces.len(),
            approx_tokens(&answer.brief) + pack.tokens,
            false,
            Some("brief"),
        );
        Ok(Recall {
            question: question.to_string(),
            synthesis: Synthesis::Server {
                model: deputy.model(),
            },
            brief: Some(answer.brief),
            citations,
            pack,
            hops,
        })
    }

    /// The caller-executed tier: the pack at the depth asked for, or the session's
    /// default, with synthesis owed.
    async fn recall_owed(
        &self,
        question: &str,
        domain: Option<&str>,
        depth: Option<Depth>,
        budget: Budget,
    ) -> Result<Recall, Error> {
        let depth = depth.unwrap_or_else(|| self.default_depth());
        let pack = self
            .pack(&[question.to_string()], domain, depth, budget)
            .await?;
        self.reads.served_via(
            "recall",
            Some(question),
            pack.pieces.len(),
            pack.tokens,
            false,
            Some(&format!("pack:{depth}")),
        );
        Ok(Recall {
            question: question.to_string(),
            synthesis: Synthesis::Owed,
            brief: None,
            citations: pack.ids(),
            pack,
            hops: 1,
        })
    }

    async fn ask(
        &self,
        deputy: &dyn Deputy,
        question: &str,
        pack: &Pack,
        deadline: Instant,
    ) -> Result<Answer, Error> {
        let prompt = prompt_for(question, pack);
        let left = deadline.saturating_duration_since(Instant::now());
        let text = tokio::time::timeout(left, deputy.complete(&prompt))
            .await
            .map_err(|_| Error::Deputy("no answer within the recall's wall clock".into()))?
            .map_err(Error::Deputy)?;
        Ok(parse_answer(&text))
    }
}

/// The JSON a binding returns for a recall. One shape for the CLI and the MCP tool.
#[must_use]
pub fn to_json(r: &Recall) -> Value {
    let next = match r.synthesis {
        Synthesis::Owed => "Write the answer from these pieces in this turn, citing entry ids in square brackets. Open an entry with `get` only when its summary warrants it.",
        Synthesis::Server { .. } => "The brief cites entries by id; open one with `get` to drill down. What the deputy read is cached for this session.",
    };
    let mut v = serde_json::to_value(r).unwrap_or(Value::Null);
    if let Some(o) = v.as_object_mut() {
        o.insert("next".into(), Value::from(next));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WriteRequest;
    use std::sync::Mutex;

    fn hit(id: &str) -> Hit {
        Hit {
            id: id.into(),
            domain: "dom-me".into(),
            entry_type: EntryType::Fact,
            trust_tier: TrustTier::Raw,
            classification: Label::Public,
            title: id.into(),
            summary: String::new(),
            score: 0.0,
        }
    }

    #[test]
    fn interleave_takes_turns_and_keeps_each_id_once() {
        let a = vec![hit("a1"), hit("a2"), hit("a3")];
        let b = vec![hit("b1"), hit("a1"), hit("b3")];
        let ids: Vec<String> = interleave(vec![a, b], 10)
            .into_iter()
            .map(|h| h.id)
            .collect();
        assert_eq!(ids, ["a1", "b1", "a2", "a3", "b3"]);
        assert_eq!(interleave(vec![vec![hit("x"), hit("y")]], 1).len(), 1);
    }

    #[test]
    fn answers_parse_through_fences_and_fall_back_to_the_text() {
        let a = parse_answer(
            "```json\n{\"brief\": \"It retries [mem-me-00000001].\", \"citations\": [\"mem-me-00000001\"], \"missing\": [\"backoff\"]}\n```",
        );
        assert_eq!(a.citations, ["mem-me-00000001"]);
        assert_eq!(a.missing, ["backoff"]);
        let b = parse_answer("Sure! {\"brief\": \"x\"} hope that helps");
        assert_eq!(b.brief, "x");
        let c = parse_answer("Plain prose, no JSON at all.");
        assert_eq!(c.brief, "Plain prose, no JSON at all.");
        assert!(c.citations.is_empty());
        assert_eq!(
            cited_in("A [mem-me-00000001] and [mem-me-00000002, mem-me-00000001] but not [note]"),
            ["mem-me-00000001", "mem-me-00000002"]
        );
        assert_eq!(Depth::default(), Depth::Sections);
        assert_eq!("summaries".parse::<Depth>().unwrap(), Depth::Summaries);
        assert!("deep".parse::<Depth>().is_err());
    }

    #[test]
    fn a_fan_out_is_two_gets_inside_the_window() {
        let mut f = FanOut::default();
        let t = Instant::now();
        f.note(t);
        assert!(!f.observed);
        f.note(t + FAN_OUT_WINDOW * 4);
        assert!(!f.observed, "a turn apart is serial");
        f.note(t + FAN_OUT_WINDOW * 4 + FAN_OUT_WINDOW / 5);
        assert!(f.observed, "inside the window is a fan-out");
    }

    fn mem(dir: &std::path::Path) -> Memory {
        Memory::init(dir, "human:test:me", "dom-me").unwrap()
    }

    async fn write(m: &Memory, title: &str, body: &str, source: Vec<String>) -> String {
        m.write(WriteRequest {
            title: Some(title.into()),
            body: body.into(),
            source,
            ..WriteRequest::default()
        })
        .await
        .unwrap()
        .id
    }

    #[tokio::test]
    async fn a_pack_carries_sections_within_budget_and_links_what_they_cite() {
        let dir = tempfile::tempdir().unwrap();
        let m = mem(dir.path());
        let cited = write(
            &m,
            "Backoff table",
            "The backoff table lists delays.\n",
            vec![],
        )
        .await;
        let main = write(
            &m,
            "Retry policy",
            "Operation R retries with exponential backoff, per the table.\n",
            vec![cited.clone(), "https://example.invalid/rfc".into()],
        )
        .await;
        let big_body = "Retry retry retry. ".repeat(300);
        let big = write(&m, "Retry log", &big_body, vec![]).await;

        let pack = m
            .pack(
                &["retry".into()],
                None,
                Depth::Sections,
                Budget {
                    tokens: 200,
                    ..Budget::default()
                },
            )
            .await
            .unwrap();
        let ids = pack.ids();
        assert!(ids.contains(&main), "{ids:?}");
        assert!(ids.contains(&big), "{ids:?}");
        let main_piece = pack.pieces.iter().find(|p| p.id == main).unwrap();
        assert!(main_piece
            .body
            .as_deref()
            .is_some_and(|b| b.contains("exponential")));
        assert_eq!(
            main_piece.links,
            std::slice::from_ref(&cited),
            "only entry ids are links"
        );
        let linked = pack.pieces.iter().find(|p| p.id == cited).unwrap();
        assert_eq!(linked.via, Via::Linked);
        assert!(linked.body.is_none(), "linked pieces come at summary depth");
        let big_piece = pack.pieces.iter().find(|p| p.id == big).unwrap();
        assert!(
            big_piece.body.is_none(),
            "over budget, the section is left out"
        );
        assert!(pack.truncated);
        assert_eq!(
            pack.tokens,
            pack.pieces.iter().map(Piece::tokens).sum::<usize>()
        );
        assert_eq!(
            m.label(),
            Label::Internal,
            "the pack's reads raise the label"
        );
        assert!(m.cached(&main).is_some(), "fetched bodies are cached");
        assert!(m.cached(&big).is_none(), "an unfetched body is not");

        let flat = m
            .pack(&["retry".into()], None, Depth::Summaries, Budget::default())
            .await
            .unwrap();
        assert!(flat
            .pieces
            .iter()
            .all(|p| p.body.is_none() && p.via == Via::Query));
        assert!(!flat.truncated);
    }

    #[tokio::test]
    async fn without_a_deputy_recall_owes_synthesis_and_follows_the_fan_out_rule() {
        let dir = tempfile::tempdir().unwrap();
        let m = mem(dir.path());
        let id = write(
            &m,
            "Retry policy",
            "Operation R retries with backoff.\n",
            vec![],
        )
        .await;
        let r = m
            .recall("how does R retry", None, None, Budget::default())
            .await
            .unwrap();
        assert_eq!(r.synthesis, Synthesis::Owed);
        assert_eq!(r.pack.depth, Depth::Sections, "a fresh session is serial");
        assert_eq!(r.citations, std::slice::from_ref(&id));
        assert!(r.pack.pieces[0].body.is_some());
        let v = to_json(&r);
        assert_eq!(v["synthesis"]["by"], "owed");
        assert!(v["next"].as_str().unwrap().contains("this turn"));

        m.get(&id).await.unwrap();
        m.get(&id).await.unwrap();
        assert!(m.fans_out());
        let r = m
            .recall("how does R retry", None, None, Budget::default())
            .await
            .unwrap();
        assert_eq!(
            r.pack.depth,
            Depth::Summaries,
            "a caller that fans out pulls"
        );
        assert!(r.pack.pieces[0].body.is_none());
        let r = m
            .recall(
                "how does R retry",
                None,
                Some(Depth::Sections),
                Budget::default(),
            )
            .await
            .unwrap();
        assert_eq!(r.pack.depth, Depth::Sections, "an explicit depth wins");

        let reads = crate::reads::all(dir.path()).unwrap();
        let gets: Vec<_> = reads.iter().filter(|r| r.tool == "get").collect();
        assert_eq!(gets.len(), 2);
        assert!(
            gets[0].cached,
            "the first recall pushed the body; the get was served from the cache"
        );
        let recalls: Vec<_> = reads.iter().filter(|r| r.tool == "recall").collect();
        assert_eq!(recalls.len(), 3);
        assert_eq!(recalls[0].query.as_deref(), Some("how does R retry"));
        assert!(
            recalls[0].tokens > recalls[1].tokens,
            "sections cost more than summaries"
        );
    }

    /// A deputy that answers from a script and records what it was asked.
    struct Scripted {
        answers: Mutex<Vec<String>>,
        prompts: Mutex<Vec<String>>,
        /// Sleep this long before every answer.
        delay: Duration,
    }

    impl Scripted {
        fn new(answers: &[&str]) -> Arc<Scripted> {
            Scripted::slow(answers, Duration::ZERO)
        }

        fn slow(answers: &[&str], delay: Duration) -> Arc<Scripted> {
            Arc::new(Scripted {
                answers: Mutex::new(answers.iter().rev().map(|s| (*s).to_string()).collect()),
                prompts: Mutex::new(Vec::new()),
                delay,
            })
        }
    }

    impl Deputy for Scripted {
        fn model(&self) -> String {
            "scripted-1".into()
        }

        fn complete<'a>(
            &'a self,
            prompt: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
            Box::pin(async move {
                self.prompts.lock().unwrap().push(prompt.to_string());
                tokio::time::sleep(self.delay).await;
                self.answers
                    .lock()
                    .unwrap()
                    .pop()
                    .ok_or_else(|| "out of script".to_string())
            })
        }
    }

    #[tokio::test]
    async fn with_a_deputy_recall_synthesizes_takes_a_second_hop_and_returns_summaries() {
        let dir = tempfile::tempdir().unwrap();
        let m = mem(dir.path());
        let retry = write(
            &m,
            "Retry policy",
            "Operation R retries with backoff.\n",
            vec![],
        )
        .await;
        // out of reach of the question's words, so only the second hop finds it
        let limit = write(
            &m,
            "Attempt ceiling",
            "The operation gives up after five attempts.\n",
            vec![],
        )
        .await;
        let deputy = Scripted::new(&[
            &format!(
                "{{\"brief\": \"R retries [{retry}].\", \"citations\": [\"{retry}\"], \"missing\": [\"attempts\"]}}"
            ),
            &format!(
                "{{\"brief\": \"R retries [{retry}] and stops after five [{limit}].\", \"citations\": [\"{retry}\", \"{limit}\", \"mem-me-notinpack\"], \"missing\": []}}"
            ),
        ]);
        let strong: Arc<dyn Deputy> = deputy.clone();
        m.set_deputy(&strong);

        let r = m
            .recall(
                "how does R retry",
                None,
                Some(Depth::Summaries),
                Budget::default(),
            )
            .await
            .unwrap();
        assert_eq!(
            r.synthesis,
            Synthesis::Server {
                model: "scripted-1".into()
            }
        );
        assert_eq!(r.hops, 2);
        assert!(r.brief.as_deref().unwrap().contains("five"));
        assert_eq!(
            r.citations,
            [retry.clone(), limit.clone()],
            "cited and in the pack"
        );
        assert_eq!(
            r.pack.depth,
            Depth::Summaries,
            "the caller gets summaries beside the brief"
        );
        assert!(r.pack.pieces.iter().all(|p| p.body.is_none()));
        assert_eq!(r.pack.queries, ["how does R retry", "attempts"]);
        {
            let prompts = deputy.prompts.lock().unwrap();
            assert_eq!(prompts.len(), 2, "{prompts:?}");
            assert!(
                prompts[0].contains("retries with backoff"),
                "the deputy read sections"
            );
            assert!(
                prompts[1].contains("five attempts"),
                "the second hop added the missing entry"
            );
        }
        assert_eq!(
            m.label(),
            Label::Internal,
            "the caller's label rose to what the deputy read"
        );
        assert!(
            m.cached(&limit).is_some(),
            "what the deputy read is one cached get away"
        );
    }

    #[tokio::test]
    async fn the_deputy_lives_as_long_as_its_owner() {
        let dir = tempfile::tempdir().unwrap();
        let m = mem(dir.path());
        write(
            &m,
            "Retry policy",
            "Operation R retries with backoff.\n",
            vec![],
        )
        .await;
        let deputy = Scripted::new(&["{\"brief\": \"unused\"}"]);
        let strong: Arc<dyn Deputy> = deputy.clone();
        m.set_deputy(&strong);
        assert!(m.deputy().is_some());
        drop(strong);
        drop(deputy);
        assert!(m.deputy().is_none(), "a weak handle: the launcher owns it");
        let r = m
            .recall("how does R retry", None, None, Budget::default())
            .await
            .unwrap();
        assert_eq!(r.synthesis, Synthesis::Owed);
    }

    #[tokio::test]
    async fn the_cache_never_outlives_the_record() {
        let dir = tempfile::tempdir().unwrap();
        let m = mem(dir.path());
        let x = write(
            &m,
            "Retry policy",
            "Operation R retries with backoff.\n",
            vec![],
        )
        .await;
        m.pack(&["retry".into()], None, Depth::Sections, Budget::default())
            .await
            .unwrap();
        assert!(m.cached(&x).is_some());
        // the record moves under the cache: a new version of the same id
        let mut again = m.store.get("dom-me", &x).await.unwrap().unwrap();
        again.body = "Operation R retries with jitter now.\n".into();
        again.front.trust_tier = TrustTier::Curated;
        m.store.put(&again).await.unwrap();
        m.index.lock().await.upsert(&again).unwrap();
        let got = m.get(&x).await.unwrap();
        assert!(
            got.body.contains("jitter"),
            "get serves the record's version"
        );
        assert_eq!(got.front.trust_tier, TrustTier::Curated);
        let reads = crate::reads::all(dir.path()).unwrap();
        assert!(
            !reads.last().unwrap().cached,
            "a stale copy is not a cache hit"
        );
        // admission is the index's: a superseded entry is refused and dropped
        m.pack(&["retry".into()], None, Depth::Sections, Budget::default())
            .await
            .unwrap();
        assert!(m.cached(&x).is_some());
        m.write(WriteRequest {
            supersedes: Some(x.clone()),
            ..WriteRequest {
                body: "Operation R no longer retries.\n".into(),
                ..WriteRequest::default()
            }
        })
        .await
        .unwrap();
        assert!(matches!(m.get(&x).await, Err(Error::NotFound(_))));
        assert!(m.cached(&x).is_none(), "a refused id leaves the cache");
    }

    #[tokio::test]
    async fn the_cache_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let m = mem(dir.path());
        let mut ids = Vec::new();
        for i in 0..(CACHE_MAX + 5) {
            ids.push(
                write(
                    &m,
                    &format!("Retry note {i}"),
                    "Operation R retries.\n",
                    vec![],
                )
                .await,
            );
        }
        for i in 0..ids.len() {
            m.pack(
                &[format!("note {i}")],
                None,
                Depth::Sections,
                Budget::default(),
            )
            .await
            .unwrap();
        }
        assert!(lock(&m.cache).len() <= CACHE_MAX);
        assert!(m.cached(ids.last().unwrap()).is_some(), "newest kept");
        assert!(m.cached(&ids[0]).is_none(), "oldest dropped");
    }

    #[tokio::test]
    async fn the_body_budget_is_cumulative_and_one_recall_wide() {
        let dir = tempfile::tempdir().unwrap();
        let m = mem(dir.path());
        let body = format!("{}\n", "retry ".repeat(75)); // about 100 tokens
        let a = write(&m, "Retry alpha", &body, vec![]).await;
        let b = write(&m, "Retry bravo", &body, vec![]).await;
        let c = write(&m, "Retry charlie", &body, vec![]).await;
        let pack = m
            .pack(
                &["retry".into()],
                None,
                Depth::Sections,
                Budget {
                    tokens: 250,
                    ..Budget::default()
                },
            )
            .await
            .unwrap();
        let with_body = pack.pieces.iter().filter(|p| p.body.is_some()).count();
        assert_eq!(
            with_body, 2,
            "two fit, the third would exceed the budget {pack:?}"
        );
        assert!(pack.truncated);

        // with a deputy: the second hop spends what the first left, not a fresh budget
        let deputy = Scripted::new(&[
            &format!("{{\"brief\": \"[{a}]\", \"missing\": [\"bravo charlie\"]}}"),
            &format!("{{\"brief\": \"[{a}] [{b}] [{c}]\"}}"),
        ]);
        let strong: Arc<dyn Deputy> = deputy.clone();
        m.set_deputy(&strong);
        let r = m
            .recall(
                "alpha",
                None,
                None,
                Budget {
                    tokens: 120,
                    ..Budget::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(r.hops, 2);
        let prompts = deputy.prompts.lock().unwrap();
        // a summary is clipped to 200 characters; only a body holds forty repeats
        let run = "retry ".repeat(40);
        let bodies_in = |p: &str| p.matches(run.as_str()).count();
        assert!(
            bodies_in(&prompts[0]) > 0,
            "the first hop carried alpha's body"
        );
        assert_eq!(
            bodies_in(&prompts[1]),
            bodies_in(&prompts[0]),
            "the second hop had no body budget left, so bravo and charlie came as summaries"
        );
        assert!(prompts[1].contains("Retry bravo"));
    }

    #[test]
    fn a_deputy_s_missing_list_is_bounded_and_an_empty_brief_falls_back() {
        let many: Vec<String> = (0..2000).map(|i| format!("q{i} OR *")).collect();
        let a = parse_answer(&format!(
            "{{\"brief\": \"x\", \"missing\": {}}}",
            serde_json::to_string(&many).unwrap()
        ));
        assert_eq!(a.missing.len(), MAX_MISSING);
        let b = parse_answer("{\"brief\": \"x\", \"missing\": [\"\", \"  \", \"real\"]}");
        assert_eq!(b.missing, ["real"]);
        let c = parse_answer("{\"citations\": []}");
        assert_eq!(
            c.brief, "{\"citations\": []}",
            "no brief: the text is the brief"
        );
    }

    #[tokio::test]
    async fn a_fan_out_is_counted_only_for_admitted_gets_and_can_be_reset() {
        let dir = tempfile::tempdir().unwrap();
        let m = mem(dir.path());
        let id = write(
            &m,
            "Retry policy",
            "Operation R retries with backoff.\n",
            vec![],
        )
        .await;
        assert!(m.get("mem-me-nope1").await.is_err());
        assert!(m.get("mem-me-nope2").await.is_err());
        assert!(!m.fans_out(), "two misses are not a fan-out");
        m.get(&id).await.unwrap();
        m.get(&id).await.unwrap();
        assert!(m.fans_out());
        m.harness_starts();
        assert!(!m.fans_out(), "the launcher's prompt reads do not count");
        m.get(&id).await.unwrap();
        assert!(!m.fans_out(), "one get after the reset is not a fan-out");
    }

    #[tokio::test]
    async fn linking_belongs_to_recall_not_to_search() {
        let dir = tempfile::tempdir().unwrap();
        let m = mem(dir.path());
        let internal = write(&m, "Internal table", "The internal table.\n", vec![]).await;
        m.write(WriteRequest {
            title: Some("Open policy".into()),
            body: "The open policy cites the table.\n".into(),
            source: vec![internal.clone()],
            classification: Some(Label::Public),
            ..WriteRequest::default()
        })
        .await
        .unwrap();
        drop(m);
        let m = Memory::open(dir.path()).unwrap();
        let pack = m.search_sections("open policy", None, 10).await.unwrap();
        assert!(pack.pieces.iter().all(|p| p.via == Via::Query), "{pack:?}");
        assert_eq!(m.label(), Label::Public, "a search costs only what it hit");
        let r = m
            .recall("open policy", None, None, Budget::default())
            .await
            .unwrap();
        assert!(r
            .pack
            .pieces
            .iter()
            .any(|p| p.id == internal && p.via == Via::Linked));
        assert_eq!(
            m.label(),
            Label::Internal,
            "a recall reads what the hits cite"
        );
    }

    #[tokio::test]
    async fn the_hop_cap_holds_against_a_deputy_that_always_wants_more() {
        let dir = tempfile::tempdir().unwrap();
        let m = mem(dir.path());
        write(&m, "Alpha", "Only alpha here.\n", vec![]).await;
        write(&m, "Bravo", "Only bravo here.\n", vec![]).await;
        write(&m, "Charlie", "Only charlie here.\n", vec![]).await;
        let deputy = Scripted::new(&[
            "{\"brief\": \"a\", \"missing\": [\"bravo\"]}",
            "{\"brief\": \"ab\", \"missing\": [\"charlie\"]}",
            "{\"brief\": \"abc\", \"missing\": [\"delta\"]}",
        ]);
        let strong: Arc<dyn Deputy> = deputy.clone();
        m.set_deputy(&strong);
        let r = m
            .recall("alpha", None, None, Budget::default())
            .await
            .unwrap();
        assert_eq!(r.hops, MAX_HOPS);
        assert_eq!(deputy.prompts.lock().unwrap().len(), MAX_HOPS);
        assert!(!r.pack.pieces.iter().any(|p| p.title == "Charlie"));
    }

    #[test]
    fn extend_keeps_each_id_once() {
        let mut a = Pack::empty(&["q1".into()], Depth::Sections);
        a.pieces.push(Piece::from_hit(&hit("x"), Via::Query));
        let mut b = Pack::empty(&["q2".into()], Depth::Sections);
        b.pieces.push(Piece::from_hit(&hit("x"), Via::Query));
        b.pieces.push(Piece::from_hit(&hit("y"), Via::Query));
        a.extend(b);
        assert_eq!(a.ids(), ["x", "y"]);
        assert_eq!(a.queries, ["q1", "q2"]);
    }

    /// A deputy that never answers.
    struct Hangs;

    impl Deputy for Hangs {
        fn model(&self) -> String {
            "hangs".into()
        }

        fn complete<'a>(
            &'a self,
            _prompt: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
            Box::pin(async {
                tokio::time::sleep(Duration::from_secs(5)).await;
                Ok(String::new())
            })
        }
    }

    #[tokio::test]
    async fn the_wall_clock_bounds_the_whole_recall() {
        let dir = tempfile::tempdir().unwrap();
        let m = mem(dir.path());
        write(
            &m,
            "Retry policy",
            "Operation R retries with backoff.\n",
            vec![],
        )
        .await;
        let deputy: Arc<dyn Deputy> = Arc::new(Hangs);
        m.set_deputy(&deputy);
        let started = Instant::now();
        let err = m
            .recall(
                "retry",
                None,
                None,
                Budget {
                    wall: Duration::from_millis(50),
                    ..Budget::default()
                },
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, Error::Deputy(ref s) if s.contains("within")),
            "{err}"
        );
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn a_section_read_raises_the_label_to_what_the_record_says() {
        let dir = tempfile::tempdir().unwrap();
        let m = mem(dir.path());
        let x = write(
            &m,
            "Retry policy",
            "Operation R retries with backoff.\n",
            vec![],
        )
        .await;
        // the record moves ahead of the index: a restricted version, no upsert yet
        let mut again = m.store.get("dom-me", &x).await.unwrap().unwrap();
        again.front.classification = Label::Restricted;
        m.store.put(&again).await.unwrap();
        let pack = m
            .pack(&["retry".into()], None, Depth::Sections, Budget::default())
            .await
            .unwrap();
        assert!(pack.pieces[0].body.is_some());
        assert_eq!(
            m.label(),
            Label::Restricted,
            "the body read is the record's version"
        );
        // under a cap below the record's version, the body is refused, not served
        let capped = Memory::open(dir.path())
            .unwrap()
            .with_handling_cap(Label::Internal);
        let pack = capped
            .pack(&["retry".into()], None, Depth::Sections, Budget::default())
            .await
            .unwrap();
        assert!(pack.pieces.is_empty(), "{pack:?}");
        assert_eq!(
            capped.label(),
            Label::Internal,
            "the summary the index admitted was read; the restricted body was not"
        );
    }

    #[tokio::test]
    async fn a_hit_the_record_lost_leaves_the_pack() {
        let dir = tempfile::tempdir().unwrap();
        let m = mem(dir.path());
        let x = write(
            &m,
            "Retry policy",
            "Operation R retries with backoff.\n",
            vec![],
        )
        .await;
        let y = write(&m, "Retry log", "Retry log for R.\n", vec![]).await;
        std::fs::remove_file(dir.path().join(format!("record/dom-me/{y}.md"))).unwrap();
        let pack = m
            .pack(&["retry".into()], None, Depth::Sections, Budget::default())
            .await
            .unwrap();
        assert_eq!(pack.ids(), [x]);
        assert!(!pack.truncated);
    }

    #[tokio::test]
    async fn get_serves_the_record_even_when_the_index_is_behind() {
        let dir = tempfile::tempdir().unwrap();
        let m = mem(dir.path());
        let x = write(
            &m,
            "Retry policy",
            "Operation R retries with backoff.\n",
            vec![],
        )
        .await;
        m.pack(&["retry".into()], None, Depth::Sections, Budget::default())
            .await
            .unwrap();
        let mut again = m.store.get("dom-me", &x).await.unwrap().unwrap();
        again.body = "Operation R retries with jitter now.\n".into();
        m.store.put(&again).await.unwrap(); // no index upsert: the index is behind
        let got = m.get(&x).await.unwrap();
        assert!(
            got.body.contains("jitter"),
            "the record serves, whatever the index says"
        );
        let reads = crate::reads::all(dir.path()).unwrap();
        assert!(!reads.last().unwrap().cached);
    }

    #[tokio::test]
    async fn the_second_hop_runs_when_the_first_page_was_full() {
        let dir = tempfile::tempdir().unwrap();
        let m = mem(dir.path());
        for i in 0..9 {
            write(
                &m,
                &format!("Retry note {i}"),
                "Operation R retries.\n",
                vec![],
            )
            .await;
        }
        let limit = write(
            &m,
            "Attempt ceiling",
            "The operation gives up after five attempts.\n",
            vec![],
        )
        .await;
        let deputy = Scripted::new(&[
            "{\"brief\": \"R retries.\", \"missing\": [\"attempts\"]}",
            &format!("{{\"brief\": \"R retries and stops after five [{limit}].\"}}"),
        ]);
        let strong: Arc<dyn Deputy> = deputy.clone();
        m.set_deputy(&strong);
        let r = m
            .recall("retry", None, None, Budget::default())
            .await
            .unwrap();
        assert_eq!(
            r.hops, 2,
            "a full first page does not end the recall: {r:?}"
        );
        assert!(r.pack.ids().contains(&limit));
        assert_eq!(r.citations, [limit]);
    }

    #[tokio::test]
    async fn a_two_hop_recall_shares_one_wall_clock_and_keeps_the_first_brief() {
        let dir = tempfile::tempdir().unwrap();
        let m = mem(dir.path());
        let retry = write(
            &m,
            "Retry policy",
            "Operation R retries with backoff.\n",
            vec![],
        )
        .await;
        write(
            &m,
            "Attempt ceiling",
            "The operation gives up after five attempts.\n",
            vec![],
        )
        .await;
        // each call is under the wall; the two together are not
        let deputy = Scripted::slow(
            &[
                &format!("{{\"brief\": \"first [{retry}]\", \"missing\": [\"attempts\"]}}"),
                "{\"brief\": \"second\"}",
            ],
            Duration::from_millis(200),
        );
        let strong: Arc<dyn Deputy> = deputy.clone();
        m.set_deputy(&strong);
        let r = m
            .recall(
                "retry",
                None,
                None,
                Budget {
                    wall: Duration::from_millis(300),
                    ..Budget::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(
            deputy.prompts.lock().unwrap().len(),
            2,
            "the second hop was tried"
        );
        assert_eq!(
            r.brief.as_deref(),
            Some(format!("first [{retry}]").as_str()),
            "the first brief survives the second hop running out of clock"
        );
        assert_eq!(r.citations, [retry]);
        assert_eq!(r.hops, 2);
    }

    #[tokio::test]
    async fn harness_starts_forgets_what_was_pushed() {
        let dir = tempfile::tempdir().unwrap();
        let m = mem(dir.path());
        let x = write(
            &m,
            "Retry policy",
            "Operation R retries with backoff.\n",
            vec![],
        )
        .await;
        m.pack(&["retry".into()], None, Depth::Sections, Budget::default())
            .await
            .unwrap();
        assert!(m.cached(&x).is_some());
        m.harness_starts();
        assert!(
            m.cached(&x).is_none(),
            "a push to the launcher is not a push to the harness"
        );
        m.get(&x).await.unwrap();
        assert!(
            !crate::reads::all(dir.path())
                .unwrap()
                .last()
                .unwrap()
                .cached
        );
    }

    #[tokio::test]
    async fn a_deputy_that_fails_is_an_error_not_a_pack() {
        let dir = tempfile::tempdir().unwrap();
        let m = mem(dir.path());
        write(
            &m,
            "Retry policy",
            "Operation R retries with backoff.\n",
            vec![],
        )
        .await;
        let deputy: Arc<dyn Deputy> = Scripted::new(&[]);
        m.set_deputy(&deputy);
        let err = m
            .recall("how does R retry", None, None, Budget::default())
            .await
            .unwrap_err();
        assert!(
            matches!(err, Error::Deputy(ref s) if s.contains("out of script")),
            "{err}"
        );
    }

    #[tokio::test]
    async fn a_brief_with_no_citation_list_is_cited_from_its_brackets() {
        let dir = tempfile::tempdir().unwrap();
        let m = mem(dir.path());
        let retry = write(
            &m,
            "Retry policy",
            "Operation R retries with backoff.\n",
            vec![],
        )
        .await;
        let deputy: Arc<dyn Deputy> = Scripted::new(&[&format!(
            "R retries with backoff [{retry}], and that is all."
        )]);
        m.set_deputy(&deputy);
        let r = m
            .recall("how does R retry", None, None, Budget::default())
            .await
            .unwrap();
        assert_eq!(r.hops, 1);
        assert_eq!(r.citations, [retry]);
        assert!(r.brief.unwrap().starts_with("R retries"));
    }
}
