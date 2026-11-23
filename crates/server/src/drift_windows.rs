//! Drift windows: the interlock between vendor drift and promotion (the paper's
//! section 11). When a model drifts, what agents wrote from then on may hold the
//! vendor's change, so detected drift opens a window for the destination and model,
//! and nothing that an agent wrote inside a window is promoted.
//!
//! A window starts at a time and stays open until the owner closes it. An entry is
//! inside a window when its `created` is at or after the start and, for a closed
//! window, at or before the end. Such an entry stays unpromotable after the window
//! closes: the remedy is to write it again. An entry does not record its session or
//! its model, so a window holds every entry that an agent wrote inside it, on any
//! model. An entry that a person wrote is not held.
//!
//! On disk, under the root: `state/drift/windows.jsonl`, one event on each line, opened
//! or closed. The windows are the fold of the events. A line that does not parse is
//! skipped, as the event log skips a line that a crash cut short.
//!
//! Every time is a parameter. The detectors run in the binary (`amem drift`); they call
//! [`Memory::open_drift_window`] when they confirm drift.

use crate::{reads, Error, Memory};
use dm_core::{Entry, TrustTier};
use serde::{Deserialize, Serialize};
use std::path::Path;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// Where the window events are, under the root.
pub const FILE: &str = "state/drift/windows.jsonl";

/// The most characters a destination or a model name holds.
const MAX_NAME: usize = 200;

/// One drift window.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Window {
    /// The destination whose model drifted.
    pub destination: String,
    /// The model, as the destination names it.
    pub model: String,
    /// When the window starts.
    #[serde(with = "time::serde::rfc3339")]
    pub from: OffsetDateTime,
    /// When the window was closed; none while it is open.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub until: Option<OffsetDateTime>,
    /// The detectors that fired; empty for a window that the owner opened by hand.
    pub detectors: Vec<String>,
    /// Why the window was opened.
    pub why: String,
    /// Why the window was closed; none while it is open.
    #[serde(default)]
    pub closed_why: Option<String>,
}

impl Window {
    /// Whether `at` is inside the window. The start is inside, and so is the end of a
    /// closed window.
    #[must_use]
    pub fn holds(&self, at: OffsetDateTime) -> bool {
        self.from <= at && self.until.is_none_or(|until| at <= until)
    }

    /// Whether the span from `from` to `to`, both inside the span, has an instant in
    /// common with the window.
    #[must_use]
    pub fn overlaps(&self, from: OffsetDateTime, to: OffsetDateTime) -> bool {
        self.from <= to && self.until.is_none_or(|until| from <= until)
    }
}

/// One line of the file.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum Event {
    Opened {
        destination: String,
        model: String,
        #[serde(with = "time::serde::rfc3339")]
        from: OffsetDateTime,
        detectors: Vec<String>,
        why: String,
        by: String,
    },
    Closed {
        destination: String,
        model: String,
        #[serde(with = "time::serde::rfc3339")]
        at: OffsetDateTime,
        why: String,
        by: String,
    },
}

/// The open window of a destination and model among `windows`.
fn open_one<'a>(
    windows: &'a mut [Window],
    destination: &str,
    model: &str,
) -> Option<&'a mut Window> {
    windows
        .iter_mut()
        .find(|w| w.until.is_none() && w.destination == destination && w.model == model)
}

/// The windows that `events` leave, in the order they were opened. An event that opens
/// a window for a destination and model that has an open one adds nothing, and an
/// event that closes one where none is open changes nothing.
fn fold(events: impl IntoIterator<Item = Event>) -> Vec<Window> {
    let mut out: Vec<Window> = Vec::new();
    for event in events {
        match event {
            Event::Opened {
                destination,
                model,
                from,
                detectors,
                why,
                ..
            } => {
                if open_one(&mut out, &destination, &model).is_none() {
                    out.push(Window {
                        destination,
                        model,
                        from,
                        until: None,
                        detectors,
                        why,
                        closed_why: None,
                    });
                }
            }
            Event::Closed {
                destination,
                model,
                at,
                why,
                ..
            } => {
                if let Some(w) = open_one(&mut out, &destination, &model) {
                    w.until = Some(at);
                    w.closed_why = Some(why);
                }
            }
        }
    }
    out
}

/// The windows under `root`. A root with no file has none, and no file is made.
fn read(root: &Path) -> Result<Vec<Window>, Error> {
    let path = root.join(FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(crate::io(&path)(e)),
    };
    Ok(fold(
        text.lines()
            .filter_map(|l| serde_json::from_str::<Event>(l).ok()),
    ))
}

/// Append one event. A file whose last line a crash cut short gets a line end first, so
/// the event stays a line of its own and only the torn line is skipped.
fn append(root: &Path, event: &Event) -> Result<(), Error> {
    let path = root.join(FILE);
    let mut line = serde_json::to_string(event)
        .map_err(|e| Error::Denied(format!("the drift window cannot be written: {e}")))?;
    let torn = std::fs::read(&path).is_ok_and(|b| b.last().is_some_and(|c| *c != b'\n'));
    if torn {
        line.insert(0, '\n');
    }
    reads::try_append_line(&path, &line).map_err(crate::io(&path))
}

/// `name` when it is a name on one line: not empty, no space at either end, no control
/// character, and at most [`MAX_NAME`] characters.
fn one_line<'a>(what: &str, name: &'a str) -> Result<&'a str, Error> {
    let fits = !name.is_empty()
        && name.trim() == name
        && name.chars().count() <= MAX_NAME
        && !name.chars().any(char::is_control);
    if fits {
        Ok(name)
    } else {
        Err(Error::Denied(format!(
            "the {what} of a drift window is a name on one line, of at most {MAX_NAME} characters"
        )))
    }
}

fn said(what: &str, why: &str) -> Result<String, Error> {
    let why = why.trim();
    if why.is_empty() {
        return Err(Error::Denied(format!("say why the drift window {what}")));
    }
    Ok(why.to_string())
}

impl Memory {
    /// Open a drift window for a destination and model, from `from` on. When that
    /// destination and model has an open window, that window is the answer and nothing
    /// is written: one change opens one window, however often it is detected.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for a destination or a model that is not a name on one line,
    /// or an empty why; the file system.
    pub fn open_drift_window(
        &self,
        destination: &str,
        model: &str,
        from: OffsetDateTime,
        detectors: &[String],
        why: &str,
    ) -> Result<Window, Error> {
        let destination = one_line("destination", destination)?;
        let model = one_line("model", model)?;
        let why = said("opens", why)?;
        let mut windows = read(&self.root)?;
        if let Some(open) = open_one(&mut windows, destination, model) {
            return Ok(open.clone());
        }
        let window = Window {
            destination: destination.to_string(),
            model: model.to_string(),
            from,
            until: None,
            detectors: detectors.to_vec(),
            why,
            closed_why: None,
        };
        append(
            &self.root,
            &Event::Opened {
                destination: window.destination.clone(),
                model: window.model.clone(),
                from,
                detectors: window.detectors.clone(),
                why: window.why.clone(),
                by: self.author.clone(),
            },
        )?;
        Ok(window)
    }

    /// Close the open drift window of a destination and model, at `at`. The entries
    /// that agents wrote inside the window stay unpromotable. The owner's alone.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for anyone but the owner, for an empty why, when no window is
    /// open for the destination and model, or when `at` is before the start of the
    /// window; the file system.
    pub fn close_drift_window(
        &self,
        destination: &str,
        model: &str,
        at: OffsetDateTime,
        why: &str,
    ) -> Result<Window, Error> {
        if self.author != self.grants.owner {
            return Err(Error::Denied(format!(
                "the owner closes a drift window: {}",
                self.grants.owner
            )));
        }
        let why = said("closes", why)?;
        let mut windows = read(&self.root)?;
        let Some(open) = open_one(&mut windows, destination, model) else {
            return Err(Error::Denied(format!(
                "no drift window is open for {destination}/{model}"
            )));
        };
        if at < open.from {
            return Err(Error::Denied(format!(
                "the drift window of {destination}/{model} starts at {}; it does not close before that",
                open.from.format(&Rfc3339).unwrap_or_default()
            )));
        }
        append(
            &self.root,
            &Event::Closed {
                destination: destination.to_string(),
                model: model.to_string(),
                at,
                why: why.clone(),
                by: self.author.clone(),
            },
        )?;
        open.until = Some(at);
        open.closed_why = Some(why);
        Ok(open.clone())
    }

    /// Every drift window, open and closed, in the order they were opened.
    ///
    /// # Errors
    ///
    /// The file system.
    pub fn drift_windows(&self) -> Result<Vec<Window>, Error> {
        read(&self.root)
    }

    /// The window that holds `entry` from promotion: the first one its `created` is
    /// inside, when an agent wrote it. None for an entry that a person wrote.
    pub(crate) fn frozen_by(&self, entry: &Entry) -> Result<Option<Window>, Error> {
        if !entry.front.author.starts_with("agent:") {
            return Ok(None);
        }
        Ok(read(&self.root)?
            .into_iter()
            .find(|w| w.holds(entry.front.created)))
    }

    /// Refuse the promotion of an entry that a drift window holds. Every promotion and
    /// every approval asks this before anything is recorded.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`], naming the window, for an entry that an agent wrote inside
    /// one; the file system.
    pub(crate) fn refuse_frozen(&self, entry: &Entry) -> Result<(), Error> {
        let Some(w) = self.frozen_by(entry)? else {
            return Ok(());
        };
        Err(Error::Denied(format!(
            "{} was written by an agent inside the drift window of {}/{}, which started at {}. Nothing that an agent wrote in a drift window is promoted: the change of the model may be in it. When the window is closed, write the entry again and promote that one",
            entry.front.id,
            w.destination,
            w.model,
            w.from.format(&Rfc3339).unwrap_or_default()
        )))
    }

    /// The tier a write at `at` lands at. A curator's rewrite lands curated, which is a
    /// promotion made by the write; inside a drift window an agent's write lands raw.
    pub(crate) fn tier_outside_windows(
        &self,
        tier: TrustTier,
        at: OffsetDateTime,
    ) -> Result<TrustTier, Error> {
        if tier == TrustTier::Raw || !self.author.starts_with("agent:") {
            return Ok(tier);
        }
        let held = read(&self.root)?.iter().any(|w| w.holds(at));
        Ok(if held { TrustTier::Raw } else { tier })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Promotion, WriteRequest};
    use dm_ledger::{Status, Task, TaskSpec};
    use time::macros::datetime;
    use time::Duration;

    const WHY: &str = "checked against the tree";
    const T: OffsetDateTime = datetime!(2026-11-02 10:00:00 UTC);

    fn req(body: &str) -> WriteRequest {
        WriteRequest {
            body: body.into(),
            ..WriteRequest::default()
        }
    }

    fn window(from: OffsetDateTime, until: Option<OffsetDateTime>) -> Window {
        Window {
            destination: "llmao".into(),
            model: "m".into(),
            from,
            until,
            detectors: vec!["flip_rate".into()],
            why: "the answers moved".into(),
            closed_why: until.map(|_| "pinned".to_string()),
        }
    }

    fn detectors() -> Vec<String> {
        vec!["flip_rate".into(), "format".into()]
    }

    /// A root with its owner, a session of the agent that writes, and a session of
    /// the curator.
    fn sessions(dir: &Path) -> (Memory, Memory, Memory) {
        let owner = Memory::init(dir, "human:test:me", "dom-me").unwrap();
        let writer = Memory::open(dir).unwrap().with_author("agent:writer");
        let curator = Memory::open(dir).unwrap().with_author("agent:curator");
        (owner, writer, curator)
    }

    /// An entry that `m` writes now, and its `created`.
    async fn written(m: &Memory, body: &str) -> (String, OffsetDateTime) {
        let id = m.write(req(body)).await.unwrap().id;
        let created = m.get(&id).await.unwrap().front.created;
        (id, created)
    }

    async fn tasks(m: &Memory) -> Vec<Task> {
        let mut out = Vec::new();
        for st in [
            Status::Proposed,
            Status::Open,
            Status::Claimed,
            Status::Done,
            Status::Invalidated,
        ] {
            out.extend(m.tasks(None, st).await.unwrap());
        }
        out
    }

    fn refusal(r: Result<Promotion, Error>) -> String {
        match r {
            Err(Error::Denied(why)) => why,
            other => panic!("not refused: {other:?}"),
        }
    }

    #[test]
    fn drift_windows_hold_their_start_and_their_end() {
        let ns = Duration::nanoseconds(1);
        let open = window(T, None);
        assert!(open.holds(T), "the start is inside");
        assert!(!open.holds(T - ns));
        assert!(open.holds(T + Duration::days(900)), "open: no end");
        let end = T + Duration::hours(2);
        let closed = window(T, Some(end));
        assert!(closed.holds(end), "the end is inside");
        assert!(!closed.holds(end + ns));
        assert!(!closed.holds(T - ns));
        // a span and a window that share one instant overlap
        assert!(closed.overlaps(T - Duration::hours(1), T));
        assert!(!closed.overlaps(T - Duration::hours(1), T - ns));
        assert!(closed.overlaps(end, end + Duration::hours(1)));
        assert!(!closed.overlaps(end + ns, end + Duration::hours(1)));
        assert!(closed.overlaps(T + ns, end - ns), "a span inside");
        assert!(
            closed.overlaps(T - ns, end + ns),
            "a span around the window"
        );
        assert!(open.overlaps(end + ns, end + Duration::hours(1)));
        assert!(!open.overlaps(T - Duration::hours(1), T - ns));
    }

    fn lines(dir: &Path) -> usize {
        let text = std::fs::read_to_string(dir.join(FILE)).unwrap();
        text.lines().count()
    }

    #[test]
    fn drift_windows_open_once_for_a_destination_and_model() {
        let dir = tempfile::tempdir().unwrap();
        let (owner, _, curator) = sessions(dir.path());
        let first = owner
            .open_drift_window("llmao", "m", T, &detectors(), "the answers moved")
            .unwrap();
        assert_eq!(
            first,
            Window {
                detectors: detectors(),
                ..window(T, None)
            }
        );
        assert_eq!(lines(dir.path()), 1);
        // detected again, later, by another session: the same window, and no line
        let again = curator
            .open_drift_window("llmao", "m", T + Duration::hours(3), &[], "again")
            .unwrap();
        assert_eq!(again, first);
        assert_eq!(lines(dir.path()), 1, "nothing is appended");
        // another model has a window of its own
        owner
            .open_drift_window("llmao", "m-2", T, &[], "by hand")
            .unwrap();
        assert_eq!(lines(dir.path()), 2);
        assert_eq!(owner.drift_windows().unwrap().len(), 2);
    }

    #[test]
    fn drift_windows_are_the_fold_of_the_lines_that_parse() {
        let dir = tempfile::tempdir().unwrap();
        let (owner, _, _) = sessions(dir.path());
        let first = owner
            .open_drift_window("llmao", "m", T, &[], "the answers moved")
            .unwrap();
        let file = dir.path().join(FILE);
        // two sessions that open at the same time can both write: the first one stands
        let mut text = std::fs::read_to_string(&file).unwrap();
        let second = text.replace("10:00:00", "11:00:00");
        assert_ne!(second, text);
        text.push_str(&second);
        // and a line that a crash cut short
        text.push_str("{\"event\":\"opened\",\"destination\":");
        std::fs::write(&file, text).unwrap();
        assert_eq!(owner.drift_windows().unwrap(), [first]);
        // an event after the torn line is a line of its own
        owner
            .open_drift_window("llmao", "m-2", T, &[], "by hand")
            .unwrap();
        assert_eq!(lines(dir.path()), 4, "the torn line, and one window more");
        assert_eq!(owner.drift_windows().unwrap().len(), 2);
    }

    #[test]
    fn drift_windows_take_names_on_one_line_and_a_why() {
        let dir = tempfile::tempdir().unwrap();
        let (owner, _, _) = sessions(dir.path());
        let long = "m".repeat(MAX_NAME + 1);
        for (destination, model, why) in [
            ("", "m", "why"),
            ("llmao", "m\nn", "why"),
            ("llmao", " m", "why"),
            ("llmao", long.as_str(), "why"),
            ("llmao", "m", "  "),
        ] {
            let refused = owner.open_drift_window(destination, model, T, &[], why);
            assert!(
                matches!(refused, Err(Error::Denied(_))),
                "{destination:?} {model:?} {why:?}"
            );
        }
        assert!(!dir.path().join(FILE).exists(), "a refusal writes nothing");
        let most = owner.open_drift_window("llmao", &long[1..], T, &[], "why");
        assert!(most.is_ok(), "{most:?}");
    }

    /// A root with one open window for `llmao/m` from [`T`], its owner and its curator.
    fn with_open_window(dir: &Path) -> (Memory, Memory) {
        let (owner, _, curator) = sessions(dir);
        owner
            .open_drift_window("llmao", "m", T, &detectors(), "the answers moved")
            .unwrap();
        (owner, curator)
    }

    fn denied(r: &Result<Window, Error>, part: &str) -> bool {
        matches!(r, Err(Error::Denied(why)) if why.contains(part))
    }

    #[test]
    fn drift_windows_close_is_refused_without_the_owner_a_why_or_a_window() {
        let dir = tempfile::tempdir().unwrap();
        let (owner, _, _) = sessions(dir.path());
        let end = T + Duration::hours(2);
        let none = owner.close_drift_window("llmao", "m", end, "pinned");
        assert!(denied(&none, "no drift window is open"), "{none:?}");
        assert!(!dir.path().join(FILE).exists(), "a refusal writes nothing");
        let dir = tempfile::tempdir().unwrap();
        let (owner, curator) = with_open_window(dir.path());
        let agent = curator.close_drift_window("llmao", "m", end, "pinned");
        assert!(denied(&agent, "the owner closes"), "{agent:?}");
        let silent = owner.close_drift_window("llmao", "m", end, " ");
        assert!(denied(&silent, "say why"), "{silent:?}");
        let early = owner.close_drift_window("llmao", "m", T - Duration::seconds(1), "pinned");
        assert!(denied(&early, "does not close before"), "{early:?}");
        let other = owner.close_drift_window("llmao", "m-2", end, "pinned");
        assert!(denied(&other, "no drift window is open"), "{other:?}");
        assert_eq!(lines(dir.path()), 1, "the refusals wrote nothing");
        assert!(owner.drift_windows().unwrap()[0].until.is_none());
    }

    #[test]
    fn drift_windows_close_for_the_owner_and_the_next_change_opens_a_new_one() {
        let dir = tempfile::tempdir().unwrap();
        let (owner, _) = with_open_window(dir.path());
        let end = T + Duration::hours(2);
        let closed = owner
            .close_drift_window("llmao", "m", end, "pinned")
            .unwrap();
        assert_eq!(
            closed,
            Window {
                detectors: detectors(),
                ..window(T, Some(end))
            }
        );
        assert_eq!(owner.drift_windows().unwrap(), [closed]);
        // closed: no window is open now, and the next change opens a new one
        let twice = owner.close_drift_window("llmao", "m", end, "pinned");
        assert!(denied(&twice, "no drift window is open"), "{twice:?}");
        let next = owner
            .open_drift_window("llmao", "m", end + Duration::days(1), &[], "moved again")
            .unwrap();
        assert_eq!(next.from, end + Duration::days(1));
        assert_eq!(owner.drift_windows().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn drift_windows_freeze_a_promotion_to_curated_and_a_proposal() {
        let dir = tempfile::tempdir().unwrap();
        let (owner, writer, curator) = sessions(dir.path());
        let (id, created) = written(&writer, "The agent's fact.\n").await;
        let from = created - Duration::hours(1);
        owner
            .open_drift_window("llmao", "m", from, &detectors(), "the answers moved")
            .unwrap();
        let direct = refusal(curator.promote(&id, TrustTier::Curated, WHY).await);
        for part in [
            id.as_str(),
            "llmao/m",
            &from.format(&Rfc3339).unwrap(),
            "Nothing that an agent wrote in a drift window is promoted",
        ] {
            assert!(direct.contains(part), "{part}: {direct}");
        }
        let proposal = refusal(curator.promote(&id, TrustTier::Verified, WHY).await);
        assert!(proposal.contains("llmao/m"), "{proposal}");
        // the owner is refused as the curator is
        let by_owner = refusal(owner.promote(&id, TrustTier::Curated, WHY).await);
        assert!(by_owner.contains("llmao/m"), "{by_owner}");
        // nothing was recorded, and the entry is as it was
        assert!(owner.promotions(&id).await.unwrap().is_empty());
        assert!(tasks(&owner).await.is_empty(), "no proposal task");
        assert_eq!(
            owner.get(&id).await.unwrap().front.trust_tier,
            TrustTier::Raw
        );
        // closed, the window still holds what was written inside it
        owner
            .close_drift_window("llmao", "m", created + Duration::hours(1), "pinned")
            .unwrap();
        let after = refusal(curator.promote(&id, TrustTier::Curated, WHY).await);
        assert!(after.contains("llmao/m"), "{after}");
        assert!(owner.promotions(&id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn drift_windows_freeze_an_approval() {
        let dir = tempfile::tempdir().unwrap();
        let (owner, writer, curator) = sessions(dir.path());
        let (id, created) = written(&writer, "The agent's fact.\n").await;
        // proposed before the drift was detected
        let Promotion::Proposed { task } = curator
            .promote(&id, TrustTier::Verified, WHY)
            .await
            .unwrap()
        else {
            panic!("verified is proposed")
        };
        // the window starts before the entry: at the run before the one that fired
        owner
            .open_drift_window(
                "llmao",
                "m",
                created - Duration::hours(1),
                &detectors(),
                "the answers moved",
            )
            .unwrap();
        let refused = refusal(owner.approve(&task.id).await);
        assert!(refused.contains("llmao/m"), "{refused}");
        assert_eq!(
            owner.get(&id).await.unwrap().front.trust_tier,
            TrustTier::Raw
        );
        let left = owner.get_task(&task.id).await.unwrap();
        assert_ne!(left.status, Status::Done, "the proposal is not approved");
        assert_ne!(left.status, Status::Claimed, "and nobody took it");
        let records = owner.promotions(&id).await.unwrap();
        assert_eq!(records.len(), 1, "the proposal's record, and no other");
        assert!(!records[0].landed);
    }

    #[tokio::test]
    async fn drift_windows_leave_entries_outside_them_and_entries_of_people() {
        let dir = tempfile::tempdir().unwrap();
        let (owner, writer, curator) = sessions(dir.path());
        // promotions work with no windows file, and make none
        let other = written(&writer, "Another fact.\n").await.0;
        let p = curator.promote(&other, TrustTier::Curated, WHY).await;
        assert!(matches!(p, Ok(Promotion::Promoted { .. })), "{p:?}");
        assert!(!dir.path().join(FILE).exists());
        // one entry of an agent: a window that closed before it was written, and a
        // window that opens after it
        let (id, created) = written(&writer, "The agent's fact.\n").await;
        let hour = Duration::hours(1);
        owner
            .open_drift_window("llmao", "m", created - 2 * hour, &[], "the answers moved")
            .unwrap();
        owner
            .close_drift_window("llmao", "m", created - hour, "pinned")
            .unwrap();
        owner
            .open_drift_window("llmao", "m-2", created + hour, &[], "moved later")
            .unwrap();
        let p = curator.promote(&id, TrustTier::Curated, WHY).await;
        assert!(matches!(p, Ok(Promotion::Promoted { .. })), "{p:?}");
        // a person's entry inside a window is promoted as before
        let (mine, t_mine) = written(&owner, "The owner's fact.\n").await;
        owner
            .open_drift_window("llmao", "m-3", t_mine - hour, &[], "moved too")
            .unwrap();
        let p = curator.promote(&mine, TrustTier::Curated, WHY).await;
        assert!(matches!(p, Ok(Promotion::Promoted { .. })), "{p:?}");
        // and an agent's entry inside the same window is not
        let inside = writer.write(req("Written inside the window.\n")).await;
        let held = refusal(
            curator
                .promote(&inside.unwrap().id, TrustTier::Curated, WHY)
                .await,
        );
        assert!(held.contains("llmao/m-3"), "{held}");
    }

    #[tokio::test]
    async fn drift_windows_make_a_curator_s_rewrite_land_raw() {
        let dir = tempfile::tempdir().unwrap();
        let (owner, _, _) = sessions(dir.path());
        let task = owner
            .file_own(TaskSpec {
                domain: "dom-me".into(),
                title: "Curator: rewrite".into(),
                body: "b".into(),
                origin: Some("curator:resummarise".into()),
                upstream_event: Some("x".into()),
                classification: dm_core::Label::Public,
                ..TaskSpec::default()
            })
            .await
            .unwrap()
            .task;
        let curator = Memory::open(dir.path())
            .unwrap()
            .with_author("agent:curator");
        curator.set_held_task(&task.id);
        let rewrite = |old: String, body: &str| WriteRequest {
            supersedes: Some(old),
            ..req(body)
        };
        let first = owner.write(req("Clipped summary.\n")).await.unwrap().id;
        let outside = curator
            .write(rewrite(first, "A summary.\n"))
            .await
            .unwrap()
            .id;
        assert_eq!(tier_of(&owner, &outside).await, TrustTier::Curated);
        // an open window that started long before this test runs
        owner
            .open_drift_window("llmao", "m", T, &[], "the answers moved")
            .unwrap();
        let inside = curator
            .write(rewrite(outside, "A better summary.\n"))
            .await
            .unwrap()
            .id;
        assert_eq!(tier_of(&owner, &inside).await, TrustTier::Raw);
        // a person who curates inside the window is not held
        owner.set_held_task(&task.id);
        let by_hand = owner
            .write(rewrite(inside, "The owner's summary.\n"))
            .await
            .unwrap()
            .id;
        assert_eq!(tier_of(&owner, &by_hand).await, TrustTier::Curated);
    }

    async fn tier_of(m: &Memory, id: &str) -> TrustTier {
        m.get(id).await.unwrap().front.trust_tier
    }
}
