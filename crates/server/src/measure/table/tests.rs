use super::*;
use crate::measure::{Target, TARGETS};
use crate::WriteRequest;
use dm_core::{EntryType, Label};
use dm_ledger::{Held, TaskSpec};
use dm_policy::{DomainLimits, Grants, LandingQueue};
use std::sync::{Arc, Mutex};
use time::macros::datetime;

const T0: OffsetDateTime = datetime!(2026-10-01 10:00 UTC);
const OWNER: &str = "human:test:me";
const DOMAIN: &str = "dom-me";
const LEASE: Duration = Duration::minutes(30);

/// A root with the domains `dom-me` and `dom-b`, the landing queue `main` on `dom-me`,
/// and a clock that every session's ledger reads.
struct Site {
    dir: tempfile::TempDir,
    clock: Arc<Mutex<OffsetDateTime>>,
}

impl Site {
    fn new() -> Site {
        let dir = tempfile::tempdir().unwrap();
        Memory::init(dir.path(), OWNER, DOMAIN).unwrap();
        let path = dir.path().join(crate::GRANTS_FILE);
        let mut g = Grants::load(&path).unwrap();
        let d = g.domains[DOMAIN].clone();
        g.domains.insert("dom-b".into(), d);
        let q = LandingQueue {
            domain: DOMAIN.into(),
            per_day: None,
            count_after_hours: None,
        };
        g.landing.insert("main".into(), q);
        let shares = DomainLimits {
            outbound: BTreeMap::from([("github".to_string(), 5)]),
            ..DomainLimits::default()
        };
        g.limits.insert(DOMAIN.into(), shares);
        std::fs::write(&path, g.render().unwrap()).unwrap();
        let clock = Arc::new(Mutex::new(T0));
        Site { dir, clock }
    }

    /// Set the clock to `mins` minutes after `T0`.
    fn at(&self, mins: i64) {
        *self.clock.lock().unwrap() = T0 + Duration::minutes(mins);
    }

    /// `memory` on the site's clock.
    async fn timed(&self, memory: Memory) -> Memory {
        let c = self.clock.clone();
        let clock = move || *c.lock().unwrap();
        memory.ledger.lock().await.set_clock(clock);
        memory
    }

    /// A new session of `author` on the site's clock.
    async fn session(&self, author: &str) -> Memory {
        let m = Memory::open(self.dir.path()).unwrap().with_author(author);
        self.timed(m).await
    }

    async fn owner(&self) -> Memory {
        self.session(OWNER).await
    }

    /// A task as the ledger takes it, admitted: no policy is asked, so that a test can
    /// file a sign-off task and a task under a task definition.
    async fn task(&self, spec: TaskSpec) -> Task {
        let m = self.owner().await;
        let mut l = m.ledger.lock().await;
        let t = l.create(spec).await.unwrap().task;
        l.admit(&t.id).await.unwrap()
    }

    /// Run `act` on the ledger for the claim that `who` makes of `id` now.
    async fn held<T>(
        &self,
        id: &str,
        who: &str,
        act: impl AsyncFnOnce(&mut Ledger, Held<'_>) -> T,
    ) -> T {
        let m = self.owner().await;
        let mut l = m.ledger.lock().await;
        let claim = l.claim(id, who, LEASE, &[], Label::Restricted).await;
        let generation = claim.unwrap().generation;
        let by = Held {
            holder: who,
            generation,
        };
        act(&mut l, by).await
    }

    async fn completed(&self, id: &str) {
        let done = async |l: &mut Ledger, by: Held<'_>| l.complete(id, by, "mem-h").await;
        self.held(id, "agent:a", done).await.unwrap();
    }

    async fn released(&self, id: &str) {
        let stop = async |l: &mut Ledger, by: Held<'_>| l.release(id, by, None).await;
        self.held(id, "agent:a", stop).await.unwrap();
    }

    /// `id`, claimed and then given up until `event` comes.
    async fn waits(&self, id: &str, event: &str) {
        let wait = async |l: &mut Ledger, by: Held<'_>| l.wait_for(id, by, event, None).await;
        self.held(id, "agent:a", wait).await.unwrap();
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

    /// A task that the owner filed, that `agent:a` claimed now and handed to the queue
    /// `main` after `work` minutes.
    async fn queued(&self, title: &str, work: i64) -> Task {
        let owner = self.owner().await;
        let t = owner.create_task(spec(title)).await.unwrap().task;
        owner.admit(&t.id).await.unwrap();
        let s = self.session("agent:a").await;
        s.claim(&t.id, LEASE).await.unwrap();
        *self.clock.lock().unwrap() += Duration::minutes(work);
        let h = self.handoff().await;
        s.queue_landing(&t.id, None, "main", &h).await.unwrap()
    }

    /// Append lines to a log under the state directory.
    fn log(&self, file: &str, lines: &[Value]) {
        let path = self.dir.path().join("state").join(file);
        for line in lines {
            reads::try_append_line(&path, &line.to_string()).unwrap();
        }
    }

    /// The table of `dom-me` over 7 days, as the owner reads it.
    async fn table(&self) -> Metrics {
        let owner = self.owner().await;
        owner.metrics(Some(DOMAIN), 7, None).await.unwrap()
    }

    /// The value of the row `name` of the owner's table.
    async fn value(&self, name: &str) -> Value {
        row(&self.table().await, name).value.clone()
    }

    async fn targets(&self) -> Vec<Target> {
        self.owner().await.targets(7).await.unwrap()
    }
}

fn spec(title: &str) -> TaskSpec {
    TaskSpec {
        domain: DOMAIN.into(),
        title: title.into(),
        body: "Do it.\n".into(),
        created_by: OWNER.into(),
        ..TaskSpec::default()
    }
}

fn from(title: &str, origin: &str) -> TaskSpec {
    TaskSpec {
        origin: Some(origin.into()),
        ..spec(title)
    }
}

fn defined(title: &str, definition: &str) -> TaskSpec {
    TaskSpec {
        definition: Some(definition.into()),
        ..spec(title)
    }
}

fn row<'a>(table: &'a Metrics, name: &str) -> &'a Metric {
    let found = table.rows.iter().find(|r| r.name == name);
    found.unwrap_or_else(|| panic!("no row {name}"))
}

/// A time `mins` minutes after `T0`, as a log line has it.
fn stamp(mins: i64) -> String {
    (T0 + Duration::minutes(mins)).format(&Rfc3339).unwrap()
}

/// A line of the read log.
fn read(mins: i64, session: &str, tool: &str, hits: usize) -> Value {
    json!({"at": stamp(mins), "session": session, "tool": tool, "hits": hits, "tokens": 10})
}

/// A line of the decision log, as `Memory::log_decision` writes it.
fn decision(mins: i64, domain: &str, allow: bool, cites: &[&str], held: Option<&str>) -> Value {
    json!({
        "at": stamp(mins), "session": "s-1", "action": "write", "principal": "agent:a",
        "class": "agent", "domain": domain, "allow": allow, "admit": false,
        "reason": "a reason", "cites": cites, "held_task": held,
    })
}

/// A root on which every source of the table holds something.
async fn busy() -> Site {
    let site = Site::new();
    let done = site.task(defined("Fix", "fix@1")).await;
    site.released(&done.id).await;
    site.waits(&done.id, &format!("{VERDICT_EVENT}{}", done.id))
        .await;
    site.at(5);
    let m = site.owner().await;
    let event = format!("{VERDICT_EVENT}{}", done.id);
    let mut l = m.ledger.lock().await;
    l.complete_awaited(&done.id, &event, "mem-h").await.unwrap();
    drop(l);
    site.task(from("Approve", "signoff:mem-x")).await;
    site.queued("Change", 0).await;
    site.owner().await.land("main").await.unwrap();
    site.log(
        "reads.jsonl",
        &[read(1, "s-1", "search", 1), read(2, "s-1", "search", 0)],
    );
    let refusal = decision(1, DOMAIN, false, &["pol-L1-001"], Some(&done.id));
    site.log("decisions.jsonl", &[refusal]);
    site
}

#[tokio::test]
async fn the_table_has_the_rows_of_the_paper_in_order_whatever_the_logs_hold() {
    assert_eq!(PAPER_METRICS[0], "Cost per completed task");
    assert_eq!(
        PAPER_METRICS[20],
        "Pinned tasks, and queue depth per position"
    );
    for site in [Site::new(), busy().await] {
        let table = site.table().await;
        let names: Vec<&str> = table.rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, PAPER_METRICS);
        assert_eq!((table.domain.as_str(), table.window_days), (DOMAIN, 7));
        assert_eq!(table.prices, "none", "no price vector under the root");
    }
}

#[tokio::test]
async fn every_row_has_a_value_or_says_why_it_has_none() {
    let mut measured = Vec::new();
    for site in [Site::new(), busy().await] {
        let table = site.table().await;
        for r in &table.rows {
            let why = r.why_not.as_deref().unwrap_or_default();
            if r.measured {
                assert!(!r.value.is_null() && !r.from.is_empty(), "{r:?}");
            } else {
                assert!(r.value.is_null() && !why.is_empty(), "{r:?}");
            }
        }
        measured.push(table.rows.iter().filter(|r| r.measured).count());
    }
    assert!(
        measured[0] < measured[1],
        "the logs answer more rows of a root with activity: {measured:?}"
    );
    let busy = busy().await.table().await;
    for name in [PAPER_METRICS[9], PAPER_METRICS[10], PAPER_METRICS[20]] {
        assert!(!row(&busy, name).measured, "{name} needs what is not built");
    }
}

#[tokio::test]
async fn the_cost_of_a_task_is_what_the_caller_passed_or_names_the_command() {
    let site = Site::new();
    let owner = site.owner().await;
    let cost = json!({"usd": 0.42, "tasks": 3});
    let table = owner.metrics(Some(DOMAIN), 7, Some(cost.clone()));
    let table = table.await.unwrap();
    let passed = &table.rows[0];
    assert_eq!((passed.measured, &passed.value), (true, &cost));
    assert_eq!((passed.from.as_str(), &passed.why_not), ("caller", &None));
    let none = &site.table().await.rows[0];
    assert!(!none.measured && none.value.is_null(), "{none:?}");
    let why = none.why_not.as_deref().unwrap();
    assert!(why.contains("amem cost"), "{why}");
}

#[tokio::test]
async fn a_search_with_no_hit_and_a_search_after_a_search_are_counted() {
    let site = Site::new();
    let name = PAPER_METRICS[4];
    assert!(!row(&site.table().await, name).measured);
    site.at(60);
    site.log(
        "reads.jsonl",
        &[
            // before the window: it is the pull before the next search, and no more
            read(-8 * 24 * 60, "s-1", "search", 0),
            read(1, "s-1", "search", 2),
            read(2, "s-1", "get", 1),
            read(3, "s-1", "search", 0),
            // neither the pull of another session nor a push comes between two searches
            read(4, "s-2", "get", 1),
            read(5, "s-1", "prefetch", 3),
            read(6, "s-1", "search", 1),
        ],
    );
    let value = site.value(name).await;
    assert_eq!(value["searches"], 3, "{value}");
    assert_eq!(value["zero_result_rate"], json!(1.0 / 3.0), "{value}");
    assert_eq!(
        value["re_query_rate"],
        json!(2.0 / 3.0),
        "the first search of the window follows a search, the second follows a get: {value}"
    );
}

#[tokio::test]
async fn a_refusal_that_cites_a_policy_is_a_citation_and_an_allowed_one_is_an_override() {
    let site = Site::new();
    site.at(60);
    let old = -8 * 24 * 60;
    site.log(
        "decisions.jsonl",
        &[
            decision(1, DOMAIN, false, &["pol-L1-001", "pol-L2-031"], None),
            decision(2, DOMAIN, false, &["pol-L1-001"], None),
            decision(3, DOMAIN, true, &["pol-L2-031"], None),
            decision(4, DOMAIN, true, &[], None),
            decision(5, "dom-b", false, &["pol-L1-001"], None),
            decision(6, "dom-b", true, &["pol-L1-001"], None),
            decision(old, DOMAIN, false, &["pol-L1-001"], None),
            decision(old, DOMAIN, true, &["pol-L1-001"], None),
            json!({"at": "no time", "domain": DOMAIN, "allow": false, "cites": ["pol-L1-001"]}),
        ],
    );
    let table = site.table().await;
    let cited = row(&table, PAPER_METRICS[5]);
    assert_eq!(cited.from, "decision log");
    assert_eq!(
        cited.value,
        json!({
            "citations": {"pol-L1-001": 2, "pol-L2-031": 1},
            "overrides": {"pol-L2-031": 1},
        })
    );
    assert_eq!(
        row(&table, PAPER_METRICS[6]).value,
        json!({"dom-b": 1, "dom-me": 2}),
        "the refusals of the window, for each granted domain"
    );
}

#[tokio::test]
async fn the_backlog_is_the_open_approvals_and_the_latency_is_on_the_clock_of_the_ledger() {
    let site = Site::new();
    let name = PAPER_METRICS[8];
    site.task(from("Approve a", "signoff:mem-a")).await;
    let declined = site.task(from("Approve b", "promote:mem-b")).await;
    site.task(spec("No approval")).await;
    let value = site.value(name).await;
    assert_eq!(
        (&value["backlog"], &value["closed"]),
        (&json!(2), &json!(0))
    );
    assert!(value["median_decision_secs"].is_null(), "{value}");
    assert!(!row(&site.table().await, PAPER_METRICS[11]).measured);

    site.at(5);
    let given = site.task(from("Approve c", "signoff:mem-c")).await;
    site.at(15);
    site.completed(&given.id).await;
    site.at(40);
    let m = site.owner().await;
    let mut l = m.ledger.lock().await;
    l.invalidate(&declined.id, "declined", None).await.unwrap();
    drop(l);
    let value = site.value(name).await;
    assert_eq!(
        (&value["backlog"], &value["closed"]),
        (&json!(1), &json!(2)),
        "a closed approval is not in the backlog: {value}"
    );
    assert_eq!(
        value["median_decision_secs"], 600,
        "10 minutes and 40 minutes from the filing; the lower one is the median: {value}"
    );
    let agreed = site.value(PAPER_METRICS[11]).await;
    assert_eq!(agreed["with_proposer"], 0.5, "{agreed}");
}

/// A task as the ledger would hold it, open, for the rules that need no ledger.
fn open_task(id: &str, priority: i64, depends_on: &[&str]) -> Task {
    Task {
        id: id.into(),
        spec: TaskSpec {
            priority,
            depends_on: depends_on.iter().map(|d| (*d).to_string()).collect(),
            ..spec(id)
        },
        created: T0,
        status: Status::Open,
        holder: None,
        lease_until: None,
        generation: 0,
        handoff: None,
        reason: None,
        superseded_by: None,
        posted_to: None,
        waiting_on: None,
        effects: Vec::new(),
    }
}

#[tokio::test]
async fn a_cycle_counts_each_task_on_it_and_a_dependency_of_lower_priority_is_a_reversal() {
    let tasks = [
        open_task("a", 0, &["b"]),
        open_task("b", 0, &["a"]),
        // it leads to the cycle and is not on it
        open_task("c", 0, &["a"]),
        open_task("d", 0, &["gone"]),
    ];
    let open: BTreeMap<&str, &Task> = tasks.iter().map(|t| (t.id.as_str(), t)).collect();
    assert_eq!(tangles(&open), (2, 0));

    let site = Site::new();
    let low = site.task(spec("Low")).await;
    let high = TaskSpec {
        priority: 5,
        depends_on: vec![low.id.clone()],
        ..spec("High")
    };
    site.task(high).await;
    let level = TaskSpec {
        depends_on: vec![low.id.clone()],
        ..spec("Level")
    };
    site.task(level).await;
    let name = PAPER_METRICS[12];
    let value = site.value(name).await;
    assert_eq!(
        value,
        json!({"tasks_on_a_cycle": 0, "priority_reversals": 1})
    );
    site.completed(&low.id).await;
    let value = site.value(name).await;
    assert_eq!(
        value["priority_reversals"], 0,
        "a done dependency holds nothing up"
    );
}

#[tokio::test]
async fn a_release_is_a_stop_of_its_definition_and_a_wait_for_a_verdict_or_a_landing_is_not() {
    let site = Site::new();
    let fix = site.task(defined("Fix", "fix@1")).await;
    let verdict = format!("{VERDICT_EVENT}{}", fix.id);
    site.released(&fix.id).await;
    site.at(10);
    site.waits(&fix.id, &verdict).await;
    let change = site.task(defined("Change", "fix@1")).await;
    site.waits(&change.id, &landing_event("main")).await;
    let asks = site.task(defined("Ask", "ask@2")).await;
    site.waits(&asks.id, "review:legal").await;
    let plain = site.task(spec("No definition")).await;
    site.released(&plain.id).await;
    site.waits(&plain.id, "review:legal").await;
    let name = PAPER_METRICS[18];
    assert_eq!(
        site.value(name).await,
        json!({"stops": {"fix@1": 1}, "escalations": {"ask@2": 1}})
    );

    let checks = PAPER_METRICS[13];
    assert!(!row(&site.table().await, checks).measured, "no verdict yet");
    site.at(25);
    let m = site.owner().await;
    m.ledger
        .lock()
        .await
        .arrived(DOMAIN, &verdict)
        .await
        .unwrap();
    assert_eq!(
        site.value(checks).await,
        json!({"median_latency_secs": {"fix@1": 900}}),
        "from the wait at minute 10 to the verdict at minute 25"
    );

    let refused = |mins, held: &str| decision(mins, DOMAIN, false, &[], Some(held));
    site.log(
        "decisions.jsonl",
        &[
            refused(11, &fix.id),
            refused(12, &fix.id),
            refused(13, &plain.id),
            decision(14, DOMAIN, true, &[], Some(&fix.id)),
        ],
    );
    assert_eq!(
        site.value(PAPER_METRICS[15]).await,
        json!({"enforced_rules": 0, "denials": {"fix@1": 2}})
    );
    site.at(8 * 24 * 60);
    assert_eq!(
        site.value(name).await,
        json!({"stops": {}, "escalations": {}}),
        "the window has passed"
    );
}

/// The row of the share of upkeep counts the upkeep among the completed tasks. Of the
/// upkeep, it counts the tasks that an agent filed and completed itself: a curation
/// task is upkeep and is not one of them.
#[tokio::test]
async fn the_upkeep_row_counts_the_tasks_that_an_agent_filed_and_completed_itself() {
    let site = Site::new();
    let own = TaskSpec {
        created_by: "agent:a".into(),
        ..spec("Its own")
    };
    let specs = [spec("Asked"), own, from("Curate", "curator:stale:mem-x")];
    for spec in specs {
        let t = site.task(spec).await;
        site.completed(&t.id).await;
    }
    let v = site.value("Share of throughput spent on upkeep").await;
    assert_eq!((&v["upkeep"], &v["done"]), (&json!(2), &json!(3)), "{v}");
    assert_eq!(v["self_filed"], 1, "{v}");
    assert!(
        (v["share"].as_f64().unwrap() - 2.0 / 3.0).abs() < 1e-9,
        "{v}"
    );
    assert_eq!(v["ceiling_percent"], Value::Null, "{v}");
}

#[tokio::test]
async fn claim_to_landed_is_the_median_over_the_landed_tasks_of_a_queue() {
    let site = Site::new();
    let name = PAPER_METRICS[14];
    site.queued("First", 10).await;
    let none = site.table().await;
    assert!(!row(&none, name).measured, "a change waits; none landed");
    assert!(row(&none, name).value.is_null());
    site.at(20);
    site.owner().await.land("main").await.unwrap();
    site.at(30);
    site.queued("Second", 0).await;
    site.at(100);
    site.queued("Third", 5).await;
    site.at(110);
    site.owner().await.land("main").await.unwrap();
    site.owner().await.land("main").await.unwrap();
    // from the claim, not from the queue: minute 0 to 20, minute 30 to 110, minute 100 to 110
    assert_eq!(
        site.value(name).await,
        json!({"landed": {"main": 3}, "median_secs": {"main": 1200}})
    );
}

#[tokio::test]
async fn the_posts_of_the_last_day_are_counted_by_family_beside_the_rate() {
    let site = Site::new();
    let name = PAPER_METRICS[16];
    let t = site.task(from("Issue", "github:o/r#1")).await;
    let s = site.session("agent:a").await;
    s.claim(&t.id, LEASE).await.unwrap();
    let h = site.handoff().await;
    s.complete(&t.id, None, &h).await.unwrap();
    assert_eq!(
        site.value(name).await,
        json!({"posted_last_day": {}, "per_day": {"github": 5}})
    );
    let tap = site.session("tap:github").await;
    tap.post(&t.id, "https://example.test/1", DOMAIN, Label::Restricted)
        .await
        .unwrap();
    assert_eq!(
        site.value(name).await["posted_last_day"],
        json!({"github": 1})
    );
    site.at(24 * 60 + 1);
    assert_eq!(site.value(name).await["posted_last_day"], json!({}));
}

#[tokio::test]
async fn a_metric_counts_no_task_above_the_handling_of_the_session() {
    let site = Site::new();
    site.task(spec("Seen")).await;
    let capped = async || {
        let m = Memory::open(site.dir.path()).unwrap();
        let m = m.with_author("agent:a").with_handling_cap(Label::Internal);
        let m = site.timed(m).await;
        m.metrics(Some(DOMAIN), 7, None).await.unwrap().rows
    };
    let before = (capped().await, site.table().await.rows);
    let hidden = |s: TaskSpec| TaskSpec {
        classification: Label::Restricted,
        ..s
    };
    let low = site.task(hidden(spec("Low"))).await;
    let high = TaskSpec {
        priority: 5,
        depends_on: vec![low.id.clone()],
        ..hidden(from("Approve", "signoff:mem-a"))
    };
    site.task(high).await;
    let invalid = site.task(hidden(defined("Fix", "fix@1"))).await;
    site.released(&invalid.id).await;
    let m = site.owner().await;
    let mut l = m.ledger.lock().await;
    l.invalidate(&invalid.id, "obsolete", None).await.unwrap();
    drop(l);
    let own = TaskSpec {
        created_by: "agent:a".into(),
        ..hidden(spec("Own"))
    };
    let own = site.task(own).await;
    site.completed(&own.id).await;
    let after = (capped().await, site.table().await.rows);
    assert_eq!(
        before.0, after.0,
        "no restricted task moves a row of an internal session"
    );
    let moved: Vec<&str> = (before.1.iter().zip(&after.1))
        .filter(|(b, a)| b != a)
        .map(|(b, _)| b.name.as_str())
        .collect();
    let rows = [7, 8, 12, 18, 19].map(|i| PAPER_METRICS[i]);
    assert_eq!(moved, rows, "the same tasks move the rows of the owner");
}

#[tokio::test]
async fn the_targets_of_the_search_latency_are_measured_from_the_timed_searches() {
    let site = Site::new();
    let none = site.targets().await;
    let names: Vec<&str> = none.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, TARGETS.map(|(name, _, _)| name));
    for t in &none {
        assert_eq!((&t.measured, t.met), (&None, None), "{t:?}");
        assert!(t.why_not.as_deref().is_some_and(|why| !why.is_empty()));
    }
    assert_eq!(
        none[0].why_not.as_deref(),
        Some("no timed search in the window")
    );

    site.at(60);
    let timed = |mins, tool: &str, ms: u64| {
        let mut line = read(mins, "s-1", tool, 1);
        line["ms"] = json!(ms);
        line
    };
    site.log(
        "reads.jsonl",
        &[
            timed(1, "search", 900),
            timed(2, "search", 10),
            timed(3, "search", 20),
            // a search without a time, a read that is no search, a search before the window
            read(4, "s-1", "search", 1),
            timed(5, "get", 5000),
            timed(-8 * 24 * 60, "search", 7000),
        ],
    );
    let targets = site.targets().await;
    let of = |t: &Target| (t.target.clone(), t.measured.clone(), t.met);
    assert_eq!(
        of(&targets[0]),
        ("100 ms".into(), Some("20 ms".into()), Some(true))
    );
    assert_eq!(
        of(&targets[1]),
        ("500 ms".into(), Some("900 ms".into()), Some(false))
    );
    assert_eq!(targets[0].why_not, None);
    for t in &targets[2..] {
        assert_eq!((&t.measured, t.met), (&None, None), "{t:?}");
    }
}

#[tokio::test]
async fn a_search_of_a_session_is_timed_in_the_read_log() {
    let site = Site::new();
    let owner = site.owner().await;
    owner.search("anything", None, 5).await.unwrap();
    owner.list(None, None, 5).await.unwrap();
    let reads = reads::all(site.dir.path()).unwrap();
    let timed: Vec<(&str, bool)> = reads
        .iter()
        .map(|r| (r.tool.as_str(), r.ms.is_some()))
        .collect();
    assert_eq!(timed, [("search", true), ("list", false)]);
    let targets = site.targets().await;
    assert!(targets[0].measured.is_some(), "{:?}", targets[0]);
}
