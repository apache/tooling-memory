//! The rules of the evaluation. Every time that a rule compares is fixed: the pure
//! tests give the times of the runs as values, and the tests that read tapes compare
//! no time of the clock.

use super::*;
use crate::tape::TapeStart;
use dm_core::TrustTier;
use std::path::Path;
use time::macros::datetime;
use time::Duration;

const PLAN: &str = r#"# a plan of the tests
preamble = "Read the record first. "

[plan]
name = "demo"
domain = "dom-me"
baseline = "no-memory"
arms = ["no-memory", "full"]
seeds = 2
margin = 0.1
judge = "regex"

[[task]]
id = "a"
prompt = "What is A?"
expect = "A"
stratum = "failed"
set = "frozen"

[[task]]
id = "b"
prompt = "What is B?"
expect = "B"
"#;

/// When the runs of the pure tests start.
const T: OffsetDateTime = datetime!(2026-11-02 10:00:00 UTC);
/// The snapshot of the runs that keep a tape: before any clock that runs the tests.
const AS_OF: OffsetDateTime = datetime!(2020-01-01 00:00:00 UTC);
const CLAUDE: Launched<'static> = Launched {
    harness: "claude",
    model: Some("m-1"),
};

fn plan() -> Plan {
    Plan::parse(PLAN).unwrap()
}

/// The plan of the tests with each `from` replaced by its `to`.
fn plan_with(changes: &[(&str, &str)]) -> Plan {
    let mut text = PLAN.to_string();
    for (from, to) in changes {
        assert!(text.contains(from), "{from}");
        text = text.replace(from, to);
    }
    Plan::parse(&text).unwrap()
}

fn denied<T: std::fmt::Debug>(r: Result<T, Error>, part: &str) {
    assert!(
        matches!(&r, Err(Error::Denied(why)) if why.contains(part)),
        "{part}: {r:?}"
    );
}

#[test]
fn a_plan_is_refused_without_what_fixes_the_comparison() {
    let cases: [(&str, &str, &str); 11] = [
        ("[plan]", "[other]", "no [plan] table"),
        ("margin = 0.1\n", "", "no margin"),
        ("margin = 0.1", "margin = 1.5", "fraction from 0 to 1"),
        ("margin = 0.1", "margin = -0.1", "fraction from 0 to 1"),
        ("seeds = 2", "seeds = 1", "two seeds or more"),
        ("\"full\"]", "\"bogus\"]", "is not an arm"),
        ("\"full\"]", "\"full\", \"full\"]", "the arm full twice"),
        (
            "baseline = \"no-memory\"",
            "baseline = \"raw-only\"",
            "not among its arms",
        ),
        ("id = \"b\"", "id = \"a\"", "the task a twice"),
        (
            "judge = \"regex\"",
            "judge = \"regex\"\nmodel = \"regex\"",
            "the judge is the model",
        ),
        ("seeds = 2", "seeds = \"two\"", "does not read"),
    ];
    for (from, to, why) in cases {
        assert!(PLAN.contains(from), "{from}");
        denied(Plan::parse(&PLAN.replace(from, to)), why);
    }
    let no_task = PLAN.split("[[task]]").next().unwrap();
    denied(Plan::parse(no_task), "no task");
    // the plan as it is reads, with the defaults
    let p = plan();
    assert_eq!((p.seeds, p.margin, p.accepted_cost), (2, 0.1, 1.0));
    assert_eq!(p.arms, [Arm::NoMemory, Arm::Full]);
    assert_eq!(p.tasks[0].stratum, Stratum::Failed);
    assert_eq!(p.tasks[0].set, TaskSet::Frozen);
    assert_eq!(p.tasks[1].stratum, Stratum::Passed);
    assert_eq!(p.tasks[1].set, TaskSet::Rolling);
    assert_eq!(
        p.prompt_of("a").as_deref(),
        Some("Read the record first. What is A?")
    );
    assert_eq!(p.prompt_of("c"), None);
}

#[test]
fn a_plan_digest_moves_with_the_plan_and_with_nothing_else() {
    let digest = plan().digest();
    assert_eq!(digest.len(), 64);
    let same = [
        ("# a plan of the tests", "# another comment\n# and one more"),
        (
            "expect = \"B\"",
            "expect = \"B\"\nanswers = [\"An entry\"]\ncounts = \"x\"",
        ),
        (
            "name = \"demo\"\ndomain = \"dom-me\"",
            "domain = \"dom-me\"\nname = \"demo\"",
        ),
        (
            "id = \"b\"\nprompt = \"What is B?\"",
            "prompt = \"What is B?\"\nid = \"b\"",
        ),
    ];
    for change in same {
        assert_eq!(plan_with(&[change]).digest(), digest, "{change:?}");
    }
    let other = [
        ("margin = 0.1", "margin = 0.2"),
        ("seeds = 2", "seeds = 3"),
        ("\"full\"]", "\"full\", \"t0-only\"]"),
        ("baseline = \"no-memory\"", "baseline = \"full\""),
        ("judge = \"regex\"", "judge = \"regex-2\""),
        ("Read the record first. ", "Read the record. "),
        ("What is A?", "What is A now?"),
        ("expect = \"A\"", "expect = \"AA\""),
        ("stratum = \"failed\"", "stratum = \"passed\""),
        ("set = \"frozen\"", "set = \"rolling\""),
    ];
    let mut seen = std::collections::BTreeSet::from([digest.clone()]);
    for change in other {
        assert!(seen.insert(plan_with(&[change]).digest()), "{change:?}");
    }
    // the frozen set has a digest of its own: a rolling task does not move it
    let frozen = plan().frozen_digest();
    assert_eq!(
        plan_with(&[("What is B?", "What is B now?")]).frozen_digest(),
        frozen
    );
    assert_ne!(
        plan_with(&[("What is A?", "What is A now?")]).frozen_digest(),
        frozen
    );
}

/// A root with one domain, `dom-me`, opened as the owner.
fn root(dir: &Path) -> Memory {
    Memory::init(dir, "human:test:me", "dom-me").unwrap()
}

/// A root whose platform is `dom-plat`, beside the owner's `dom-me`.
fn platform_root(dir: &Path) -> Memory {
    Memory::init(dir, "human:test:me", "dom-me").unwrap();
    let path = dir.join(crate::GRANTS_FILE);
    let mut g = dm_policy::Grants::load(&path).unwrap();
    let d = g.domains["dom-me"].clone();
    g.domains.insert("dom-plat".into(), d);
    g.platform = Some("dom-plat".into());
    std::fs::write(&path, g.render().unwrap()).unwrap();
    Memory::open(dir).unwrap()
}

fn agent(dir: &Path) -> Memory {
    Memory::open(dir).unwrap().with_author("agent:claude")
}

fn started_as(plan: &Plan, task: &str, model: &str) -> TapeStart {
    TapeStart {
        harness: "claude".into(),
        model: Some(model.into()),
        prompt: plan.prompt_of(task),
        ..TapeStart::default()
    }
}

/// One run of `plan` that keeps the head of its tape; its session.
async fn run(dir: &Path, plan: &Plan, task: &str, seed: u32, arm: Arm) -> String {
    let m = agent(dir);
    m.start_evaluation(plan, (task, seed, arm), AS_OF, CLAUDE)
        .unwrap();
    m.tape_start(started_as(plan, task, "m-1")).await.unwrap();
    m.session_id().to_string()
}

fn outcome(session: &str, success: bool, cost: u64) -> Outcome {
    Outcome {
        session: session.to_string(),
        success,
        cost,
        judge: "regex".into(),
    }
}

#[test]
fn start_evaluation_is_refused_outside_the_plan_and_a_second_time() {
    let dir = tempfile::tempdir().unwrap();
    root(dir.path());
    let p = plan();
    let start = |plan: &Plan, run: (&str, u32, Arm), launched: Launched<'_>| {
        agent(dir.path()).start_evaluation(plan, run, AS_OF, launched)
    };
    denied(start(&p, ("c", 0, Arm::Full), CLAUDE), "no task c");
    denied(
        start(&p, ("a", 0, Arm::RawOnly), CLAUDE),
        "the arm raw-only",
    );
    denied(start(&p, ("a", 2, Arm::Full), CLAUDE), "2 is not one");
    let elsewhere = plan_with(&[("domain = \"dom-me\"", "domain = \"dom-else\"")]);
    denied(
        start(&elsewhere, ("a", 0, Arm::Full), CLAUDE),
        "is not granted",
    );
    let fixed = plan_with(&[(
        "judge = \"regex\"",
        "judge = \"regex\"\nharness = \"claude\"\nmodel = \"m-1\"",
    )]);
    let codex = Launched {
        harness: "codex",
        ..CLAUDE
    };
    denied(
        start(&fixed, ("a", 0, Arm::Full), codex),
        "fixes the harness",
    );
    for model in [Some("m-2"), None] {
        let other = Launched { model, ..CLAUDE };
        denied(
            start(&fixed, ("a", 0, Arm::Full), other),
            "fixes the harness",
        );
    }
    assert!(start(&fixed, ("a", 1, Arm::Full), CLAUDE).is_ok());
    // once for a session, and not after its settings were given
    let m = agent(dir.path());
    let first = m
        .start_evaluation(&p, ("a", 1, Arm::Full), AS_OF, CLAUDE)
        .unwrap();
    assert_eq!(m.evaluation(), Some(first));
    denied(
        m.start_evaluation(&p, ("b", 0, Arm::NoMemory), AS_OF, CLAUDE),
        "a run of an evaluation already",
    );
    assert_eq!(m.settings(), {
        let mut s = Arm::Full.settings();
        s.not_after = Some(AS_OF);
        s
    });
    let set = agent(dir.path());
    set.set_settings(Arm::Full.settings()).unwrap();
    denied(
        set.start_evaluation(&p, ("a", 0, Arm::NoMemory), AS_OF, CLAUDE),
        "set once",
    );
    assert_eq!(set.evaluation(), None);
}

#[tokio::test]
async fn a_tape_header_names_the_run_and_the_tape_is_in_the_plan_s_domain() {
    let dir = tempfile::tempdir().unwrap();
    let owner = platform_root(dir.path());
    for domain in ["dom-me", "dom-plat"] {
        let p = plan_with(&[("domain = \"dom-me\"", &format!("domain = \"{domain}\""))]);
        let session = run(dir.path(), &p, "b", 1, Arm::Full).await;
        let header = owner.tape(&session).await.unwrap().header;
        assert_eq!(header.domain, domain, "two domains are granted");
        assert_eq!(
            header.start.evaluation,
            Some(RunOf {
                plan: p.digest(),
                task: "b".into(),
                seed: 1,
                arm: Arm::Full,
            })
        );
        let mut settings = Arm::Full.settings();
        settings.not_after = Some(AS_OF);
        assert_eq!(header.start.settings, settings);
    }
    // a session that is no run keeps no tape here, where two domains are granted
    let plain = agent(dir.path());
    plain
        .tape_start(started_as(&plan(), "a", "m-1"))
        .await
        .unwrap();
    assert!(owner.tape(plain.session_id()).await.is_err());
}

#[tokio::test]
async fn a_tape_header_takes_the_run_from_the_session_and_not_from_the_caller() {
    let dir = tempfile::tempdir().unwrap();
    let owner = root(dir.path());
    let p = plan();
    let forged = RunOf {
        plan: p.digest(),
        task: "a".into(),
        seed: 0,
        arm: Arm::Full,
    };
    let plain = agent(dir.path());
    plain
        .tape_start(TapeStart {
            evaluation: Some(forged.clone()),
            ..started_as(&p, "a", "m-1")
        })
        .await
        .unwrap();
    let start = owner.tape(plain.session_id()).await.unwrap().header.start;
    assert_eq!(start.evaluation, None, "no run, whatever the caller said");
    let m = agent(dir.path());
    let own = m
        .start_evaluation(&p, ("b", 1, Arm::NoMemory), AS_OF, CLAUDE)
        .unwrap();
    // a view of the session is the same run, and so is a tape that began with a call
    let view = m.for_request().unwrap();
    assert_eq!(view.evaluation(), Some(own.clone()));
    view.tape_call("t0", &json!({}), &json!({}), false)
        .await
        .unwrap();
    let start = owner.tape(m.session_id()).await.unwrap().header.start;
    assert_eq!(start.evaluation, Some(own));
    // a header with no such key still reads, and a session that is no run writes none
    let old = r#"{"session":"s","domain":"dom-me","started_at":"2026-11-01T10:00:00Z","harness":"claude","dirty":false,"pinned":false}"#;
    let h: crate::tape::TapeHeader = serde_json::from_str(old).unwrap();
    assert_eq!(h.start.evaluation, None);
    assert_eq!(serde_json::to_string(&h).unwrap(), old);
}

#[tokio::test]
async fn score_is_refused_for_what_is_not_a_run_of_the_plan() {
    let dir = tempfile::tempdir().unwrap();
    let owner = root(dir.path());
    let p = plan();
    let good = run(dir.path(), &p, "a", 0, Arm::Full).await;
    let one = [outcome(&good, true, 100)];
    assert!(owner.score(&p, &one).await.is_ok());
    denied(agent(dir.path()).score(&p, &one).await, "the owner's");
    denied(agent(dir.path()).score(&p, &[]).await, "the owner's");
    denied(owner.score(&p, &[]).await, "one outcome or more");
    let no_tape = owner.score(&p, &[outcome("sess-none", true, 1)]).await;
    assert!(matches!(no_tape, Err(Error::NotFound(_))), "{no_tape:?}");
    // a session that was not started as a run
    let plain = agent(dir.path());
    plain.tape_start(started_as(&p, "a", "m-1")).await.unwrap();
    let not_a_run = [outcome(plain.session_id(), true, 1)];
    denied(owner.score(&p, &not_a_run).await, "names no evaluation");
    // a run of the plan with another margin
    let wider = plan_with(&[("margin = 0.1", "margin = 0.3")]);
    let under_wider = run(dir.path(), &wider, "a", 0, Arm::NoMemory).await;
    let mixed = [one[0].clone(), outcome(&under_wider, true, 1)];
    denied(owner.score(&p, &mixed).await, "started under another plan");
    // the same run twice, by one session or by two
    let twice = [one[0].clone(), one[0].clone()];
    denied(owner.score(&p, &twice).await, "two outcomes are the run");
    let again = run(dir.path(), &p, "a", 0, Arm::Full).await;
    let two = [one[0].clone(), outcome(&again, false, 1)];
    denied(owner.score(&p, &two).await, "two outcomes are the run");
    // another prompt than the task's, and no prompt
    for prompt in [Some("What is B?".to_string()), None] {
        let m = agent(dir.path());
        m.start_evaluation(&p, ("a", 1, Arm::Full), AS_OF, CLAUDE)
            .unwrap();
        let start = TapeStart {
            prompt,
            ..started_as(&p, "a", "m-1")
        };
        m.tape_start(start).await.unwrap();
        let other_prompt = [outcome(m.session_id(), true, 1)];
        denied(owner.score(&p, &other_prompt).await, "is not the prompt");
    }
}

#[tokio::test]
async fn score_is_refused_for_runs_on_two_models_or_not_on_the_plan_s() {
    let dir = tempfile::tempdir().unwrap();
    let owner = root(dir.path());
    let p = plan();
    let good = outcome(&run(dir.path(), &p, "a", 0, Arm::Full).await, true, 100);
    for (harness, model) in [
        ("claude", Some("m-2")),
        ("claude", None),
        ("codex", Some("m-1")),
    ] {
        let m = agent(dir.path());
        m.start_evaluation(&p, ("a", 0, Arm::NoMemory), AS_OF, CLAUDE)
            .unwrap();
        let start = TapeStart {
            harness: harness.into(),
            model: model.map(str::to_string),
            ..started_as(&p, "a", "m-1")
        };
        m.tape_start(start).await.unwrap();
        let two = [good.clone(), outcome(m.session_id(), true, 100)];
        denied(owner.score(&p, &two).await, "differ in harness or model");
    }
    // a plan that fixes the harness and the model, and a tape that names others
    for (fix, harness, model) in [
        ("harness = \"codex\"", "codex", Some("m-1")),
        ("model = \"m-2\"", "claude", Some("m-2")),
    ] {
        let fixed = plan_with(&[("judge = \"regex\"", &format!("judge = \"regex\"\n{fix}"))]);
        let m = agent(dir.path());
        let launched = Launched { harness, model };
        m.start_evaluation(&fixed, ("a", 0, Arm::Full), AS_OF, launched)
            .unwrap();
        m.tape_start(started_as(&fixed, "a", "m-1")).await.unwrap();
        let on_other = [outcome(m.session_id(), true, 100)];
        denied(owner.score(&fixed, &on_other).await, "not the plan's");
    }
}

#[tokio::test]
async fn score_reads_the_runs_from_their_tapes_and_pairs_them() {
    let dir = tempfile::tempdir().unwrap();
    let owner = root(dir.path());
    let p = plan();
    let mut outcomes = Vec::new();
    for task in ["a", "b"] {
        for seed in [0, 1] {
            let base = run(dir.path(), &p, task, seed, Arm::NoMemory).await;
            outcomes.push(outcome(&base, true, 1000));
            let full = run(dir.path(), &p, task, seed, Arm::Full).await;
            outcomes.push(outcome(&full, true, 600 + 100 * u64::from(seed)));
        }
    }
    let card = owner.score(&p, &outcomes).await.unwrap();
    assert_eq!(card.plan, "demo");
    assert_eq!(card.plan_digest, p.digest());
    assert_eq!(card.frozen_digest, p.frozen_digest());
    assert_eq!(
        (card.harness.as_str(), card.model.as_deref()),
        ("claude", Some("m-1"))
    );
    assert_eq!(card.snapshot.as_deref(), Some("2020-01-01T00:00:00Z"));
    assert_eq!(card.unpaired, 0);
    assert!(card.traps.iter().all(|t| t.held), "{:?}", card.traps);
    let full = &card.arms[1];
    assert_eq!(
        (full.arm, full.runs, full.passed, full.pairs, full.seeds),
        (Arm::Full, 4, 4, 4, 2)
    );
    assert_eq!(full.cost.mean, -350.0);
    assert_eq!(full.verdict, Some(Verdict::Pays));
    assert_eq!(card.arms[0].verdict, None, "the baseline");
    // the span ends at the last call of a tape, not before its start
    assert!(at(&card.ended).unwrap() >= at(&card.started).unwrap());
}

/// One run as the pure rules read it: started at `T`, a minute long, snapshot five
/// seconds before `T`.
fn fact(task: &str, seed: u32, arm: Arm, success: bool, cost: u64) -> Fact {
    Fact {
        run: Run {
            task: task.into(),
            seed,
            arm,
            success,
            cost,
        },
        harness: "claude".into(),
        model: Some("m-1".into()),
        judge: "regex".into(),
        started: T,
        ended: T + Duration::minutes(1),
        snapshot: Some(T - Duration::seconds(5)),
    }
}

/// Every task, seed and arm of `plan`, with the success and the cost that `of` gives.
fn grid(plan: &Plan, of: impl Fn(&str, u32, Arm) -> (bool, u64)) -> Vec<Fact> {
    let mut out = Vec::new();
    for task in &plan.tasks {
        for seed in 0..plan.seeds {
            for &arm in &plan.arms {
                let (success, cost) = of(&task.id, seed, arm);
                out.push(fact(&task.id, seed, arm, success, cost));
            }
        }
    }
    out
}

/// The full arm of the scorecard of `facts`.
fn full_of(plan: &Plan, facts: &[Fact]) -> ArmScore {
    let card = assemble(plan, facts, &[]).unwrap();
    assert!(card.arms[0].baseline && card.arms[0].finding.is_none());
    assert_eq!(card.arms[0].verdict, None);
    card.arms[1].clone()
}

/// The full arm cheaper, at 600 or 700 tokens against 1000.
fn cheaper(arm: Arm, seed: u32) -> u64 {
    if arm == Arm::Full {
        600 + 100 * u64::from(seed % 2)
    } else {
        1000
    }
}

const TEN: (&str, &str) = ("seeds = 2", "seeds = 10");

#[test]
fn an_arm_within_the_margin_and_cheaper_pays() {
    // one failure in twenty pairs: the low end of the difference is near -0.148
    let of = |task: &str, seed: u32, arm: Arm| {
        let fails = arm == Arm::Full && task == "a" && seed == 3;
        (!fails, cheaper(arm, seed))
    };
    let wide = plan_with(&[TEN, ("margin = 0.1", "margin = 0.2")]);
    let full = full_of(&wide, &grid(&wide, of));
    assert_eq!(
        (full.runs, full.passed, full.pairs, full.seeds),
        (20, 19, 20, 10)
    );
    assert_eq!(full.success.mean, -0.05);
    assert!(
        (full.success.low + 0.148).abs() < 0.001,
        "{:?}",
        full.success
    );
    assert!(full.cost.high < 0.0);
    assert_eq!(full.finding, Some(Finding::Pays));
    assert_eq!(full.verdict, Some(Verdict::Pays));
    // the same runs under the margin 0.1 are not shown to be within it
    let narrow = plan_with(&[TEN]);
    let full = full_of(&narrow, &grid(&narrow, of));
    assert_eq!(full.verdict, Some(Verdict::Unclear));
    // a low end at the margin itself is within it
    let at_margin = plan_with(&[TEN, ("margin = 0.1", "margin = 0")]);
    let level = |_: &str, seed: u32, arm: Arm| (true, cheaper(arm, seed));
    let full = full_of(&at_margin, &grid(&at_margin, level));
    assert_eq!(full.success.low, 0.0);
    assert_eq!(full.verdict, Some(Verdict::Pays));
}

#[test]
fn an_interval_is_the_mean_with_the_normal_approximation() {
    let p = plan();
    let runs = |arm: Arm, values: [(bool, u64); 4]| {
        let keys = [("a", 0), ("a", 1), ("b", 0), ("b", 1)];
        keys.into_iter()
            .zip(values)
            .map(move |((task, seed), (success, cost))| Run {
                task: task.into(),
                seed,
                arm,
                success,
                cost,
            })
            .collect::<Vec<Run>>()
    };
    let mut all = runs(Arm::NoMemory, [(true, 1000); 4]);
    all.extend(runs(
        Arm::Full,
        [(true, 900), (false, 700), (true, 800), (true, 800)],
    ));
    let full = compare(&p, &all).remove(1);
    assert_eq!(full.mean_cost, 800.0);
    assert_eq!(full.success.mean, -0.25);
    assert!((full.success.low + 0.74).abs() < 1e-6, "{:?}", full.success);
    assert!(
        (full.success.high - 0.24).abs() < 1e-6,
        "{:?}",
        full.success
    );
    assert_eq!(full.cost.mean, -200.0);
    assert!(
        (full.cost.low + 280.016_665).abs() < 1e-5,
        "{:?}",
        full.cost
    );
    assert!(
        (full.cost.high + 119.983_335).abs() < 1e-5,
        "{:?}",
        full.cost
    );
    assert_eq!(full.verdict, None, "compare gives no verdict");
    // one pair has no interval and decides nothing
    let one: Vec<Run> = all
        .iter()
        .filter(|r| r.task == "a" && r.seed == 0)
        .cloned()
        .collect();
    let full = compare(&p, &one).remove(1);
    assert_eq!(
        (full.cost.low, full.cost.mean, full.cost.high),
        (-100.0, -100.0, -100.0)
    );
    assert_eq!(full.finding, Some(Finding::Unclear));
}

#[test]
fn an_arm_that_is_cheaper_and_worse_fails() {
    let p = plan_with(&[TEN]);
    let of = |_: &str, seed: u32, arm: Arm| (arm != Arm::Full, cheaper(arm, seed));
    let full = full_of(&p, &grid(&p, of));
    assert!(full.cost.high < 0.0, "cheaper");
    assert_eq!(full.success.high, -1.0);
    assert_eq!(full.verdict, Some(Verdict::Fails));
    // worse by the margin exactly is not shown to be worse
    let all = plan_with(&[TEN, ("margin = 0.1", "margin = 1")]);
    let full = full_of(&all, &grid(&all, of));
    assert_eq!(full.verdict, Some(Verdict::Pays), "within a margin of 1");
}

#[test]
fn an_arm_that_is_better_pays_only_at_an_accepted_cost() {
    // better, at 1500 tokens against 1000
    let of = |_: &str, seed: u32, arm: Arm| {
        if arm == Arm::Full {
            (true, 1500 + u64::from(seed))
        } else {
            (false, 1000)
        }
    };
    let p = plan_with(&[TEN]);
    let full = full_of(&p, &grid(&p, of));
    assert!(full.success.low > 0.0 && full.cost.low > 0.0);
    assert_eq!(full.verdict, Some(Verdict::Unclear), "not accepted at 1.0");
    let accept = |factor: &str| {
        let to = format!("margin = 0.1\naccepted_cost = {factor}");
        let p = plan_with(&[TEN, ("margin = 0.1", &to)]);
        full_of(&p, &grid(&p, of)).verdict
    };
    assert_eq!(accept("1.4"), Some(Verdict::Unclear));
    assert_eq!(accept("2.0"), Some(Verdict::Pays));
    // no better and dearer fails
    let dearer = |_: &str, seed: u32, arm: Arm| (true, 3000 - 2 * cheaper(arm, seed));
    let full = full_of(&p, &grid(&p, dearer));
    assert!(full.cost.low > 0.0);
    assert_eq!(full.verdict, Some(Verdict::Fails));
    // no better and no dearer decides nothing
    let level = |_: &str, _: u32, _: Arm| (true, 1000);
    assert_eq!(
        full_of(&p, &grid(&p, level)).verdict,
        Some(Verdict::Unclear)
    );
}

#[test]
fn an_outcome_with_no_partner_in_the_baseline_is_left_out_and_counted() {
    let p = plan();
    let of = |_: &str, seed: u32, arm: Arm| (true, cheaper(arm, seed));
    let mut facts = grid(&p, of);
    facts.retain(|f| !(f.run.arm == Arm::NoMemory && f.run.task == "b" && f.run.seed == 1));
    let card = assemble(&p, &facts, &[]).unwrap();
    assert_eq!(card.unpaired, 1);
    let full = &card.arms[1];
    assert_eq!((full.runs, full.pairs), (4, 3));
    // the run of b at seed 1 cost 700: it is in the mean cost and in no difference
    assert_eq!(full.mean_cost, 650.0);
    assert!(
        (full.cost.mean + 366.666_667).abs() < 1e-5,
        "{:?}",
        full.cost
    );
}

fn inconclusive(names: &[&str]) -> Option<Verdict> {
    Some(Verdict::Inconclusive {
        traps: names.iter().map(|n| (*n).to_string()).collect(),
    })
}

fn open_traps(card: &Scorecard) -> Vec<&str> {
    assert_eq!(
        card.traps
            .iter()
            .map(|t| t.name.as_str())
            .collect::<Vec<_>>(),
        TRAPS
    );
    card.traps
        .iter()
        .filter(|t| !t.held)
        .map(|t| t.name.as_str())
        .collect()
}

fn pays(_: &str, seed: u32, arm: Arm) -> (bool, u64) {
    (true, cheaper(arm, seed))
}

#[test]
fn an_arm_with_pairs_from_one_seed_is_inconclusive_by_the_noise_trap() {
    let p = plan();
    let all = grid(&p, pays);
    assert!(open_traps(&assemble(&p, &all, &[]).unwrap()).is_empty());
    let one_seed: Vec<Fact> = all.iter().filter(|f| f.run.seed == 0).cloned().collect();
    let card = assemble(&p, &one_seed, &[]).unwrap();
    assert_eq!(open_traps(&card), ["noise"]);
    let full = &card.arms[1];
    assert_eq!((full.pairs, full.seeds), (2, 1));
    assert_eq!(full.finding, Some(Finding::Pays), "what the numbers give");
    assert_eq!(full.verdict, inconclusive(&["noise"]));
    // two seeds in the arm, and pairs from one: the baseline ran one seed
    let base_one: Vec<Fact> = all
        .iter()
        .filter(|f| f.run.arm == Arm::Full || f.run.seed == 0)
        .cloned()
        .collect();
    let card = assemble(&p, &base_one, &[]).unwrap();
    assert_eq!(open_traps(&card), ["noise"]);
    // an arm of the plan with no run is scored, and is open to noise
    let three = plan_with(&[("\"full\"]", "\"full\", \"t0-only\"]")]);
    let card = assemble(&three, &all, &[]).unwrap();
    assert_eq!(open_traps(&card), ["noise"]);
    assert_eq!(card.arms[1].verdict, inconclusive(&["noise"]));
    assert_eq!(card.arms[2].finding, Some(Finding::Unclear));
}

#[test]
fn the_survivorship_trap_needs_a_task_that_failed() {
    let p = plan_with(&[("stratum = \"failed\"\n", "")]);
    let card = assemble(&p, &grid(&p, pays), &[]).unwrap();
    assert_eq!(open_traps(&card), ["survivorship"]);
    assert_eq!(card.arms[1].finding, Some(Finding::Pays));
    assert_eq!(card.arms[1].verdict, inconclusive(&["survivorship"]));
}

#[test]
fn the_leakage_trap_needs_one_snapshot_that_is_not_after_the_first_run() {
    let p = plan();
    let with = |change: &dyn Fn(&mut Vec<Fact>)| {
        let mut facts = grid(&p, pays);
        change(&mut facts);
        let card = assemble(&p, &facts, &[]).unwrap();
        (open_traps(&card).join(","), card.snapshot)
    };
    let at_start = with(&|f| f.iter_mut().for_each(|f| f.snapshot = Some(T)));
    assert_eq!(
        at_start,
        (String::new(), Some("2026-11-02T10:00:00Z".into()))
    );
    // one run with another snapshot
    let two = with(&|f| f[3].snapshot = Some(T - Duration::seconds(6)));
    assert_eq!(two, ("leakage".into(), None));
    // one run with none
    assert_eq!(with(&|f| f[0].snapshot = None), ("leakage".into(), None));
    assert_eq!(with(&|f| f[5].snapshot = None), ("leakage".into(), None));
    // one snapshot, after the earliest start: the first run could read later writes
    let late = with(&|f| {
        f.iter_mut()
            .for_each(|f| f.snapshot = Some(T + Duration::seconds(1)));
        f.iter_mut()
            .skip(1)
            .for_each(|f| f.started = T + Duration::seconds(2));
    });
    assert_eq!(late.0, "leakage");
    assert_eq!(late.1.as_deref(), Some("2026-11-02T10:00:01Z"));
}

#[test]
fn the_judge_drift_trap_needs_the_plan_s_judge_on_every_outcome() {
    let p = plan();
    let mut facts = grid(&p, pays);
    facts[6].judge = "regex-2".into();
    let card = assemble(&p, &facts, &[]).unwrap();
    assert_eq!(open_traps(&card), ["judge-drift"]);
    assert_eq!(card.arms[1].verdict, inconclusive(&["judge-drift"]));
}

#[test]
fn the_distribution_shift_trap_needs_a_frozen_task_and_a_rolling_task() {
    let rolling = plan_with(&[("set = \"frozen\"\n", "")]);
    let frozen = plan_with(&[("expect = \"B\"", "expect = \"B\"\nset = \"frozen\"")]);
    for p in [rolling, frozen] {
        let card = assemble(&p, &grid(&p, pays), &[]).unwrap();
        assert_eq!(open_traps(&card), ["distribution-shift"]);
    }
    // two traps open are both named
    let p = plan_with(&[("set = \"frozen\"\n", ""), ("stratum = \"failed\"\n", "")]);
    let card = assemble(&p, &grid(&p, pays), &[]).unwrap();
    assert_eq!(
        card.arms[1].verdict,
        inconclusive(&["survivorship", "distribution-shift"])
    );
}

fn window(model: &str, from: OffsetDateTime, until: Option<OffsetDateTime>) -> Window {
    Window {
        destination: "llmao".into(),
        model: model.into(),
        from,
        until,
        detectors: vec!["flip_rate".into()],
        why: "the answers moved".into(),
        closed_why: until.map(|_| "pinned".to_string()),
    }
}

#[test]
fn a_scorecard_whose_runs_spanned_a_drift_window_is_quarantined() {
    let p = plan();
    // the runs span T to T + 3 min: the last run made its last call then
    let mut facts = grid(&p, pays);
    facts[7].started = T + Duration::minutes(2);
    facts[7].ended = T + Duration::minutes(3);
    let end = T + Duration::minutes(3);
    let hour = Duration::hours(1);
    let second = Duration::seconds(1);
    let quarantined = |w: Window| {
        let other = window("m-0", T - hour, None);
        let card = assemble(&p, &facts, &[other, w.clone()]).unwrap();
        assert_eq!(
            (card.started.as_str(), card.ended.as_str()),
            ("2026-11-02T10:00:00Z", "2026-11-02T10:03:00Z")
        );
        match &card.quarantined {
            None => {
                assert_eq!(card.arms[1].verdict, Some(Verdict::Pays));
                false
            }
            Some(by) => {
                assert_eq!(*by, w);
                let verdict = Verdict::Quarantined {
                    destination: "llmao".into(),
                    model: w.model.clone(),
                };
                assert_eq!(card.arms[1].verdict, Some(verdict));
                assert_eq!(card.arms[0].verdict, None, "the baseline");
                assert_eq!(card.arms[1].finding, Some(Finding::Pays));
                true
            }
        }
    };
    assert!(quarantined(window("m-1", T - hour, None)), "open");
    assert!(
        quarantined(window("m-1", T - hour, Some(T))),
        "closed at the start"
    );
    assert!(quarantined(window("m-1", end, None)), "opened at the end");
    assert!(
        quarantined(window("m-1", T + second, Some(T + 2 * second))),
        "inside"
    );
    assert!(
        !quarantined(window("m-1", T - hour, Some(T - second))),
        "closed before"
    );
    assert!(
        !quarantined(window("m-1", end + second, None)),
        "opened after"
    );
    assert!(!quarantined(window("m-2", T - hour, None)), "another model");
    // a window comes before an open trap
    let one_seed: Vec<Fact> = facts.iter().filter(|f| f.run.seed == 0).cloned().collect();
    let card = assemble(&p, &one_seed, &[window("m-1", T - hour, None)]).unwrap();
    assert_eq!(open_traps(&card), ["noise"]);
    assert!(matches!(
        card.arms[1].verdict,
        Some(Verdict::Quarantined { .. })
    ));
    // runs that name no model are in no window
    let mut unnamed = facts.clone();
    unnamed.iter_mut().for_each(|f| f.model = None);
    let card = assemble(&p, &unnamed, &[window("m-1", T - hour, None)]).unwrap();
    assert_eq!(card.quarantined, None);
}

fn card() -> Scorecard {
    let p = plan();
    assemble(&p, &grid(&p, pays), &[]).unwrap()
}

#[tokio::test]
async fn keep_scorecard_needs_a_platform_domain_and_the_owner() {
    let dir = tempfile::tempdir().unwrap();
    let owner = root(dir.path());
    denied(owner.keep_scorecard(&card()).await, "the grants name none");
    assert!(owner.scorecards(10).await.unwrap().is_empty());
    let with = tempfile::tempdir().unwrap();
    platform_root(with.path());
    denied(
        agent(with.path()).keep_scorecard(&card()).await,
        "the owner keeps a scorecard",
    );
}

#[tokio::test]
async fn a_kept_scorecard_is_a_decision_of_the_platform_and_is_read_back() {
    let dir = tempfile::tempdir().unwrap();
    let owner = platform_root(dir.path());
    let first = card();
    let written = owner.keep_scorecard(&first).await.unwrap();
    assert_eq!(written.domain, "dom-plat");
    let entry = owner.get(&written.id).await.unwrap();
    assert_eq!(entry.front.entry_type, EntryType::Decision);
    assert_eq!(entry.front.domain, "dom-plat");
    assert_eq!(entry.front.trust_tier, TrustTier::Raw);
    assert_eq!(
        entry.front.title.as_deref(),
        Some("Scorecard demo: m-1, 2026-11-02")
    );
    assert!(entry
        .body
        .contains("Against no-memory, margin 0.1: full pays."));
    assert!(entry
        .body
        .contains("| full | 4 | 4 | 650 | 4 | 0.000 (0.000 to 0.000) | -350.000 ("));
    assert!(entry.body.contains("- `leakage`, held: Every run read"));
    let kept = owner.scorecards(10).await.unwrap();
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].entry, written.id);
    assert_eq!(kept[0].author, "human:test:me");
    assert_eq!(kept[0].scorecard, first, "the scorecard that was kept");
    // the platform's listing shows the artifact, and the launcher loads no scorecard
    let listed = owner.platform().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(
        (
            listed[0].artifact.kind.as_str(),
            listed[0].artifact.name.as_str()
        ),
        ("scorecard", "demo")
    );
    assert!(!listed[0].loaded);
    assert!(listed[0]
        .why_not
        .as_deref()
        .unwrap()
        .contains("a scorecard is a record"));
    assert!(owner
        .platform_artifact(KIND, "demo")
        .await
        .unwrap()
        .is_none());
    // a decision that is no scorecard is not one, and the newest scorecard is first
    owner
        .write(WriteRequest {
            domain: Some("dom-plat".into()),
            entry_type: EntryType::Decision,
            body: "# Scorecards stay\n\nWe keep them.\n".into(),
            ..WriteRequest::default()
        })
        .await
        .unwrap();
    let mut second = first.clone();
    second.unpaired = 7;
    let newer = owner.keep_scorecard(&second).await.unwrap();
    let kept = owner.scorecards(10).await.unwrap();
    let ids: Vec<&str> = kept.iter().map(|k| k.entry.as_str()).collect();
    assert_eq!(ids, [newer.id.as_str(), written.id.as_str()]);
    let newest = owner.scorecards(1).await.unwrap();
    assert_eq!(newest.len(), 1);
    assert_eq!(newest[0].scorecard.unpaired, 7);
}

#[tokio::test]
async fn scorecards_are_served_under_the_session_s_read_filter() {
    let dir = tempfile::tempdir().unwrap();
    let owner = platform_root(dir.path());
    let written = owner.keep_scorecard(&card()).await.unwrap();
    // a session that reads the platform's domain: its label rises as a get raises it
    let reader = agent(dir.path());
    let before = reader.label();
    assert_eq!(reader.scorecards(10).await.unwrap().len(), 1);
    assert!(reader.label() >= written.classification);
    assert!(before <= reader.label());
    // a session with no memory access, and one whose snapshot is before the entry
    let closed = agent(dir.path());
    closed.set_settings(Arm::NoMemory.settings()).unwrap();
    assert!(closed.scorecards(10).await.unwrap().is_empty());
    let earlier = agent(dir.path());
    let mut settings = Arm::Full.settings();
    settings.not_after = Some(AS_OF);
    earlier.set_settings(settings).unwrap();
    assert!(earlier.scorecards(10).await.unwrap().is_empty());
}
