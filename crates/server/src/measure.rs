//! Measurement from the ledger: the times of a task, the share of the completed work
//! that is upkeep, the work that the fleet induced, and the landing queue.
//!
//! Every time here is the ledger's: a window ends at the ledger's clock, and a duration
//! is the difference of two moments that the ledger appended.
//!
//! A landing queue reuses the ledger's wait. A holder that queues its work gives the task
//! up until the event `landing:<queue>` comes, with its handoff on it. A person in person
//! lands the head of the queue, and that closes the task with the handoff.

use crate::{fabric, lock, Error, Memory};
use dm_ledger::{Ledger, Moment, Status, Task};
use dm_policy::decide::Class;
use dm_policy::LandingQueue;
use serde::Serialize;
use std::collections::BTreeMap;
use time::{Duration, OffsetDateTime};

/// The prefix of the event that a queued task waits on.
pub const LANDING_EVENT: &str = "landing:";

/// The event that a task in the landing queue `queue` waits on.
#[must_use]
pub fn landing_event(queue: &str) -> String {
    format!("{LANDING_EVENT}{queue}")
}

/// The steps that the walk to a task's root takes at most.
const ROOT_STEPS: usize = 64;

/// The times of one task, from the ledger's moments.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TaskTimes {
    /// The task.
    pub task: String,
    /// The first claim of it.
    #[serde(with = "time::serde::rfc3339::option")]
    pub claimed: Option<OffsetDateTime>,
    /// Its completion, when it is done.
    #[serde(with = "time::serde::rfc3339::option")]
    pub completed: Option<OffsetDateTime>,
    /// When its holder first handed it to a landing queue.
    #[serde(with = "time::serde::rfc3339::option")]
    pub queued: Option<OffsetDateTime>,
    /// From the first claim to the completion, for a done task that was ever claimed.
    pub claim_to_done_secs: Option<i64>,
    /// From the first claim to the landing, for a done task that went through a queue.
    pub claim_to_landed_secs: Option<i64>,
    /// The landing queue it went through.
    pub queue: Option<String>,
}

/// What kind of work a completed task was.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Work {
    /// A person or a tap asked for it, directly or through the task it is a part of.
    Requested,
    /// An agent or the system filed it, and another principal completed it.
    Induced,
    /// Curation, or a task that an agent filed and completed itself.
    Upkeep,
}

/// How the completed work of a domain divides, over a window that ends now.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Flow {
    /// The domain.
    pub domain: String,
    /// The length of the window, in days.
    pub window_days: u32,
    /// The tasks completed in the window.
    pub done: usize,
    /// Of those, the requested work.
    pub requested: usize,
    /// Of those, the induced work.
    pub induced: usize,
    /// Of those, the upkeep.
    pub upkeep: usize,
    /// Of the upkeep, the tasks that an agent filed and completed itself.
    pub self_filed: usize,
    /// Upkeep over done; `None` when nothing was completed.
    pub upkeep_share: Option<f64>,
    /// The tasks not yet done whose root an agent or the system filed, curation left out.
    pub induced_open: usize,
    /// The median time from the first claim to the completion.
    pub median_claim_to_done_secs: Option<i64>,
}

/// One change that waits in a landing queue.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Queued {
    /// The task.
    pub task: String,
    /// Its title.
    pub title: String,
    /// Who did the work: the holder of its last claim.
    pub by: String,
    /// Whether that is a person.
    pub person: bool,
    /// When it was queued.
    #[serde(with = "time::serde::rfc3339")]
    pub since: OffsetDateTime,
}

/// A landing queue as it stands.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LandingView {
    /// The queue's name.
    pub queue: String,
    /// The domain whose tasks it lands.
    pub domain: String,
    /// The changes that wait, in landing order.
    pub waiting: Vec<Queued>,
    /// The share of the fleet: the changes of agents that land in 24 hours.
    pub per_day: Option<u32>,
    /// The changes of agents that landed in the last 24 hours.
    pub fleet_landed_last_day: usize,
    /// The landings that are older than the queue's delay, and so count as progress.
    pub landed_counted: usize,
    /// The landings that are newer than the delay.
    pub landed_not_yet_counted: usize,
    /// The median time from the first claim to the landing.
    pub median_claim_to_landed_secs: Option<i64>,
}

/// The moments of a domain, by task.
struct Moments(BTreeMap<String, Vec<Moment>>);

impl Moments {
    async fn of(l: &Ledger, domain: &str) -> Result<Moments, Error> {
        let mut by: BTreeMap<String, Vec<Moment>> = BTreeMap::new();
        for m in l.moments(domain).await? {
            by.entry(m.task.clone()).or_default().push(m);
        }
        Ok(Moments(by))
    }

    fn kind<'a>(
        &'a self,
        task: &str,
        kind: &'a str,
    ) -> impl DoubleEndedIterator<Item = &'a Moment> {
        let all = self.0.get(task).map_or(&[][..], Vec::as_slice);
        all.iter().filter(move |m| m.kind == kind)
    }

    /// Who did the work: the holder of the last claim.
    fn worker(&self, task: &str) -> Option<&str> {
        self.kind(task, "claimed")
            .next_back()
            .and_then(|m| m.holder.as_deref())
    }

    fn first_claim(&self, task: &str) -> Option<OffsetDateTime> {
        self.kind(task, "claimed").next().map(|m| m.at)
    }

    fn completed(&self, task: &str) -> Option<OffsetDateTime> {
        self.kind(task, "completed").next_back().map(|m| m.at)
    }

    /// When the task was last handed to the queue of `event`.
    fn queued_on(&self, task: &str, event: &str) -> Option<OffsetDateTime> {
        self.kind(task, "waiting")
            .rfind(|m| m.on.as_deref() == Some(event))
            .map(|m| m.at)
    }

    /// When the task landed through the queue of `event`: the completion that the
    /// arrival of that event came with.
    fn landed_on(&self, task: &str, event: &str) -> Option<OffsetDateTime> {
        let done = self.completed(task)?;
        self.kind(task, "arrived")
            .any(|m| m.on.as_deref() == Some(event) && m.at == done)
            .then_some(done)
    }

    fn times(&self, task: &Task) -> TaskTimes {
        let claimed = self.first_claim(&task.id);
        let completed = self
            .completed(&task.id)
            .filter(|_| task.status == Status::Done);
        let first = self.kind(&task.id, "waiting").find(|m| {
            m.on.as_deref()
                .is_some_and(|o| o.starts_with(LANDING_EVENT))
        });
        let secs = claimed.zip(completed).map(|(c, d)| (d - c).whole_seconds());
        let queue = first
            .and_then(|m| m.on.as_deref())
            .and_then(|o| o.strip_prefix(LANDING_EVENT));
        TaskTimes {
            task: task.id.clone(),
            claimed,
            completed,
            queued: first.map(|m| m.at),
            claim_to_done_secs: secs,
            claim_to_landed_secs: secs.filter(|_| first.is_some()),
            queue: queue.map(str::to_string),
        }
    }
}

/// The task that `task` was filed from: its parent, or the task that its origin names.
fn above(task: &Task) -> Option<&str> {
    if let Some(parent) = task.spec.parent.as_deref() {
        return Some(parent);
    }
    let origin = task.spec.origin.as_deref()?;
    ["task:", "continues:", "retries:"]
        .iter()
        .find_map(|p| origin.strip_prefix(p))
}

/// The root of a task: the last task that the walk up finds. An ancestor that the ledger
/// does not hold ends the walk.
async fn root_of(l: &Ledger, task: &Task) -> Task {
    let mut root = task.clone();
    for _ in 0..ROOT_STEPS {
        let Some(id) = above(&root).map(str::to_string) else {
            break;
        };
        match l.get(&id).await {
            Ok(t) => root = t,
            Err(_) => break,
        }
    }
    root
}

fn is_person(who: &str, owner: &str) -> bool {
    matches!(Class::of(who, owner), Class::Owner | Class::Human)
}

/// Whether an agent or the system filed the task: no person and no tap asked for it.
fn fleet_filed(root: &Task, owner: &str) -> bool {
    let class = Class::of(&root.spec.created_by, owner);
    !matches!(class, Class::Owner | Class::Human | Class::Tap)
}

/// What kind of work `task` was. Its root decides: curation is upkeep, and what a person
/// or a tap filed is requested. Of the rest, a task that its filer also completed is
/// upkeep, and a task that another principal completed is induced.
#[must_use]
pub fn work_of(task: &Task, root: &Task, completer: Option<&str>, owner: &str) -> Work {
    if fabric::maintenance(root) {
        Work::Upkeep
    } else if !fleet_filed(root, owner) {
        Work::Requested
    } else if completer == Some(task.spec.created_by.as_str()) {
        Work::Upkeep
    } else {
        Work::Induced
    }
}

/// The median of the durations; the lower of the two middle ones for an even count.
fn median(mut secs: Vec<i64>) -> Option<i64> {
    secs.sort_unstable();
    secs.get(secs.len().checked_sub(1)? / 2).copied()
}

impl Flow {
    fn count(&mut self, work: Work, curation: bool) {
        self.done += 1;
        match work {
            Work::Requested => self.requested += 1,
            Work::Induced => self.induced += 1,
            Work::Upkeep => {
                self.upkeep += 1;
                self.self_filed += usize::from(!curation);
            }
        }
    }

    fn close(mut self, secs: Vec<i64>) -> Flow {
        #[allow(clippy::cast_precision_loss)] // counts of tasks
        let share = self.upkeep as f64 / self.done as f64;
        self.upkeep_share = (self.done > 0).then_some(share);
        self.median_claim_to_done_secs = median(secs);
        self
    }
}

impl Memory {
    /// The times of one task: its first claim, its completion, and its landing.
    ///
    /// # Errors
    ///
    /// The ledger's not-found, including for a task above the session's handling.
    pub async fn task_times(&self, id: &str) -> Result<TaskTimes, Error> {
        let l = self.ledger.lock().await;
        let task = l.get(id).await?;
        self.read_task(&task)?;
        Ok(Moments::of(&l, &task.spec.domain).await?.times(&task))
    }

    /// How the work completed in the last `days` days divides into requested work,
    /// induced work and upkeep, and how much induced work is still open.
    ///
    /// # Errors
    ///
    /// An ungranted domain, or a ledger error.
    pub async fn flow(&self, domain: Option<&str>, days: u32) -> Result<Flow, Error> {
        let domain = self.resolve_domain(domain)?;
        let l = self.ledger.lock().await;
        self.flow_in(&l, &domain, days).await
    }

    /// [`Memory::flow`] for a caller that holds the ledger.
    pub(crate) async fn flow_in(&self, l: &Ledger, domain: &str, days: u32) -> Result<Flow, Error> {
        let ms = Moments::of(l, domain).await?;
        let since = l.now() - Duration::days(i64::from(days));
        let owner = self.grants.owner.as_str();
        let mut flow = Flow {
            domain: domain.to_string(),
            window_days: days,
            done: 0,
            requested: 0,
            induced: 0,
            upkeep: 0,
            self_filed: 0,
            upkeep_share: None,
            induced_open: 0,
            median_claim_to_done_secs: None,
        };
        let mut secs = Vec::new();
        for t in self.read_tasks(l.list(domain, Status::Done).await?)? {
            let Some(done) = ms.completed(&t.id).filter(|at| *at >= since) else {
                continue;
            };
            let root = root_of(l, &t).await;
            let work = work_of(&t, &root, ms.worker(&t.id), owner);
            flow.count(work, fabric::maintenance(&root));
            secs.extend(ms.first_claim(&t.id).map(|c| (done - c).whole_seconds()));
        }
        for status in [
            Status::Proposed,
            Status::Open,
            Status::Blocked,
            Status::Claimed,
        ] {
            for t in self.read_tasks(l.list(domain, status).await?)? {
                let root = root_of(l, &t).await;
                let induced = fleet_filed(&root, owner) && !fabric::maintenance(&root);
                flow.induced_open += usize::from(induced);
            }
        }
        Ok(flow.close(secs))
    }

    /// The landing queue `name` of the grants file.
    fn landing_queue(&self, name: &str) -> Result<&LandingQueue, Error> {
        self.grants
            .landing
            .get(name)
            .ok_or_else(|| Error::Denied(format!("the grants name no landing queue {name}")))
    }

    /// Hand the work of a held task to a landing queue instead of completing it. The
    /// task is then blocked with the handoff on it, nobody can claim it, and this
    /// session's claim is over. The landing by a person is what closes it, so it does
    /// not wait for a verdict as well.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for a queue that the grants do not name or that lands another
    /// domain; [`Error::NotFound`] for a missing handoff; a ledger error for a caller
    /// that does not hold the task.
    pub async fn queue_landing(
        &self,
        id: &str,
        generation: Option<u64>,
        queue: &str,
        handoff: &str,
    ) -> Result<Task, Error> {
        let q = self.landing_queue(queue)?;
        let mut l = self.ledger.lock().await;
        let task = l.get(id).await?;
        self.check_task(&task)?;
        if q.domain != task.spec.domain {
            return Err(Error::Denied(format!(
                "the landing queue {queue} lands {}, and {id} is of {}",
                q.domain, task.spec.domain
            )));
        }
        if self.store.get(&task.spec.domain, handoff).await?.is_none() {
            return Err(Error::NotFound(format!("handoff {handoff}")));
        }
        let by = self.under(self.claim_of(id, generation)?);
        let parked = l
            .wait_for(id, by, &landing_event(queue), Some(handoff))
            .await?;
        lock(&self.claims).remove(id);
        self.observe_task(&parked);
        Ok(parked)
    }

    /// A landing queue as it stands: what waits, in landing order, and what landed.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for a queue that the grants do not name or whose domain is not
    /// granted, or a ledger error.
    pub async fn landing(&self, queue: &str) -> Result<LandingView, Error> {
        let l = self.ledger.lock().await;
        self.landing_in(&l, queue).await
    }

    /// The changes that wait in a queue, in landing order: the changes of people first,
    /// then the change queued first, then the task id.
    async fn waiting_in(
        &self,
        l: &Ledger,
        ms: &Moments,
        queue: &str,
    ) -> Result<Vec<Queued>, Error> {
        let q = self.landing_queue(queue)?;
        let event = landing_event(queue);
        let owner = self.grants.owner.as_str();
        let mut waiting = Vec::new();
        for status in [Status::Blocked, Status::Proposed, Status::Open] {
            for t in self.read_tasks(l.list(&q.domain, status).await?)? {
                if t.waiting_on.as_deref() != Some(event.as_str()) {
                    continue;
                }
                let by = ms.worker(&t.id).unwrap_or(&t.spec.created_by).to_string();
                waiting.push(Queued {
                    person: is_person(&by, owner),
                    since: ms.queued_on(&t.id, &event).unwrap_or(t.created),
                    task: t.id,
                    title: t.spec.title,
                    by,
                });
            }
        }
        waiting.sort_by(|a, b| (!a.person, a.since, &a.task).cmp(&(!b.person, b.since, &b.task)));
        Ok(waiting)
    }

    async fn landing_in(&self, l: &Ledger, queue: &str) -> Result<LandingView, Error> {
        let q = self.landing_queue(queue)?;
        let domain = self.resolve_domain(Some(&q.domain))?;
        let ms = Moments::of(l, &domain).await?;
        let event = landing_event(queue);
        let now = l.now();
        let owner = self.grants.owner.as_str();
        let mut view = LandingView {
            queue: queue.to_string(),
            waiting: self.waiting_in(l, &ms, queue).await?,
            per_day: q.per_day,
            fleet_landed_last_day: 0,
            landed_counted: 0,
            landed_not_yet_counted: 0,
            median_claim_to_landed_secs: None,
            domain: domain.clone(),
        };
        let mut secs = Vec::new();
        for t in self.read_tasks(l.list(&domain, Status::Done).await?)? {
            let Some(landed) = ms.landed_on(&t.id, &event) else {
                continue;
            };
            let fleet = !ms.worker(&t.id).is_some_and(|w| is_person(w, owner));
            let last_day = landed > now - Duration::hours(24);
            view.fleet_landed_last_day += usize::from(fleet && last_day);
            let counted = landed + q.count_after() <= now;
            view.landed_counted += usize::from(counted);
            view.landed_not_yet_counted += usize::from(!counted);
            secs.extend(ms.first_claim(&t.id).map(|c| (landed - c).whole_seconds()));
        }
        view.median_claim_to_landed_secs = median(secs);
        Ok(view)
    }

    /// Land the head of a landing queue: the task closes with the handoff that its
    /// holder queued, and what waited on it opens. Only a person in person lands a
    /// change. A change of an agent does not land while the fleet's landings of the last
    /// 24 hours are at the queue's share; no later change lands in its place.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for a session that is no person in person, for an empty queue,
    /// and for the fleet's share; the last is a line of the decision log with the
    /// action `limit`. Otherwise as [`Memory::landing`].
    pub async fn land(&self, queue: &str) -> Result<Task, Error> {
        self.in_person("lands a change")?;
        let mut l = self.ledger.lock().await;
        let view = self.landing_in(&l, queue).await?;
        let Some(head) = view.waiting.first() else {
            return Err(Error::Denied(format!(
                "nothing waits in the landing queue {queue}"
            )));
        };
        let landed = view.fleet_landed_last_day;
        if let Some(share) = view
            .per_day
            .filter(|n| !head.person && landed >= *n as usize)
        {
            return Err(self.refusal(
                "limit",
                &view.domain,
                format!(
                    "the landing queue {queue} lands {share} changes of agents in 24 hours, and {landed} landed: {} waits",
                    head.task
                ),
            ));
        }
        let task = l.get(&head.task).await?;
        let handoff = task.handoff.clone().unwrap_or_default();
        let done = l
            .complete_awaited(&task.id, &landing_event(queue), &handoff)
            .await?;
        l.close_resumed(&done).await;
        self.observe_task(&done);
        Ok(done)
    }
}

#[cfg(test)]
mod tests;
