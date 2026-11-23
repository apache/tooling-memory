use super::*;
use crate::testing::refused_actions;
use crate::WriteRequest;
use dm_core::EntryType;
use dm_ledger::TaskSpec;
use std::sync::{Arc, Mutex};
use time::macros::datetime;

const T0: OffsetDateTime = datetime!(2026-10-01 10:00 UTC);
const OWNER: &str = "human:test:me";

/// A root with the landing queue `main` on `dom-me` and the queue `plat` on `dom-plat`,
/// and a clock that every session's ledger reads.
struct Site {
    dir: tempfile::TempDir,
    clock: Arc<Mutex<OffsetDateTime>>,
}

impl Site {
    fn new(per_day: Option<u32>) -> Site {
        let dir = tempfile::tempdir().unwrap();
        Memory::init(dir.path(), OWNER, "dom-me").unwrap();
        let path = dir.path().join(crate::GRANTS_FILE);
        let mut g = dm_policy::Grants::load(&path).unwrap();
        let d = g.domains["dom-me"].clone();
        g.domains.insert("dom-plat".into(), d);
        for (name, domain, per_day) in [("main", "dom-me", per_day), ("plat", "dom-plat", None)] {
            let q = LandingQueue {
                domain: domain.into(),
                per_day,
                count_after_hours: None,
            };
            g.landing.insert(name.into(), q);
        }
        std::fs::write(&path, g.render().unwrap()).unwrap();
        let clock = Arc::new(Mutex::new(T0));
        Site { dir, clock }
    }

    /// Set the clock to `mins` minutes after `T0`.
    fn at(&self, mins: i64) {
        *self.clock.lock().unwrap() = T0 + Duration::minutes(mins);
    }

    /// A new session of `author` on the site's clock.
    async fn session(&self, author: &str) -> Memory {
        let m = Memory::open(self.dir.path()).unwrap().with_author(author);
        let c = self.clock.clone();
        m.ledger.lock().await.set_clock(move || *c.lock().unwrap());
        m
    }

    async fn handoff(&self) -> String {
        let h = WriteRequest {
            body: "What was done.\n".into(),
            domain: Some("dom-me".into()),
            entry_type: EntryType::Handoff,
            ..WriteRequest::default()
        };
        self.session(OWNER).await.write(h).await.unwrap().id
    }

    /// A task that `by` filed, admitted by the owner when it was born proposed.
    async fn filed(&self, by: &str, spec: TaskSpec) -> Task {
        let t = self.session(by).await.create_task(spec).await.unwrap().task;
        if t.status == Status::Proposed {
            self.session(OWNER).await.admit(&t.id).await.unwrap()
        } else {
            t
        }
    }

    /// `task`, claimed and completed by a new session of `worker`.
    async fn done(&self, task: &Task, worker: &str) {
        let s = self.session(worker).await;
        s.claim(&task.id, Duration::minutes(30)).await.unwrap();
        let h = self.handoff().await;
        s.complete(&task.id, None, &h).await.unwrap();
    }

    /// A task that `worker` filed, claimed and handed to the queue `main`.
    async fn queued(&self, title: &str, worker: &str) -> Task {
        let t = self.filed(OWNER, spec(title)).await;
        let s = self.session(worker).await;
        s.claim(&t.id, Duration::minutes(30)).await.unwrap();
        let h = self.handoff().await;
        s.queue_landing(&t.id, None, "main", &h).await.unwrap()
    }

    async fn flow(&self) -> Flow {
        self.session(OWNER)
            .await
            .flow(Some("dom-me"), 7)
            .await
            .unwrap()
    }

    async fn land(&self) -> Result<Task, Error> {
        self.session(OWNER).await.land("main").await
    }
}

fn spec(title: &str) -> TaskSpec {
    TaskSpec {
        domain: "dom-me".into(),
        title: title.into(),
        body: "Do it.\n".into(),
        ..TaskSpec::default()
    }
}

fn from(title: &str, origin: &str) -> TaskSpec {
    TaskSpec {
        origin: Some(origin.into()),
        ..spec(title)
    }
}

/// A task as the ledger would hold it, for the rule that needs no ledger.
fn task(by: &str, origin: Option<&str>) -> Task {
    Task {
        id: "t".into(),
        spec: TaskSpec {
            created_by: by.into(),
            origin: origin.map(str::to_string),
            ..spec("A task")
        },
        created: T0,
        status: Status::Done,
        holder: None,
        lease_until: None,
        generation: 1,
        handoff: None,
        reason: None,
        superseded_by: None,
        posted_to: None,
        waiting_on: None,
        effects: Vec::new(),
    }
}

fn denied<T: std::fmt::Debug>(r: &Result<T, Error>, saying: &str) -> bool {
    matches!(r, Err(Error::Denied(why)) if why.contains(saying))
}

#[test]
fn the_root_and_the_completer_decide_the_kind_of_work() {
    let kind = |t: &Task, root: &Task, by: &str| work_of(t, root, Some(by), OWNER);
    let asked = task(OWNER, None);
    assert_eq!(kind(&asked, &asked, "agent:a"), Work::Requested);
    let tapped = task("tap:issues", None);
    assert_eq!(kind(&tapped, &tapped, "agent:a"), Work::Requested);
    let own = task("agent:a", None);
    assert_eq!(kind(&own, &own, "agent:a"), Work::Upkeep);
    assert_eq!(kind(&own, &own, "agent:b"), Work::Induced);
    assert_eq!(work_of(&own, &own, None, OWNER), Work::Induced);
    let system = task("launcher", None);
    assert_eq!(kind(&system, &system, "agent:a"), Work::Induced);
    for origin in ["curator:stale", "auditor:1", "telemetry:reads"] {
        let curation = task(OWNER, Some(origin));
        assert_eq!(kind(&curation, &curation, "agent:a"), Work::Upkeep);
    }
    // the root decides: a part that an agent filed and completed, of a person's task
    assert_eq!(kind(&own, &asked, "agent:a"), Work::Requested);
    assert_eq!(
        kind(&own, &task(OWNER, Some("curator:x")), "agent:b"),
        Work::Upkeep
    );
}

#[tokio::test]
async fn completed_work_divides_into_requested_induced_and_upkeep() {
    let site = Site::new(None);
    let none = site.flow().await;
    assert_eq!((none.done, none.upkeep_share), (0, None));
    site.done(&site.filed(OWNER, spec("Asked")).await, "agent:a")
        .await;
    site.done(&site.filed("agent:a", spec("Own")).await, "agent:a")
        .await;
    site.done(&site.filed("agent:a", spec("For b")).await, "agent:b")
        .await;
    let survey = site.filed(OWNER, from("Survey", "curator:stale")).await;
    site.done(&survey, "agent:a").await;
    site.filed("agent:b", spec("Not done yet")).await;
    let flow = site.flow().await;
    assert_eq!(
        (flow.done, flow.requested, flow.induced, flow.upkeep),
        (4, 1, 1, 2)
    );
    assert_eq!(
        flow.self_filed, 1,
        "curation is upkeep that nobody self-filed"
    );
    assert_eq!(flow.upkeep_share, Some(0.5));
    assert_eq!(flow.induced_open, 1);
}

#[tokio::test]
async fn a_part_of_a_person_s_task_is_requested_work_whoever_files_it() {
    let site = Site::new(None);
    let whole = site.filed(OWNER, spec("The whole")).await;
    let agent = site.session("agent:a").await;
    agent.claim(&whole.id, Duration::minutes(30)).await.unwrap();
    agent.set_held_task(&whole.id);
    let part = agent.create_task(spec("A part")).await.unwrap().task;
    assert_eq!(part.spec.parent.as_deref(), Some(whole.id.as_str()));
    assert_eq!(part.spec.created_by, "agent:a");
    let open = site.flow().await;
    assert_eq!(open.induced_open, 0, "the root is a person's");
    if part.status == Status::Proposed {
        site.session(OWNER).await.admit(&part.id).await.unwrap();
    }
    site.done(&part, "agent:a").await;
    let flow = site.flow().await;
    assert_eq!((flow.done, flow.requested, flow.upkeep), (1, 1, 0));
    // and what continues an agent's own task has that task as its root
    let own = site.filed("agent:a", spec("Own")).await;
    let next = site
        .filed("agent:b", from("Go on", &format!("task:{}", own.id)))
        .await;
    site.done(&next, "agent:b").await;
    let flow = site.flow().await;
    assert_eq!((flow.done, flow.requested, flow.upkeep), (2, 1, 1));
}

#[tokio::test]
async fn the_window_ends_at_the_clock_of_the_ledger() {
    let site = Site::new(None);
    site.done(&site.filed(OWNER, spec("Early")).await, "agent:a")
        .await;
    assert_eq!(site.flow().await.done, 1);
    site.at(8 * 24 * 60);
    assert_eq!(site.flow().await.done, 0, "completed before the window");
    site.done(&site.filed(OWNER, spec("Late")).await, "agent:a")
        .await;
    let flow = site.flow().await;
    assert_eq!((flow.done, flow.requested), (1, 1));
    assert_eq!(flow.median_claim_to_done_secs, Some(0));
}

#[tokio::test]
async fn the_times_of_a_task_start_at_its_first_claim() {
    let site = Site::new(None);
    let t = site.filed(OWNER, spec("Claimed twice")).await;
    site.at(1);
    let first = site.session("agent:a").await;
    first.claim(&t.id, Duration::minutes(30)).await.unwrap();
    let open = first.task_times(&t.id).await.unwrap();
    assert_eq!(
        (open.claimed, open.completed),
        (Some(T0 + Duration::minutes(1)), None)
    );
    assert_eq!(open.claim_to_done_secs, None);
    site.at(2);
    first.release(&t.id, None, None).await.unwrap();
    site.at(5);
    let second = site.session("agent:b").await;
    second.claim(&t.id, Duration::minutes(30)).await.unwrap();
    site.at(10);
    let h = site.handoff().await;
    second.complete(&t.id, None, &h).await.unwrap();
    let times = second.task_times(&t.id).await.unwrap();
    assert_eq!(times.claimed, Some(T0 + Duration::minutes(1)));
    assert_eq!(times.completed, Some(T0 + Duration::minutes(10)));
    assert_eq!(times.claim_to_done_secs, Some(9 * 60));
    assert_eq!(
        (times.queued, times.claim_to_landed_secs, times.queue),
        (None, None, None)
    );
}

#[tokio::test]
async fn only_the_holder_queues_a_task_and_only_in_a_queue_of_its_domain() {
    let site = Site::new(None);
    let t = site.filed(OWNER, spec("A change")).await;
    let holder = site.session("agent:a").await;
    holder.claim(&t.id, Duration::minutes(30)).await.unwrap();
    let h = site.handoff().await;
    let unknown = holder.queue_landing(&t.id, None, "nope", &h).await;
    assert!(denied(&unknown, "no landing queue nope"), "{unknown:?}");
    let elsewhere = holder.queue_landing(&t.id, None, "plat", &h).await;
    assert!(denied(&elsewhere, "lands dom-plat"), "{elsewhere:?}");
    let missing = holder.queue_landing(&t.id, None, "main", "mem-none").await;
    assert!(matches!(missing, Err(Error::NotFound(_))), "{missing:?}");
    let other = site.session("agent:b").await;
    for generation in [None, Some(1)] {
        let stolen = other.queue_landing(&t.id, generation, "main", &h).await;
        assert!(
            matches!(
                stolen,
                Err(Error::Ledger(dm_ledger::Error::NotHolder { .. }))
            ),
            "{stolen:?}"
        );
    }
    assert_eq!(
        holder.get_task(&t.id).await.unwrap().status,
        Status::Claimed
    );
    let parked = holder.queue_landing(&t.id, None, "main", &h).await.unwrap();
    assert_eq!(parked.status, Status::Blocked);
    assert_eq!(parked.waiting_on.as_deref(), Some("landing:main"));
    assert_eq!(parked.handoff.as_deref(), Some(h.as_str()));
    site.at(120);
    assert!(other.claim(&t.id, Duration::minutes(30)).await.is_err());
    let next = other
        .claim_next(Some("dom-me"), Duration::minutes(30))
        .await
        .unwrap();
    assert!(next.is_none(), "nobody claims a queued task");
}

#[tokio::test]
async fn only_a_person_in_person_lands_a_change() {
    let site = Site::new(None);
    let t = site.queued("A change", "agent:a").await;
    let agent = site.session("agent:b").await;
    let served = site.session(OWNER).await;
    served.mark_served();
    for session in [&agent, &served] {
        let refused = session.land("main").await;
        assert!(
            denied(&refused, "only a person lands a change"),
            "{refused:?}"
        );
    }
    let still = served.get_task(&t.id).await.unwrap();
    assert_eq!(still.status, Status::Blocked);
    assert_eq!(
        refused_actions(site.dir.path(), "agent:b"),
        [("person_only".to_string(), "agent".to_string())]
    );
    let nothing = site.session(OWNER).await.land("plat").await;
    assert!(denied(&nothing, "nothing waits"), "{nothing:?}");
}

#[tokio::test]
async fn a_person_s_change_lands_first_and_then_the_change_queued_first() {
    let site = Site::new(None);
    site.at(1);
    let a = site.queued("By agent a", "agent:a").await;
    site.at(2);
    let b = site.queued("By agent b", "agent:b").await;
    site.at(3);
    let p = site.queued("By a person", "human:test:you").await;
    let view = site.session(OWNER).await.landing("main").await.unwrap();
    let order: Vec<(&str, bool)> = view
        .waiting
        .iter()
        .map(|q| (q.task.as_str(), q.person))
        .collect();
    assert_eq!(
        order,
        [
            (p.id.as_str(), true),
            (a.id.as_str(), false),
            (b.id.as_str(), false)
        ]
    );
    assert_eq!(view.waiting[1].by, "agent:a");
    assert_eq!(view.waiting[1].since, T0 + Duration::minutes(1));
    for expected in [&p, &a, &b] {
        assert_eq!(site.land().await.unwrap().id, expected.id);
    }
    assert!(denied(&site.land().await, "nothing waits"));
}

#[tokio::test]
async fn the_fleet_lands_its_share_in_a_day_and_people_are_not_counted() {
    let site = Site::new(Some(1));
    let first = site.queued("First of the fleet", "agent:a").await;
    site.at(1);
    let second = site.queued("Second of the fleet", "agent:b").await;
    assert_eq!(site.land().await.unwrap().id, first.id);
    let refused = site.land().await;
    assert!(
        denied(
            &refused,
            "lands 1 changes of agents in 24 hours, and 1 landed"
        ),
        "{refused:?}"
    );
    assert_eq!(
        refused_actions(site.dir.path(), OWNER),
        [("limit".to_string(), "owner".to_string())]
    );
    let owner = site.session(OWNER).await;
    assert_eq!(
        owner.get_task(&second.id).await.unwrap().status,
        Status::Blocked
    );
    assert_eq!(
        owner.landing("main").await.unwrap().fleet_landed_last_day,
        1
    );
    let person = site.queued("By a person", OWNER).await;
    assert_eq!(site.land().await.unwrap().id, person.id);
    let after = owner.landing("main").await.unwrap();
    assert_eq!(
        after.fleet_landed_last_day, 1,
        "a person's landing is not counted"
    );
    assert!(
        site.land().await.is_err(),
        "the person's landing is not the fleet's"
    );
    site.at(24 * 60 + 1);
    assert_eq!(site.land().await.unwrap().id, second.id);
}

#[tokio::test]
async fn a_landed_task_is_done_and_opens_what_waited_on_it() {
    let site = Site::new(None);
    site.at(1);
    let t = site.queued("A change", "agent:a").await;
    let mut after = spec("After the change");
    after.depends_on = vec![t.id.clone()];
    let dependent = site.filed(OWNER, after).await;
    assert_eq!(dependent.status, Status::Blocked);
    let owner = site.session(OWNER).await;
    let waiting = owner.task_times(&t.id).await.unwrap();
    assert_eq!(waiting.queued, Some(T0 + Duration::minutes(1)));
    assert_eq!(
        (waiting.completed, waiting.claim_to_landed_secs),
        (None, None)
    );
    site.at(31);
    let done = site.land().await.unwrap();
    assert_eq!(done.status, Status::Done);
    assert_eq!(done.handoff, t.handoff);
    assert_eq!(
        owner.get_task(&dependent.id).await.unwrap().status,
        Status::Open
    );
    let times = owner.task_times(&t.id).await.unwrap();
    assert_eq!(times.claim_to_landed_secs, Some(30 * 60));
    assert_eq!(times.claim_to_done_secs, Some(30 * 60));
    assert_eq!(times.queue.as_deref(), Some("main"));
    let view = owner.landing("main").await.unwrap();
    assert_eq!(view.median_claim_to_landed_secs, Some(30 * 60));
}

#[tokio::test]
async fn a_landing_counts_as_progress_after_the_delay_of_the_queue() {
    let site = Site::new(None);
    site.queued("A change", "agent:a").await;
    site.land().await.unwrap();
    // a task completed without the queue is no landing
    site.done(&site.filed(OWNER, spec("Direct")).await, "agent:a")
        .await;
    let owner = site.session(OWNER).await;
    site.at(24 * 60 - 1);
    let new = owner.landing("main").await.unwrap();
    assert_eq!((new.landed_counted, new.landed_not_yet_counted), (0, 1));
    site.at(24 * 60);
    let old = owner.landing("main").await.unwrap();
    assert_eq!((old.landed_counted, old.landed_not_yet_counted), (1, 0));
    assert_eq!(old.fleet_landed_last_day, 0);
}
