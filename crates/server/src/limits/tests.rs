use super::*;
use crate::testing::refused_actions;
use crate::WriteRequest;
use dm_core::{EntryType, Label};
use dm_ledger::{Status, TaskSpec};
use dm_policy::{Grants, LandingQueue};
use std::sync::{Arc, Mutex};
use time::macros::datetime;
use time::OffsetDateTime;

const T0: OffsetDateTime = datetime!(2026-10-01 10:00 UTC);
const OWNER: &str = "human:test:me";
const DOMAIN: &str = "dom-me";
const LEASE: Duration = Duration::minutes(30);

/// A root whose domain `dom-me` admits its own filings, with the landing queue `main`,
/// and a clock that every session's ledger reads.
struct Site {
    dir: tempfile::TempDir,
    clock: Arc<Mutex<OffsetDateTime>>,
}

impl Site {
    fn new(shares: DomainLimits) -> Site {
        let dir = tempfile::tempdir().unwrap();
        Memory::init(dir.path(), OWNER, DOMAIN).unwrap();
        let site = Site {
            dir,
            clock: Arc::new(Mutex::new(T0)),
        };
        site.grants(|g| {
            let q = LandingQueue {
                domain: DOMAIN.into(),
                per_day: None,
                count_after_hours: None,
            };
            g.landing.insert("main".into(), q);
            g.limits.insert(DOMAIN.into(), shares);
        });
        site
    }

    /// Change the grants file. A session opened after this reads the new file.
    fn grants(&self, change: impl FnOnce(&mut Grants)) {
        let path = self.dir.path().join(crate::GRANTS_FILE);
        let mut g = Grants::load(&path).unwrap();
        change(&mut g);
        std::fs::write(&path, g.render().unwrap()).unwrap();
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

    async fn owner(&self) -> Memory {
        self.session(OWNER).await
    }

    /// A new session of `author` that reads nothing above `internal`.
    async fn capped(&self, author: &str) -> Memory {
        let m = Memory::open(self.dir.path()).unwrap().with_author(author);
        let m = m.with_handling_cap(Label::Internal);
        let c = self.clock.clone();
        m.ledger.lock().await.set_clock(move || *c.lock().unwrap());
        m
    }

    /// A `restricted` task as the ledger takes it from `by`, admitted.
    async fn restricted(&self, by: &str, spec: TaskSpec) -> Task {
        let owner = self.owner().await;
        let mut l = owner.ledger.lock().await;
        let spec = TaskSpec {
            classification: Label::Restricted,
            created_by: by.into(),
            ..spec
        };
        let t = l.create(spec).await.unwrap().task;
        l.admit(&t.id).await.unwrap()
    }

    async fn handoff(&self) -> String {
        let h = WriteRequest {
            body: "What was done.\n".into(),
            domain: Some(DOMAIN.into()),
            entry_type: EntryType::Handoff,
            ..WriteRequest::default()
        };
        self.owner().await.write(h).await.unwrap().id
    }

    /// A task that the owner filed and admitted.
    async fn asked(&self, spec: TaskSpec) -> Task {
        let owner = self.owner().await;
        let t = owner.create_task(spec).await.unwrap().task;
        owner.admit(&t.id).await.unwrap()
    }

    /// The ids of `n` tasks that the owner filed and admitted.
    async fn open(&self, n: usize) -> Vec<String> {
        let mut ids = Vec::new();
        for i in 0..n {
            ids.push(self.asked(spec(&format!("Task {i}"))).await.id);
        }
        ids
    }

    /// `task`, claimed and completed by a new session of `worker`.
    async fn done(&self, task: &Task, worker: &str) {
        let s = self.session(worker).await;
        s.claim(&task.id, LEASE).await.unwrap();
        let h = self.handoff().await;
        s.complete(&task.id, None, &h).await.unwrap();
    }

    /// Whether a new session of `who` claims `task`.
    async fn claims(&self, who: &str, task: &str) -> Result<(), Error> {
        self.session(who).await.claim(task, LEASE).await.map(|_| ())
    }

    /// A task that a new session of `who` files as the system does: admitted as filed
    /// where the domain admits its own.
    async fn own(&self, who: &str, spec: TaskSpec) -> Task {
        self.session(who).await.file_own(spec).await.unwrap().task
    }

    /// One requested task and one task that `agent:a` filed and completed: half of the
    /// completed work is upkeep. The second task was admitted as filed.
    async fn half_upkeep(&self) {
        self.done(&self.asked(spec("Asked")).await, "agent:b").await;
        let own = self.own("agent:a", spec("Own")).await;
        assert_eq!(
            own.status,
            Status::Open,
            "under the share, admitted as filed"
        );
        self.done(&own, "agent:a").await;
    }

    fn limit_lines(&self, principal: &str) -> usize {
        let log = self.dir.path().join("state/decisions.jsonl");
        let log = std::fs::read_to_string(log).unwrap_or_default();
        let lines = log.lines().filter(|line| {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            v["action"] == LIMIT_ACTION && v["allow"] == false && v["principal"] == principal
        });
        lines.count()
    }
}

fn spec(title: &str) -> TaskSpec {
    TaskSpec {
        domain: DOMAIN.into(),
        title: title.into(),
        body: "Do it.\n".into(),
        ..TaskSpec::default()
    }
}

fn from(title: &str, origin: &str) -> TaskSpec {
    TaskSpec {
        origin: Some(origin.into()),
        upstream_event: Some(format!("event of {title}")),
        ..spec(title)
    }
}

fn per_hour(n: u32) -> DomainLimits {
    DomainLimits {
        claims_per_hour: Some(n),
        ..DomainLimits::default()
    }
}

fn upkeep(percent: u8) -> DomainLimits {
    DomainLimits {
        upkeep_percent: Some(percent),
        ..DomainLimits::default()
    }
}

fn denied<T: std::fmt::Debug>(r: &Result<T, Error>, saying: &str) -> bool {
    matches!(r, Err(Error::Denied(why)) if why.contains(saying))
}

fn limited(dir: &std::path::Path, principal: &str) -> bool {
    refused_actions(dir, principal)
        .iter()
        .any(|(action, _)| action == LIMIT_ACTION)
}

#[test]
fn a_limit_has_a_name_and_the_target_of_its_approval() {
    for (word, limit) in [
        ("throughput", Limit::Throughput),
        ("upkeep", Limit::Upkeep),
        ("outbound:github", Limit::Outbound("github".into())),
        ("landing:main", Limit::Landing("main".into())),
    ] {
        assert_eq!(word.parse::<Limit>().unwrap(), limit);
        assert_eq!(limit.target(), format!("limit:{word}"));
    }
    for word in ["spend", "outbound", "outbound:", "landing:", "upkeep:x", ""] {
        assert!(denied(&word.parse::<Limit>(), "is no limit"), "{word}");
    }
}

#[tokio::test]
async fn an_agent_at_its_share_of_throughput_claims_no_more_until_the_hour_passes() {
    let site = Site::new(per_hour(2));
    let t = site.open(5).await;
    site.claims("agent:a", &t[0]).await.unwrap();
    site.at(30);
    site.claims("agent:a", &t[1]).await.unwrap();
    assert!(!limited(site.dir.path(), "agent:a"));
    site.at(45);
    let third = site.claims("agent:a", &t[2]).await;
    assert!(
        denied(
            &third,
            "agent:a made 2 claims in dom-me in the last hour; its share of throughput is 2"
        ),
        "{third:?}"
    );
    assert_eq!(
        refused_actions(site.dir.path(), "agent:a"),
        [("limit".to_string(), "agent".to_string())]
    );
    let next = site.session("agent:a").await.claim_next(None, LEASE).await;
    assert!(denied(&next, "share of throughput is 2"), "{next:?}");
    // the first claim is 61 minutes old: one claim is left in the hour
    site.at(61);
    site.claims("agent:a", &t[2]).await.unwrap();
    let fourth = site.claims("agent:a", &t[3]).await;
    assert!(denied(&fourth, "made 2 claims"), "{fourth:?}");
    // with no share in the grants, nothing is refused
    site.grants(|g| g.limits.clear());
    site.claims("agent:a", &t[3]).await.unwrap();
}

#[tokio::test]
async fn the_claims_of_others_do_not_count_and_a_person_is_not_limited() {
    let site = Site::new(per_hour(1));
    let t = site.open(5).await;
    site.claims("agent:b", &t[0]).await.unwrap();
    site.claims(OWNER, &t[1]).await.unwrap();
    site.claims(OWNER, &t[2]).await.unwrap();
    site.claims("human:test:you", &t[3]).await.unwrap();
    let again = site.session("human:test:you").await;
    assert!(again.claim_next(None, LEASE).await.unwrap().is_some());
    assert!(refused_actions(site.dir.path(), OWNER).is_empty());
    assert_eq!(site.limit_lines("human:test:you"), 0);
    let other = site.open(2).await;
    site.claims("agent:a", &other[0]).await.unwrap();
    let second = site.claims("agent:a", &other[1]).await;
    assert!(denied(&second, "agent:a made 1 claims"), "{second:?}");
}

#[tokio::test]
async fn only_a_person_in_person_raises_a_limit_that_the_grants_set() {
    let site = Site::new(per_hour(1));
    let day = Duration::hours(24);
    let agent = site.session("agent:a").await;
    let r = agent
        .raise_limit(None, &Limit::Throughput, 9, Some("agent:a"), day)
        .await;
    assert!(denied(&r, "only a person raises a limit"), "{r:?}");
    let served = site.owner().await;
    served.mark_served();
    let r = served
        .raise_limit(None, &Limit::Throughput, 9, Some("agent:a"), day)
        .await;
    assert!(denied(&r, "only a person raises a limit"), "{r:?}");
    let owner = site.owner().await;
    for limit in [
        Limit::Upkeep,
        Limit::Outbound("github".into()),
        Limit::Landing("main".into()),
        Limit::Landing("other".into()),
    ] {
        let r = owner.raise_limit(Some(DOMAIN), &limit, 9, None, day).await;
        assert!(denied(&r, "there is nothing to raise"), "{limit}: {r:?}");
    }
    let r = owner
        .raise_limit(None, &Limit::Throughput, 9, None, day)
        .await;
    assert!(denied(&r, "name the principal"), "{r:?}");
    let l = owner.ledger.lock().await;
    assert!(l.approvals(DOMAIN, None).await.unwrap().is_empty());
    drop(l);
    let raised = owner
        .raise_limit(None, &Limit::Throughput, 9, Some("agent:a"), day)
        .await
        .unwrap();
    assert_eq!(raised.spec.principal, OWNER);
    assert_eq!(raised.spec.action, "raise:9");
    assert_eq!(raised.spec.target, "limit:throughput");
    assert_eq!(raised.spec.version, dm_core::digest("limit:throughput=1"));
    assert_eq!(raised.spec.scope, "agent:a");
    assert_eq!(raised.spec.expires, T0 + day);
    // an approval of a limit is no approval of an entry: the list of those is empty
    assert!(owner.approvals(None, None).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_raise_holds_for_its_principal_until_it_expires_or_the_grants_change() {
    let site = Site::new(per_hour(1));
    let t = site.open(10).await;
    let owner = site.owner().await;
    let raise = |to, life| owner.raise_limit(None, &Limit::Throughput, to, Some("agent:a"), life);
    raise(3, Duration::minutes(120)).await.unwrap();
    for id in &t[0..3] {
        site.claims("agent:a", id).await.unwrap();
    }
    let fourth = site.claims("agent:a", &t[3]).await;
    assert!(denied(&fourth, "share of throughput is 3"), "{fourth:?}");
    site.claims("agent:b", &t[3]).await.unwrap();
    let other = site.claims("agent:b", &t[4]).await;
    assert!(denied(&other, "share of throughput is 1"), "{other:?}");
    // one minute before the expiry the raise holds, and at the expiry it does not
    site.at(119);
    site.claims("agent:a", &t[4]).await.unwrap();
    site.claims("agent:a", &t[5]).await.unwrap();
    site.at(120);
    let expired = site.claims("agent:a", &t[6]).await;
    assert!(denied(&expired, "share of throughput is 1"), "{expired:?}");
    // a raise is of the value that the grants hold: a new value voids it
    raise(8, Duration::hours(24)).await.unwrap();
    site.claims("agent:a", &t[6]).await.unwrap();
    site.grants(|g| {
        g.limits.insert(DOMAIN.into(), per_hour(2));
    });
    let voided = site.claims("agent:a", &t[7]).await;
    assert!(denied(&voided, "share of throughput is 2"), "{voided:?}");
}

#[tokio::test]
async fn a_raise_that_no_person_gave_and_a_raise_to_a_lower_value_do_nothing() {
    let site = Site::new(per_hour(2));
    let t = site.open(4).await;
    let owner = site.owner().await;
    owner
        .raise_limit(
            None,
            &Limit::Throughput,
            1,
            Some("agent:a"),
            Duration::hours(24),
        )
        .await
        .unwrap();
    // the ledger takes a record from whoever holds it; the limit takes it from a person
    let forged = ApprovalSpec {
        domain: DOMAIN.into(),
        principal: "agent:a".into(),
        action: "raise:9".into(),
        target: Limit::Throughput.target(),
        version: version_of("limit:throughput", 2),
        scope: "agent:a".into(),
        expires: T0 + Duration::hours(24),
    };
    let mut l = owner.ledger.lock().await;
    l.record_approval(&forged).await.unwrap();
    drop(l);
    site.claims("agent:a", &t[0]).await.unwrap();
    site.claims("agent:a", &t[1]).await.unwrap();
    let third = site.claims("agent:a", &t[2]).await;
    assert!(denied(&third, "share of throughput is 2"), "{third:?}");
    let rows = site.session("agent:a").await.limits(None).await.unwrap();
    let row = rows.iter().find(|r| r.limit == "throughput").unwrap();
    assert_eq!((row.cap, row.configured), (Some(2.0), Some(2.0)));
    assert_eq!(row.raised_by, None);
}

#[tokio::test]
async fn a_tap_is_handed_no_more_than_the_outbound_rate_leaves_in_a_day() {
    let site = Site::new(DomainLimits {
        outbound: BTreeMap::from([("github".to_string(), 2)]),
        ..DomainLimits::default()
    });
    let mut github = Vec::new();
    for i in 0..3 {
        site.at(i);
        let t = site.asked(from(&format!("G{i}"), "github:o/r#1")).await;
        site.done(&t, "agent:a").await;
        github.push(t.id);
    }
    for i in 0..3 {
        let t = site.asked(from(&format!("I{i}"), "issues:o/r#2")).await;
        site.done(&t, "agent:a").await;
    }
    let tap = site.session("tap:github").await;
    let handed = |out: Vec<(Task, dm_core::Entry)>, family: &str| -> Vec<String> {
        let of = out.into_iter().filter(|(t, _)| fabric::family(t) == family);
        of.map(|(t, _)| t.id).collect()
    };
    assert_eq!(site.limit_lines("tap:github"), 0);
    let out = tap.unposted(DOMAIN, Label::Restricted).await.unwrap();
    assert_eq!(
        handed(out.clone(), "issues").len(),
        3,
        "a family with no rate"
    );
    assert_eq!(handed(out, "github"), github[0..2], "the oldest two");
    assert_eq!(
        site.limit_lines("tap:github"),
        1,
        "one waits, and the log says so"
    );
    // a post is recorded whatever the rate: it happened
    for id in &github[0..2] {
        tap.post(id, "https://example.test/1", DOMAIN, Label::Restricted)
            .await
            .unwrap();
    }
    let out = tap.unposted(DOMAIN, Label::Restricted).await.unwrap();
    assert!(handed(out, "github").is_empty(), "two posted in the day");
    site.at(24 * 60);
    let out = tap.unposted(DOMAIN, Label::Restricted).await.unwrap();
    assert!(
        handed(out, "github").is_empty(),
        "the posts are 24 hours old less two minutes"
    );
    site.at(24 * 60 + 2);
    let out = tap.unposted(DOMAIN, Label::Restricted).await.unwrap();
    assert_eq!(handed(out, "github"), github[2..3]);
}

#[tokio::test]
async fn a_raise_of_the_outbound_rate_hands_out_more() {
    let site = Site::new(DomainLimits {
        outbound: BTreeMap::from([("github".to_string(), 1)]),
        ..DomainLimits::default()
    });
    for i in 0..3 {
        let t = site.asked(from(&format!("G{i}"), "github:o/r#1")).await;
        site.done(&t, "agent:a").await;
    }
    let tap = site.session("tap:github").await;
    let out = tap.unposted(DOMAIN, Label::Restricted).await.unwrap();
    assert_eq!(out.len(), 1);
    let limit = Limit::Outbound("github".into());
    let owner = site.owner().await;
    owner
        .raise_limit(None, &limit, 2, None, Duration::hours(1))
        .await
        .unwrap();
    let out = tap.unposted(DOMAIN, Label::Restricted).await.unwrap();
    assert_eq!(out.len(), 2);
}

#[tokio::test]
async fn work_that_the_fleet_filed_stays_proposed_at_the_share_of_upkeep() {
    let site = Site::new(upkeep(50));
    // with nothing completed there is no share, and under the share nothing is held
    let first = site.own("agent:a", spec("Before any work")).await;
    assert_eq!(first.status, Status::Open);
    site.half_upkeep().await;
    assert!(!limited(site.dir.path(), "agent:a"));
    let held = site.own("agent:a", spec("More of its own")).await;
    assert_eq!(held.status, Status::Proposed);
    assert_eq!(
        refused_actions(site.dir.path(), "agent:a"),
        [("limit".to_string(), "agent".to_string())]
    );
    let log = std::fs::read_to_string(site.dir.path().join("state/decisions.jsonl")).unwrap();
    let said = format!(
        "{} is not admitted: the upkeep among the tasks completed in dom-me in 7 days is at the share, and the share of upkeep is 50 percent. A person admits it",
        held.id
    );
    assert!(log.contains(&said), "{log}");
    // a part of an agent's own task is the fleet's work too
    let root = site.session(OWNER).await.admit(&held.id).await.unwrap();
    let agent = site.session("agent:a").await;
    agent.claim(&root.id, LEASE).await.unwrap();
    agent.set_held_task(&root.id);
    let part = agent.create_task(spec("A part of its own")).await.unwrap();
    assert_eq!(part.task.status, Status::Proposed);
    // the window passes: nothing is completed in it, so nothing is held back
    site.at(8 * 24 * 60);
    let later = site.own("agent:a", spec("After the window")).await;
    assert_eq!(later.status, Status::Open);
}

#[tokio::test]
async fn work_that_a_person_asked_for_is_admitted_at_the_share_of_upkeep() {
    let site = Site::new(upkeep(50));
    site.half_upkeep().await;
    let owner = site.owner().await;
    let asked = owner.create_task(spec("Asked at the share"));
    let asked = asked.await.unwrap().task;
    assert_eq!(asked.status, Status::Proposed, "the owner admits it");
    let agent = site.session("agent:a").await;
    assert_eq!(agent.admit(&asked.id).await.unwrap().status, Status::Open);
    agent.claim(&asked.id, LEASE).await.unwrap();
    agent.set_held_task(&asked.id);
    let part = agent.create_task(spec("A part")).await.unwrap().task;
    assert_eq!(part.spec.created_by, "agent:a");
    assert_eq!(part.status, Status::Open, "its root is a person's task");
    assert!(!limited(site.dir.path(), "agent:a"));
}

/// The root of a task decides whether the share of upkeep holds it back. An agent that
/// could name a task in the origin of its filing would choose that root: a task of a
/// person passes the share, and a hidden task answers whether it is there.
#[tokio::test]
async fn an_agent_names_no_task_in_the_origin_of_its_filing() {
    let site = Site::new(upkeep(50));
    site.half_upkeep().await;
    let asked = site.asked(spec("Asked by a person")).await;
    let secret = site.restricted(OWNER, spec("Secret")).await;
    let named = "names the task that a task is filed from";
    for who in ["agent:a", "tap:github", "curator"] {
        for prefix in ["task:", "continues:", "retries:"] {
            for id in [asked.id.as_str(), secret.id.as_str(), "task-none"] {
                let forged = from("Mine", &format!("{prefix}{id}"));
                let r = site.session(who).await.create_task(forged).await;
                assert!(denied(&r, named), "{who} {prefix}{id}: {r:?}");
            }
        }
    }
    let owner = site.owner().await;
    let proposed = owner.tasks(Some(DOMAIN), Status::Proposed).await.unwrap();
    assert!(proposed.is_empty(), "nothing was filed: {proposed:?}");
    // without the name, the filing of the agent is its own and the share holds it back
    let plain = site.session("agent:a").await;
    let plain = plain.create_task(spec("Mine")).await.unwrap().task;
    let admit = site.session("agent:a").await.admit(&plain.id).await;
    assert!(
        denied(&admit, "the share of upkeep is 50 percent"),
        "{admit:?}"
    );
    // a person names a task that the session reads; a task above its handling and a
    // task that is not there get the same answer
    let person = site.capped("human:test:you").await;
    for id in [secret.id.as_str(), "task-none"] {
        let r = person
            .create_task(from("Go on", &format!("task:{id}")))
            .await;
        let missing = matches!(&r, Err(Error::Ledger(dm_ledger::Error::NotFound(_))));
        assert!(missing, "{id}: {r:?}");
    }
    let next = from("Go on", &format!("task:{}", asked.id));
    let next = person.create_task(next).await.unwrap().task;
    assert_eq!(next.spec.origin, Some(format!("task:{}", asked.id)));
}

/// The share of upkeep is the domain's: the tasks above the handling of the filer count
/// too. A session that reads less would otherwise pass the share.
#[tokio::test]
async fn the_share_of_upkeep_counts_the_tasks_above_the_handling_of_the_filer() {
    let site = Site::new(upkeep(50));
    site.done(&site.asked(spec("Asked")).await, "agent:b").await;
    let own = site.restricted("agent:b", spec("Own, restricted")).await;
    site.done(&own, "agent:b").await;
    let low = site.capped("agent:a").await;
    let seen = low.flow(Some(DOMAIN), 7).await.unwrap();
    assert_eq!((seen.done, seen.upkeep), (1, 0), "the session reads one");
    let held = low.file_own(spec("More of the fleet's own")).await.unwrap();
    assert_eq!(held.task.status, Status::Proposed, "1 of 2 is upkeep");
    assert!(limited(site.dir.path(), "agent:a"));
}

/// A limit is held against every task of the domain. The report gives a session the
/// counts of the tasks that it may read, so no row moves for a task above its handling.
#[tokio::test]
async fn the_report_counts_no_task_above_the_handling_of_the_session() {
    let site = Site::new(DomainLimits {
        upkeep_percent: Some(90),
        outbound: BTreeMap::from([("github".to_string(), 4)]),
        ..DomainLimits::default()
    });
    site.half_upkeep().await;
    let counts = async |m: Memory| {
        let rows = m.limits(None).await.unwrap();
        let now = |limit: &str| rows.iter().find(|r| r.limit == limit).unwrap().now;
        (now("upkeep"), now("outbound:github"))
    };
    let before = counts(site.capped("agent:a").await).await;
    assert_eq!(before, (Some(50.0), Some(0.0)));
    assert_eq!(counts(site.owner().await).await, before);
    for i in 0..3 {
        let own = site.restricted("agent:b", spec(&format!("Own {i}"))).await;
        site.done(&own, "agent:b").await;
    }
    let posted = site.restricted(OWNER, from("G", "github:o/r#1")).await;
    site.done(&posted, "agent:b").await;
    let owner = site.owner().await;
    let url = "https://example.test/1";
    owner
        .post(&posted.id, url, DOMAIN, Label::Restricted)
        .await
        .unwrap();
    assert_eq!(counts(site.capped("agent:a").await).await, before);
    // 4 of the 6 completed tasks are upkeep, and one handoff was posted
    let all = counts(owner).await;
    assert!((all.0.unwrap() - 400.0 / 6.0).abs() < 1e-9, "{all:?}");
    assert_eq!(all.1, Some(1.0));
}

#[tokio::test]
async fn curation_is_admitted_at_the_share_of_upkeep_while_it_is_under_its_floor() {
    let site = Site::new(upkeep(50));
    site.half_upkeep().await;
    // no curation among the two completed tasks: under the floor of 0.2
    let survey = site.own("curator", from("Survey", "curator:stale")).await;
    assert_eq!(survey.status, Status::Open);
    site.done(&survey, "agent:b").await;
    // one of three is curation: at the floor, and the share of upkeep holds it back
    let again = site
        .own("curator", from("Survey again", "curator:old"))
        .await;
    assert_eq!(again.status, Status::Proposed);
    assert_eq!(site.limit_lines("curator"), 1);
}

#[tokio::test]
async fn a_person_in_person_admits_what_the_share_of_upkeep_holds_back() {
    let site = Site::new(upkeep(50));
    site.half_upkeep().await;
    let held = site.own("agent:a", spec("More of its own")).await;
    assert_eq!(held.status, Status::Proposed);
    let by_agent = site.session("agent:b").await.admit(&held.id).await;
    assert!(
        denied(&by_agent, "the share of upkeep is 50 percent"),
        "{by_agent:?}"
    );
    assert!(limited(site.dir.path(), "agent:b"));
    let served = site.owner().await;
    served.mark_served();
    let by_served = served.admit(&held.id).await;
    assert!(denied(&by_served, "is not admitted"), "{by_served:?}");
    let admitted = site.owner().await.admit(&held.id).await.unwrap();
    assert_eq!(admitted.status, Status::Open);
    assert_eq!(
        site.limit_lines(OWNER),
        1,
        "only the served session was refused"
    );
}

#[tokio::test]
async fn a_raise_of_the_share_of_upkeep_admits_work_up_to_the_raised_percentage() {
    let site = Site::new(upkeep(50));
    site.half_upkeep().await;
    let owner = site.owner().await;
    let r = owner
        .raise_limit(None, &Limit::Upkeep, 101, None, Duration::hours(1))
        .await;
    assert!(denied(&r, "100 at most"), "{r:?}");
    owner
        .raise_limit(None, &Limit::Upkeep, 60, None, Duration::hours(1))
        .await
        .unwrap();
    let more = site.own("agent:a", spec("More of its own")).await;
    assert_eq!(more.status, Status::Open, "50 percent is under 60");
    site.done(&more, "agent:a").await;
    let over = site.own("agent:a", spec("Still more")).await;
    assert_eq!(over.status, Status::Proposed, "2 of 3 is over 60 percent");
    let log = std::fs::read_to_string(site.dir.path().join("state/decisions.jsonl")).unwrap();
    assert!(log.contains("the share of upkeep is 60 percent"), "{log}");
}

#[tokio::test]
async fn the_report_has_a_row_for_each_of_the_five_limits_when_none_is_set() {
    let site = Site::new(DomainLimits::default());
    let rows = site.owner().await.limits(None).await.unwrap();
    let names: Vec<&str> = rows.iter().map(|r| r.limit.as_str()).collect();
    assert_eq!(
        names,
        [
            "spend",
            "throughput",
            "outbound",
            "landing:main",
            "rules",
            "upkeep"
        ]
    );
    for row in &rows {
        assert_eq!(row.cap, None, "{row:?}");
        assert_eq!(row.note.as_deref(), Some("not set"), "{row:?}");
        assert_eq!((row.fraction, &row.raised_by), (None, &None), "{row:?}");
    }
}

/// A site with every limit set, three completed tasks of which one is upkeep, one post
/// of the family `github`, and a raise of the share of throughput of `agent:a` to 4.
async fn busy_site() -> (Site, ApprovalRecord) {
    let site = Site::new(DomainLimits {
        claims_per_hour: Some(2),
        upkeep_percent: Some(50),
        window_days: None,
        outbound: BTreeMap::from([("github".to_string(), 4)]),
    });
    site.grants(|g| {
        g.gates.max_enforced_rules = Some(3);
        let spend = dm_policy::SpendLimit {
            session_usd_cents: Some(5),
            day_tokens: Some(1000),
            ..dm_policy::SpendLimit::default()
        };
        g.spend.insert("openai".into(), spend);
        g.landing.get_mut("main").unwrap().per_day = Some(1);
    });
    site.half_upkeep().await;
    let posted = site.asked(from("G", "github:o/r#1")).await;
    site.done(&posted, "agent:a").await;
    let owner = site.owner().await;
    owner
        .post(
            &posted.id,
            "https://example.test/1",
            DOMAIN,
            Label::Restricted,
        )
        .await
        .unwrap();
    let hour = Duration::hours(1);
    let raised = owner
        .raise_limit(None, &Limit::Throughput, 4, Some("agent:a"), hour)
        .await
        .unwrap();
    (site, raised)
}

fn row<'a>(rows: &'a [LimitRow], limit: &str, unit: &str) -> &'a LimitRow {
    let found = rows.iter().find(|r| r.limit == limit && r.unit == unit);
    found.unwrap_or_else(|| panic!("no row {limit} in {unit}: {rows:?}"))
}

#[tokio::test]
async fn the_report_names_the_approval_that_raised_a_share() {
    let (site, raised) = busy_site().await;
    let rows = site.session("agent:a").await.limits(None).await.unwrap();
    let claims = row(&rows, "throughput", "claims per hour");
    assert_eq!(claims.scope, "agent:a");
    assert_eq!((claims.cap, claims.configured), (Some(4.0), Some(2.0)));
    assert_eq!(claims.raised_by.as_deref(), Some(raised.id.as_str()));
    assert_eq!((claims.now, claims.fraction), (Some(2.0), Some(0.5)));
    assert_eq!(claims.note, None);
    // to a person the row says that the limit is not that person's
    let rows = site.owner().await.limits(None).await.unwrap();
    let claims = row(&rows, "throughput", "claims per hour");
    assert_eq!((claims.cap, &claims.raised_by), (Some(2.0), &None));
    let note = claims.note.as_deref().unwrap();
    assert!(note.contains("a person is not limited"), "{note}");
}

#[tokio::test]
async fn the_report_gives_each_limit_in_its_unit_with_its_count() {
    let (site, _) = busy_site().await;
    let rows = site.session("agent:a").await.limits(None).await.unwrap();
    let names: Vec<&str> = rows.iter().map(|r| r.limit.as_str()).collect();
    let paper = [
        "spend",
        "spend",
        "throughput",
        "outbound:github",
        "landing:main",
        "rules",
        "upkeep",
    ];
    assert_eq!(names, paper);
    let cents = row(&rows, "spend", "US cents");
    assert_eq!((cents.cap, cents.now), (Some(5.0), None));
    assert_eq!(cents.scope, "openai, one session");
    let note = cents.note.as_deref().unwrap();
    assert!(note.contains("the egress meters spend"), "{note}");
    assert_eq!(row(&rows, "spend", "tokens").cap, Some(1000.0));
    let posts = row(&rows, "outbound:github", "posts per day");
    assert_eq!(
        (posts.now, posts.cap, posts.fraction),
        (Some(1.0), Some(4.0), Some(0.25))
    );
    let landed = row(&rows, "landing:main", "changes per day");
    assert_eq!((landed.now, landed.cap), (Some(0.0), Some(1.0)));
    let rules = row(&rows, "rules", "rules");
    assert_eq!((rules.now, rules.cap), (Some(0.0), Some(3.0)));
    let share = row(&rows, "upkeep", "percent of completed tasks");
    assert_eq!((share.cap, share.scope.as_str()), (Some(50.0), DOMAIN));
    // one of the three completed tasks is upkeep
    let now = share.now.unwrap();
    assert!((now - 100.0 / 3.0).abs() < 1e-9, "{now}");
}

/// The caller that can read the meter of the egress gives the spend of one day. A row
/// over one day then has its count, in its own unit. A row over one session has none.
#[tokio::test]
async fn the_report_counts_the_spend_of_a_day_from_the_meter_that_the_caller_read() {
    let site = Site::new(DomainLimits::default());
    site.grants(|g| {
        let spend = dm_policy::SpendLimit {
            session_tokens: Some(50),
            day_tokens: Some(1000),
            session_usd_cents: None,
            day_usd_cents: Some(200),
        };
        g.spend.insert("openai".into(), spend.clone());
        g.spend.insert("other".into(), spend);
    });
    let read = Metered {
        tokens: 250,
        cents: 30,
        unpriced: vec!["m-x".into()],
    };
    let metered = BTreeMap::from([("openai".to_string(), read)]);
    let owner = site.owner().await;
    let rows = owner.limits_metered(None, &metered).await.unwrap();
    let of = |unit: &str, scope: &str| {
        let found = rows.iter().find(|r| r.unit == unit && r.scope == scope);
        found.unwrap_or_else(|| panic!("no row of {scope} in {unit}: {rows:?}"))
    };
    let tokens = of("tokens", "openai, one day");
    assert_eq!((tokens.now, tokens.fraction), (Some(250.0), Some(0.25)));
    assert_eq!(tokens.note, None);
    let cents = of("US cents", "openai, one day");
    assert_eq!((cents.now, cents.fraction), (Some(30.0), Some(0.15)));
    let note = cents.note.as_deref().unwrap();
    assert!(note.contains("does not price m-x"), "{note}");
    let session = of("tokens", "openai, one session");
    assert_eq!((session.now, session.cap), (None, Some(50.0)));
    let note = session.note.as_deref().unwrap();
    assert!(note.contains("the egress meters spend"), "{note}");
    // the meter of the other destination was not read
    assert_eq!(of("tokens", "other, one day").now, None);
    // without a meter the report is the one that the memory server gives
    let plain = owner.limits(None).await.unwrap();
    assert!(plain.iter().all(|r| r.limit != "spend" || r.now.is_none()));
}

#[tokio::test]
async fn a_change_of_an_agent_over_the_share_lands_after_a_person_raised_it() {
    let site = Site::new(DomainLimits::default());
    site.grants(|g| g.landing.get_mut("main").unwrap().per_day = Some(1));
    for title in ["First", "Second"] {
        let t = site.asked(spec(title)).await;
        let s = site.session("agent:a").await;
        s.claim(&t.id, LEASE).await.unwrap();
        let h = site.handoff().await;
        s.queue_landing(&t.id, None, "main", &h).await.unwrap();
    }
    let owner = site.owner().await;
    owner.land("main").await.unwrap();
    let second = owner.land("main").await;
    assert!(
        denied(&second, "lands 1 changes of agents in 24 hours"),
        "{second:?}"
    );
    let limit = Limit::Landing("main".into());
    let raised = owner
        .raise_limit(None, &limit, 2, None, Duration::hours(1))
        .await
        .unwrap();
    assert_eq!(
        (raised.spec.scope.as_str(), raised.spec.domain.as_str()),
        (DOMAIN, DOMAIN)
    );
    assert_eq!(owner.land("main").await.unwrap().status, Status::Done);
}
