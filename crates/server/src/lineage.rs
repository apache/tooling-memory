//! Lineage: what an entry may have been made from, and what may have been made from
//! it, read from the logs of the domains (the paper's lineage query, which serves
//! erasure and a poisoned entry alike).
//!
//! The log holds three things lineage reads. A `read` names the entries a session was
//! served. A `write` names the entry a session offered, what it cites and what it
//! replaces. A `send` names where something went when it left the perimeter. Two kinds
//! of edge follow. An entry is *informed by* each entry its session had read before it
//! was written: a session's context is everything it read, so every such read counts,
//! which is coarse on purpose. An entry *cites* what its `write` event names. A send is
//! informed by what its session had read before it.
//!
//! "Before" is the order of one log where there is one: the sequence numbers of a
//! domain's log are its order of record. Across two domains there is no common order,
//! so the times of the two events decide, compared as times; a time that does not parse
//! decides nothing, and the read then counts.
//!
//! A write the screen refused is no entry, so nothing follows forward into it; back from
//! it, what its session had read is what the poison may have come from. A write that
//! failed before the record took it is no entry either: the log says so with a
//! `quarantined` event whose gate is `error`. An entry with no
//! `write` event, one from before the log or one an import brought, has edges the log
//! cannot give: the answer says it is not complete.

use crate::log::{quarantined_writes, Change, Event};
use crate::{Error, Memory};
use dm_core::erasure::Sent;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// Which way a lineage query follows the edges.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// What it may have been made from.
    Back,
    /// What may have been made from it, and where that went.
    Forward,
}

/// One entry a lineage query reached.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edge {
    /// The entry.
    pub id: String,
    /// How many steps from the start, by the shortest way.
    pub hops: u32,
    /// Whether a session read one and then wrote the other.
    pub informed: bool,
    /// Whether one names the other, as a source or as the entry it replaces.
    pub cited: bool,
}

/// What a lineage query found.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lineage {
    /// What was asked about: an entry, or `<domain>@<seq>` for a send event.
    pub of: String,
    /// Which way.
    pub direction: Direction,
    /// The entries reached, nearest first.
    pub entries: Vec<Edge>,
    /// Forward: the sends of the sessions on the way, after what they read.
    pub sends: Vec<Sent>,
    /// Forward: the sessions that read `of`. Back: the session that wrote or sent it.
    pub sessions: Vec<String>,
    /// Whether the log could say. False when an entry on the way has no `write` event,
    /// so what it was made from is not known, or when `of` names no send event.
    pub complete: bool,
}

/// Where an event is: its domain's log, its place there, and its time.
#[derive(Clone, Copy)]
struct At<'a> {
    domain: &'a str,
    seq: u64,
    time: Option<OffsetDateTime>,
}

impl At<'_> {
    /// Whether this may have come before `later`: by sequence number in one log, by
    /// time across two, and a time that does not parse does not rule it out.
    fn before(&self, later: &At<'_>) -> bool {
        if self.domain == later.domain {
            return self.seq < later.seq;
        }
        match (self.time, later.time) {
            (Some(a), Some(b)) => a <= b,
            _ => true,
        }
    }
}

/// A `write` event.
struct Wrote<'a> {
    id: &'a str,
    session: Option<&'a str>,
    at: At<'a>,
    /// The entries it cites and the one it replaces.
    names: Vec<&'a str>,
    /// Whether the record took it: no `quarantined` event names it, or the owner
    /// released it after one did.
    kept: bool,
}

/// A `send` event.
#[derive(Clone, Copy)]
struct SentAt<'a> {
    session: &'a str,
    at: At<'a>,
    to: &'a str,
}

/// The events lineage reads, out of every log.
#[derive(Default)]
struct Logs<'a> {
    /// Each read: the session, where, and the entries it names.
    reads: Vec<(&'a str, At<'a>, &'a [String])>,
    writes: Vec<Wrote<'a>>,
    sends: Vec<SentAt<'a>>,
}

/// Where lineage steps from: an entry, or a send event.
#[derive(Clone, Copy)]
enum Node<'a> {
    Entry(&'a str),
    Send(&'a str, u64),
}

/// One step from a node.
#[derive(Default)]
struct Step<'a> {
    /// Each entry reached, and whether it was reached by a read (else by a citation).
    edges: Vec<(&'a str, bool)>,
    sends: Vec<SentAt<'a>>,
    sessions: BTreeSet<&'a str>,
    /// Whether the log held what the step needed.
    known: bool,
}

impl<'a> Logs<'a> {
    fn new(logs: &'a BTreeMap<String, Vec<Event>>) -> Logs<'a> {
        let mut out = Logs::default();
        for (domain, events) in logs {
            let refused = quarantined_writes(events);
            for e in events {
                let at = At {
                    domain,
                    seq: e.seq,
                    time: OffsetDateTime::parse(&e.at, &Rfc3339).ok(),
                };
                out.take(e, at, events, &refused);
            }
        }
        out
    }

    fn take(&mut self, e: &'a Event, at: At<'a>, log: &'a [Event], refused: &BTreeMap<u64, u64>) {
        let session = e.session.as_deref();
        match (&e.change, session) {
            (Change::Read { ids, .. }, Some(session)) => self.reads.push((session, at, ids)),
            (Change::Send { to, .. }, Some(session)) => {
                self.sends.push(SentAt { session, at, to });
            }
            (
                Change::Write {
                    id,
                    cites,
                    supersedes,
                    ..
                },
                _,
            ) => self.writes.push(Wrote {
                id,
                session,
                at,
                names: cites.iter().chain(supersedes).map(String::as_str).collect(),
                // not refused, or released since: a `put` from before the refusal is a
                // version the record did not take
                kept: refused
                    .get(&e.seq)
                    .is_none_or(|refusal| put_after(log, id, *refusal)),
            }),
            _ => {}
        }
    }

    /// The entries `session` had read before `at`, each as an edge by a read.
    fn read_before(&self, session: &str, at: &At<'_>) -> Vec<(&'a str, bool)> {
        self.reads
            .iter()
            .filter(|(s, read, _)| *s == session && read.before(at))
            .flat_map(|(_, _, ids)| ids.iter().map(|id| (id.as_str(), true)))
            .collect()
    }

    /// One step back: what the node's session had read before it, and what it cites.
    fn back(&self, node: Node<'a>) -> Step<'a> {
        let mut step = Step::default();
        match node {
            Node::Entry(id) => {
                for w in self.writes.iter().filter(|w| w.id == id) {
                    step.known = true;
                    step.edges.extend(w.names.iter().map(|n| (*n, false)));
                    if let Some(session) = w.session {
                        step.sessions.insert(session);
                        step.edges.extend(self.read_before(session, &w.at));
                    }
                }
            }
            Node::Send(domain, seq) => {
                let sent = self
                    .sends
                    .iter()
                    .find(|s| s.at.domain == domain && s.at.seq == seq);
                if let Some(s) = sent {
                    step.known = true;
                    step.sessions.insert(s.session);
                    step.edges.extend(self.read_before(s.session, &s.at));
                }
            }
        }
        step
    }

    /// One step forward from an entry: what each of its readers wrote and sent after
    /// the read, and what cites it. Nothing follows forward from a send.
    fn forward(&self, node: Node<'a>) -> Step<'a> {
        let mut step = Step::default();
        let Node::Entry(id) = node else {
            return step;
        };
        step.known = true;
        let read_at: Vec<(&str, At<'_>)> = self
            .reads
            .iter()
            .filter(|(_, _, ids)| ids.iter().any(|i| i == id))
            .map(|(session, at, _)| (*session, *at))
            .collect();
        let after = |session: &str, at: &At<'_>| {
            read_at
                .iter()
                .any(|(reader, read)| *reader == session && read.before(at))
        };
        step.sessions = read_at.iter().map(|(session, _)| *session).collect();
        for w in self.writes.iter().filter(|w| w.kept) {
            if w.names.contains(&id) {
                step.edges.push((w.id, false));
            }
            if w.session.is_some_and(|session| after(session, &w.at)) {
                step.edges.push((w.id, true));
            }
        }
        step.sends = self
            .sends
            .iter()
            .filter(|s| after(s.session, &s.at))
            .copied()
            .collect();
        step
    }
}

/// Whether the log holds a `put` of `id` after sequence number `seq`: a write the
/// screen refused that the owner released into the record.
fn put_after(log: &[Event], id: &str, seq: u64) -> bool {
    log.iter()
        .any(|e| e.seq > seq && matches!(&e.change, Change::Put { id: put, .. } if put == id))
}

/// What `of` names: an entry, or the send event `<domain>@<seq>`.
fn start(of: &str) -> Option<Node<'_>> {
    match of.split_once('@') {
        None => Some(Node::Entry(of)),
        Some((domain, seq)) => seq.parse().ok().map(|seq| Node::Send(domain, seq)),
    }
}

/// The send events found, as one [`Sent`] for each session, destination and log.
fn grouped(sends: &BTreeMap<(&str, u64), SentAt<'_>>) -> Vec<Sent> {
    let mut out: BTreeMap<(&str, &str, &str), Sent> = BTreeMap::new();
    // in the order of each log, so the first of a group is the first seen
    for ((domain, seq), s) in sends {
        let sent = out.entry((domain, s.to, s.session)).or_insert(Sent {
            domain: (*domain).to_string(),
            to: s.to.to_string(),
            session: s.session.to_string(),
            count: 0,
            first: *seq,
            last: *seq,
        });
        sent.count += 1;
        sent.last = *seq;
    }
    let mut out: Vec<Sent> = out.into_values().collect();
    out.sort_by(|a, b| (&a.domain, a.first).cmp(&(&b.domain, b.first)));
    out
}

/// What the steps so far reached.
#[derive(Default)]
struct Reached<'a> {
    entries: BTreeMap<&'a str, Edge>,
    /// Each send event once, by its log and its place there.
    sends: BTreeMap<(&'a str, u64), SentAt<'a>>,
    /// Whether a step lacked what it needed from the log.
    unknown: bool,
}

impl<'a> Reached<'a> {
    /// Take what one step from one node reached, as step number `hops` of the query
    /// about `of`. Gives the entries no step reached before: where the next step
    /// starts.
    fn take(&mut self, step: Step<'a>, hops: u32, of: &str) -> Vec<Node<'a>> {
        self.unknown |= !step.known;
        self.sends
            .extend(step.sends.into_iter().map(|s| ((s.at.domain, s.at.seq), s)));
        let mut new = Vec::new();
        for (id, informed) in step.edges {
            if id == of {
                continue;
            }
            let edge = self.entries.entry(id).or_insert_with(|| {
                new.push(Node::Entry(id));
                Edge {
                    id: id.to_string(),
                    hops,
                    informed: false,
                    cited: false,
                }
            });
            edge.informed |= informed;
            edge.cited |= !informed;
        }
        new
    }
}

/// The lineage of `of` over `logs`, the log of each domain by its name: each step
/// follows the edges one way, from every entry the step before reached, until `depth`
/// steps are made or a step reaches nothing new. An entry keeps the number of the
/// first step that reached it, and is informed, cited or both by the edges that
/// reached it.
#[must_use]
pub fn trace(
    logs: &BTreeMap<String, Vec<Event>>,
    of: &str,
    direction: Direction,
    depth: Option<u32>,
) -> Lineage {
    let events = Logs::new(logs);
    let mut frontier: Vec<Node<'_>> = start(of).into_iter().collect();
    let mut reached = Reached {
        unknown: frontier.is_empty(),
        ..Reached::default()
    };
    let mut sessions = BTreeSet::new();
    let mut hops = 0;
    while !frontier.is_empty() && depth.is_none_or(|d| hops < d) {
        hops += 1;
        let mut next = Vec::new();
        for node in frontier {
            let mut step = match direction {
                Direction::Back => events.back(node),
                Direction::Forward => events.forward(node),
            };
            if hops == 1 {
                sessions = std::mem::take(&mut step.sessions);
            }
            next.extend(reached.take(step, hops, of));
        }
        frontier = next;
    }
    let mut entries: Vec<Edge> = reached.entries.into_values().collect();
    entries.sort_by(|a, b| (a.hops, &a.id).cmp(&(b.hops, &b.id)));
    Lineage {
        of: of.to_string(),
        direction,
        entries,
        sends: grouped(&reached.sends),
        sessions: sessions.into_iter().map(str::to_string).collect(),
        complete: !reached.unknown,
    }
}

impl Memory {
    /// The lineage of an entry, or back from a send event named `<domain>@<seq>`, over
    /// the log of every granted domain. For the security role, and read as the log is
    /// read: an event above what this session may handle is left out, so the answer
    /// holds only what the session may be told, and the session's label rises as it
    /// does for the log. The ledger's events are copied into each log first, as before
    /// every serving of a log. `depth` bounds the steps; without one the query runs
    /// until it reaches nothing new.
    ///
    /// A session that does not read the held-out domain gets no event of that domain's
    /// log, as it gets no entry of the domain.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for an author outside the security role, or for a send asked
    /// about forward: what a send reached is outside the record. A ledger error, or the
    /// file system.
    pub async fn lineage(
        &self,
        of: &str,
        direction: Direction,
        depth: Option<u32>,
    ) -> Result<Lineage, Error> {
        if !self.grants().is_security(self.author()) {
            return Err(Error::Denied(
                "lineage is for the owner and the security role".into(),
            ));
        }
        if direction == Direction::Forward && of.contains('@') {
            return Err(Error::Denied(
                "a send has no lineage forward: what it reached is outside the record".into(),
            ));
        }
        let mut logs = BTreeMap::new();
        // the log of the held-out domain is no read of a session that does not read
        // that domain: the answer leaves its events out
        let read = self.domains().into_iter();
        for domain in read.filter(|d| !self.keeps_from_session(d)) {
            self.ledger_into_log(&domain).await?;
            let events = self.log(Some(&domain), 0)?;
            logs.insert(domain, events);
        }
        Ok(trace(&logs, of, direction, depth))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WriteRequest;
    use dm_core::Label;

    const NOON: &str = "2026-11-01T12:00:00Z";

    /// An event of `session` at `seq`, at noon.
    fn ev(seq: u64, session: &str, change: Change) -> Event {
        Event {
            seq,
            at: NOON.into(),
            by: "agent:test".into(),
            session: Some(session.into()),
            label: Some(Label::Internal),
            quarantined: matches!(change, Change::Quarantined { .. }),
            outside: false,
            change,
        }
    }

    /// The same event at another time.
    fn at(time: &str, mut event: Event) -> Event {
        event.at = time.into();
        event
    }

    fn read(ids: &[&str]) -> Change {
        Change::Read {
            ids: ids.iter().map(|i| (*i).to_string()).collect(),
            survey: false,
        }
    }

    fn write(id: &str) -> Change {
        citing(id, &[], None)
    }

    fn citing(id: &str, cites: &[&str], supersedes: Option<&str>) -> Change {
        Change::Write {
            id: id.into(),
            digest: "d".into(),
            cites: cites.iter().map(|c| (*c).to_string()).collect(),
            supersedes: supersedes.map(str::to_string),
        }
    }

    fn send(to: &str) -> Change {
        Change::Send {
            to: to.into(),
            digest: "d".into(),
        }
    }

    /// One log, of `dom-a`.
    fn one(events: Vec<Event>) -> BTreeMap<String, Vec<Event>> {
        BTreeMap::from([("dom-a".to_string(), events)])
    }

    /// Each entry reached: its id, its hops, whether informed, whether cited.
    fn edges(l: &Lineage) -> Vec<(&str, u32, bool, bool)> {
        l.entries
            .iter()
            .map(|e| (e.id.as_str(), e.hops, e.informed, e.cited))
            .collect()
    }

    fn back(logs: &BTreeMap<String, Vec<Event>>, of: &str) -> Lineage {
        trace(logs, of, Direction::Back, None)
    }

    fn forward(logs: &BTreeMap<String, Vec<Event>>, of: &str) -> Lineage {
        trace(logs, of, Direction::Forward, None)
    }

    #[test]
    fn back_from_an_entry_gives_what_its_session_read_below_its_offset_and_not_after() {
        let logs = one(vec![
            ev(1, "s1", read(&["mem-a", "mem-b"])),
            ev(2, "s1", write("mem-e")),
            ev(3, "s1", read(&["mem-late"])),
        ]);
        let l = back(&logs, "mem-e");
        assert_eq!(
            edges(&l),
            [("mem-a", 1, true, false), ("mem-b", 1, true, false)]
        );
        assert_eq!(l.sessions, ["s1"], "the session that wrote it");
        assert_eq!((l.of.as_str(), l.direction), ("mem-e", Direction::Back));
        assert!(l.sends.is_empty());
    }

    #[test]
    fn a_read_by_another_session_is_not_an_edge() {
        let logs = one(vec![
            ev(1, "s2", read(&["mem-theirs"])),
            ev(2, "s1", read(&["mem-x"])),
            ev(3, "s1", write("mem-e")),
            ev(4, "s2", write("mem-other")),
        ]);
        assert_eq!(edges(&back(&logs, "mem-e")), [("mem-x", 1, true, false)]);
        let l = forward(&logs, "mem-x");
        assert_eq!(edges(&l), [("mem-e", 1, true, false)]);
        assert_eq!(l.sessions, ["s1"], "the sessions that read it");
    }

    #[test]
    fn forward_from_an_entry_gives_what_a_reader_wrote_after_the_read_and_not_before() {
        let logs = one(vec![
            ev(1, "s1", write("mem-before")),
            ev(2, "s1", read(&["mem-x"])),
            ev(3, "s1", write("mem-after1")),
            ev(4, "s1", read(&["mem-x"])),
            ev(5, "s1", write("mem-after2")),
        ]);
        assert_eq!(
            edges(&forward(&logs, "mem-x")),
            [
                ("mem-after1", 1, true, false),
                ("mem-after2", 1, true, false)
            ]
        );
    }

    #[test]
    fn a_quarantined_write_is_no_forward_edge_and_back_from_it_gives_what_its_session_read() {
        let mut events = vec![
            ev(1, "s1", read(&["mem-x"])),
            ev(2, "s1", write("mem-held")),
            ev(
                3,
                "s1",
                Change::Quarantined {
                    id: "mem-held".into(),
                    of: 2,
                    gate: "injection".into(),
                    findings: vec![],
                },
            ),
        ];
        let logs = one(events.clone());
        assert!(edges(&forward(&logs, "mem-x")).is_empty());
        let l = trace(&logs, "mem-held", Direction::Back, Some(1));
        assert_eq!(edges(&l), [("mem-x", 1, true, false)]);
        assert!(l.complete, "the log holds its write event");
        // the owner released it: the record took it, so it is an entry like any other
        let put = Change::Put {
            id: "mem-held".into(),
            digest: "d".into(),
            active: true,
        };
        events.push(ev(4, "owner", put));
        assert_eq!(
            edges(&forward(&one(events), "mem-x")),
            [("mem-held", 1, true, false)]
        );
    }

    #[test]
    fn a_write_the_record_refused_after_its_put_event_is_no_forward_edge() {
        // the log takes the `put` before the record does: the record then refused the
        // version, and the log says that the write failed
        let put = Change::Put {
            id: "mem-lost".into(),
            digest: "d".into(),
            active: true,
        };
        let failed = Change::Quarantined {
            id: "mem-lost".into(),
            of: 2,
            gate: crate::log::GATE_ERROR.into(),
            findings: vec![],
        };
        let logs = one(vec![
            ev(1, "s1", read(&["mem-x"])),
            ev(2, "s1", citing("mem-lost", &["mem-x"], None)),
            ev(3, "s1", put),
            ev(4, "s1", failed),
        ]);
        assert!(edges(&forward(&logs, "mem-x")).is_empty());
        assert_eq!(edges(&back(&logs, "mem-lost")), [("mem-x", 1, true, true)]);
    }

    #[test]
    fn forward_lists_a_send_a_reader_made_after_the_read_and_back_from_it_gives_the_entry() {
        let logs = one(vec![
            ev(1, "s1", send("https://tracker/1")),
            ev(2, "s1", read(&["mem-x"])),
            ev(3, "s1", send("https://tracker/1")),
            ev(4, "s1", send("https://tracker/1")),
            ev(5, "s1", send("anthropic")),
            ev(6, "s2", send("https://tracker/1")),
        ]);
        let sent = |to: &str, count, first, last| Sent {
            domain: "dom-a".into(),
            to: to.into(),
            session: "s1".into(),
            count,
            first,
            last,
        };
        assert_eq!(
            forward(&logs, "mem-x").sends,
            [
                sent("https://tracker/1", 2, 3, 4),
                sent("anthropic", 1, 5, 5)
            ]
        );
        let l = trace(&logs, "dom-a@3", Direction::Back, Some(1));
        assert_eq!(edges(&l), [("mem-x", 1, true, false)]);
        assert_eq!(
            (l.sessions.as_slice(), l.complete),
            (&["s1".to_string()][..], true)
        );
        // the send before the read carried nothing of it
        assert!(edges(&back(&logs, "dom-a@1")).is_empty());
        // and what names no send event is not known
        for of in ["dom-a@2", "dom-a@99", "dom-b@3", "dom-a@x"] {
            let l = back(&logs, of);
            assert!(!l.complete && l.entries.is_empty(), "{of}: {l:?}");
        }
    }

    #[test]
    fn a_cited_entry_is_a_cited_edge_when_its_session_read_nothing() {
        let logs = one(vec![ev(
            1,
            "s1",
            citing("mem-e", &["mem-c"], Some("mem-old")),
        )]);
        assert_eq!(
            edges(&back(&logs, "mem-e")),
            [("mem-c", 1, false, true), ("mem-old", 1, false, true)]
        );
        assert_eq!(edges(&forward(&logs, "mem-c")), [("mem-e", 1, false, true)]);
        assert_eq!(
            edges(&forward(&logs, "mem-old")),
            [("mem-e", 1, false, true)]
        );
        // read and cited: both
        let logs = one(vec![
            ev(1, "s1", read(&["mem-c"])),
            ev(2, "s1", citing("mem-e", &["mem-c"], None)),
        ]);
        assert_eq!(edges(&back(&logs, "mem-e")), [("mem-c", 1, true, true)]);
        assert_eq!(edges(&forward(&logs, "mem-c")), [("mem-e", 1, true, true)]);
    }

    #[test]
    fn an_entry_two_hops_away_has_hops_two_and_a_depth_of_one_leaves_it_out() {
        let logs = one(vec![
            ev(1, "s0", write("mem-x")),
            ev(2, "s1", read(&["mem-x"])),
            ev(3, "s1", write("mem-y")),
            ev(4, "s2", read(&["mem-y"])),
            ev(5, "s2", write("mem-z")),
            ev(6, "s2", send("anthropic")),
        ]);
        let l = forward(&logs, "mem-x");
        assert_eq!(
            edges(&l),
            [("mem-y", 1, true, false), ("mem-z", 2, true, false)]
        );
        assert_eq!(l.sessions, ["s1"], "the readers of what was asked about");
        assert_eq!(l.sends.len(), 1, "a send two hops away: {:?}", l.sends);
        let near = trace(&logs, "mem-x", Direction::Forward, Some(1));
        assert_eq!(edges(&near), [("mem-y", 1, true, false)]);
        assert!(near.sends.is_empty());
        let l = back(&logs, "mem-z");
        assert_eq!(
            edges(&l),
            [("mem-y", 1, true, false), ("mem-x", 2, true, false)]
        );
        assert!(l.complete, "each entry on the way has its write event");
        let near = trace(&logs, "mem-z", Direction::Back, Some(1));
        assert_eq!(edges(&near), [("mem-y", 1, true, false)]);
    }

    #[test]
    fn across_two_domains_the_order_is_by_time_and_a_time_that_does_not_parse_counts() {
        // as text, the time of the read sorts after the time of the later write
        let read_at = "2026-10-01T10:00:00.12Z";
        let logs = BTreeMap::from([
            (
                "dom-a".to_string(),
                vec![
                    at(read_at, ev(7, "s1", read(&["mem-x"]))),
                    at("", ev(8, "s2", read(&["mem-x"]))),
                ],
            ),
            (
                "dom-b".to_string(),
                vec![
                    at("2026-10-01T10:00:00.123Z", ev(1, "s1", write("mem-after"))),
                    at("2026-10-01T10:00:00.1Z", ev(2, "s1", write("mem-before"))),
                    at("2026-10-01T09:00:00Z", ev(3, "s2", write("mem-unknown"))),
                    at("2026-10-01T11:00:00Z", ev(9, "s3", write("mem-else"))),
                ],
            ),
        ]);
        // not by sequence number: both writes of s1 are below the read's number
        assert_eq!(
            edges(&forward(&logs, "mem-x")),
            [
                ("mem-after", 1, true, false),
                ("mem-unknown", 1, true, false)
            ]
        );
        assert_eq!(
            edges(&back(&logs, "mem-after")),
            [("mem-x", 1, true, false)]
        );
        assert!(edges(&back(&logs, "mem-before")).is_empty());
        assert_eq!(
            edges(&back(&logs, "mem-unknown")),
            [("mem-x", 1, true, false)]
        );
    }

    #[test]
    fn the_entry_asked_about_is_not_among_what_it_reached() {
        // two logs and no time that parses: each read counts for each write, so the way
        // back from the entry comes round to it
        let logs = BTreeMap::from([
            (
                "dom-a".to_string(),
                vec![
                    at("", ev(1, "s1", write("mem-x"))),
                    at("", ev(2, "s2", read(&["mem-x"]))),
                ],
            ),
            (
                "dom-b".to_string(),
                vec![
                    at("", ev(1, "s1", read(&["mem-y"]))),
                    at("", ev(2, "s2", write("mem-y"))),
                ],
            ),
        ]);
        assert_eq!(edges(&back(&logs, "mem-x")), [("mem-y", 1, true, false)]);
        assert_eq!(edges(&forward(&logs, "mem-x")), [("mem-y", 1, true, false)]);
    }

    #[test]
    fn an_entry_without_a_write_event_gives_complete_false() {
        let logs = one(vec![
            ev(1, "s1", read(&["mem-imported"])),
            ev(2, "s1", write("mem-e")),
        ]);
        let l = back(&logs, "mem-imported");
        assert!(!l.complete && l.entries.is_empty() && l.sessions.is_empty());
        // one step back from the entry made from it is known; the step after is not
        assert!(trace(&logs, "mem-e", Direction::Back, Some(1)).complete);
        assert!(!back(&logs, "mem-e").complete);
        assert!(forward(&logs, "mem-imported").complete);
    }

    fn req(body: &str) -> WriteRequest {
        WriteRequest {
            body: body.into(),
            ..WriteRequest::default()
        }
    }

    /// A root whose grants name `agent:security` in the security role, and its owner.
    fn root_with_security(dir: &std::path::Path) -> Memory {
        drop(Memory::init(dir, "human:test:me", "dom-me").unwrap());
        let grants = dir.join(crate::GRANTS_FILE);
        let text = std::fs::read_to_string(&grants).unwrap();
        std::fs::write(&grants, format!("security = [\"agent:security\"]\n{text}")).unwrap();
        Memory::open(dir).unwrap()
    }

    #[tokio::test]
    async fn an_author_outside_the_security_role_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let owner = root_with_security(dir.path());
        let x = owner.write(req("# Port\n\nIt listens on 8080.\n")).await;
        let x = x.unwrap().id;
        let worker = Memory::open(dir.path())
            .unwrap()
            .with_author("agent:worker");
        for direction in [Direction::Back, Direction::Forward] {
            let refused = worker.lineage(&x, direction, None).await;
            assert!(
                matches!(&refused, Err(Error::Denied(why)) if why.starts_with("lineage is for")),
                "{refused:?}"
            );
        }
        let security = Memory::open(dir.path())
            .unwrap()
            .with_author("agent:security");
        assert!(security.lineage(&x, Direction::Back, None).await.is_ok());
        assert!(owner.lineage(&x, Direction::Forward, None).await.is_ok());
        // a send is asked about back only
        let forward = owner.lineage("dom-me@1", Direction::Forward, None).await;
        assert!(matches!(forward, Err(Error::Denied(_))), "{forward:?}");
    }

    #[tokio::test]
    async fn lineage_follows_a_session_from_what_it_read_to_what_it_wrote_and_sent() {
        let dir = tempfile::tempdir().unwrap();
        let owner = root_with_security(dir.path());
        let x = owner.write(req("# Port\n\nIt listens on 8080.\n")).await;
        let x = x.unwrap().id;
        let reader = Memory::open(dir.path()).unwrap().with_author("agent:r");
        reader.get(&x).await.unwrap();
        let y = reader.write(req("# Proxy\n\nIt forwards to 8080.\n")).await;
        let y = y.unwrap().id;
        reader
            .keep_context("anthropic", "/v1/messages", b"{}", b"{}")
            .await
            .unwrap();
        let l = owner.lineage(&x, Direction::Forward, None).await.unwrap();
        assert_eq!(edges(&l), [(y.as_str(), 1, true, false)]);
        assert_eq!(l.sessions, [reader.session_id()]);
        assert_eq!(l.sends.len(), 1, "{:?}", l.sends);
        assert_eq!(
            (l.sends[0].to.as_str(), l.sends[0].session.as_str()),
            ("anthropic", reader.session_id())
        );
        let from = format!("dom-me@{}", l.sends[0].first);
        let l = owner.lineage(&from, Direction::Back, None).await.unwrap();
        assert_eq!(edges(&l), [(x.as_str(), 1, true, false)]);
        let l = owner.lineage(&y, Direction::Back, Some(1)).await.unwrap();
        assert_eq!(edges(&l), [(x.as_str(), 1, true, false)]);
        assert_eq!(l.sessions, [reader.session_id()]);
    }

    #[tokio::test]
    async fn lineage_leaves_out_an_event_above_what_the_session_may_handle() {
        let dir = tempfile::tempdir().unwrap();
        let owner = root_with_security(dir.path());
        let x = owner.write(req("# Port\n\nIt listens on 8080.\n")).await;
        let x = x.unwrap().id;
        let reader = Memory::open(dir.path()).unwrap().with_author("agent:r");
        reader.get(&x).await.unwrap();
        let open = reader.write(req("# Proxy\n\nIt forwards to 8080.\n")).await;
        let open = open.unwrap().id;
        let secret = WriteRequest {
            classification: Some(Label::Restricted),
            ..req("# Vault\n\nThe vault listens on 8080 too.\n")
        };
        let secret = reader.write(secret).await.unwrap().id;
        let all = owner.lineage(&x, Direction::Forward, None).await.unwrap();
        let mut both = vec![open.as_str(), secret.as_str()];
        both.sort_unstable();
        let reached: Vec<&str> = all.entries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(reached, both);
        let capped = Memory::open(dir.path())
            .unwrap()
            .with_author("agent:security")
            .with_handling_cap(Label::Internal);
        let some = capped.lineage(&x, Direction::Forward, None).await.unwrap();
        assert_eq!(edges(&some), [(open.as_str(), 1, true, false)]);
    }

    #[tokio::test]
    async fn lineage_copies_the_ledger_s_events_into_the_log_before_it_reads_it() {
        let dir = tempfile::tempdir().unwrap();
        let owner = root_with_security(dir.path());
        let x = owner.write(req("# Port\n\nIt listens on 8080.\n")).await;
        let x = x.unwrap().id;
        let spec = dm_ledger::TaskSpec {
            domain: "dom-me".into(),
            title: "Move the port".into(),
            body: "Do it.\n".into(),
            ..dm_ledger::TaskSpec::default()
        };
        owner.create_task(spec).await.unwrap();
        let copied = || {
            crate::log::read(dir.path(), "dom-me", 0)
                .unwrap()
                .iter()
                .filter(|e| matches!(e.change, Change::Ledger { .. }))
                .count()
        };
        assert_eq!(copied(), 0);
        owner.lineage(&x, Direction::Back, None).await.unwrap();
        assert!(copied() > 0);
    }
}
