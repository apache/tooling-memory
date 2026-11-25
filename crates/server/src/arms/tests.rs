//! The rules of the session settings. Every time here is fixed: no test compares two
//! readings of the clock.

use super::*;
use crate::recall::{Budget, Deputy};
use crate::transcripts::Turn;
use crate::{prefetch, WriteRequest};
use dm_core::{Entry, Frontmatter, Label};
use std::collections::BTreeMap;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use time::macros::datetime;
use time::Duration;

/// The snapshot the tests read at.
const SNAPSHOT: OffsetDateTime = datetime!(2026-10-01 10:00:00.5 UTC);
const TIERS: [TrustTier; 3] = [TrustTier::Raw, TrustTier::Curated, TrustTier::Verified];
const LABELS: [Label; 3] = [Label::Public, Label::Internal, Label::Restricted];

/// A root with two domains, `dom-a` and `dom-b`, opened as the owner.
fn root(dir: &Path) -> Memory {
    Memory::init(dir, "human:test:me", "dom-a").unwrap();
    let path = dir.join(crate::GRANTS_FILE);
    let mut g = dm_policy::Grants::load(&path).unwrap();
    let d = g.domains["dom-a"].clone();
    g.domains.insert("dom-b".into(), d);
    std::fs::write(&path, g.render().unwrap()).unwrap();
    Memory::open(dir).unwrap()
}

/// A session of an agent on the root, under `settings` when given.
fn agent(dir: &Path, settings: Option<Settings>) -> Memory {
    let m = Memory::open(dir).unwrap().with_author("agent:test");
    if let Some(s) = settings {
        m.set_settings(s).unwrap();
    }
    m
}

/// One entry to put in the record as it is given.
struct Put<'a> {
    id: &'a str,
    kind: EntryType,
    tier: TrustTier,
    created: OffsetDateTime,
    cites: Vec<String>,
    label: Label,
}

impl<'a> Put<'a> {
    fn new(id: &'a str, kind: EntryType) -> Put<'a> {
        Put {
            id,
            kind,
            tier: TrustTier::Raw,
            created: SNAPSHOT - Duration::hours(1),
            cites: Vec::new(),
            label: Label::Internal,
        }
    }

    fn label(mut self, label: Label) -> Put<'a> {
        self.label = label;
        self
    }

    fn cites(mut self, cite: &str) -> Put<'a> {
        self.cites.push(cite.into());
        self
    }

    fn tier(mut self, tier: TrustTier) -> Put<'a> {
        self.tier = tier;
        self
    }

    fn created(mut self, created: OffsetDateTime) -> Put<'a> {
        self.created = created;
        self
    }

    /// Put it in `dom-a`, in the record and the index, with a body every search for
    /// "backoff" finds.
    async fn into(self, m: &Memory) {
        let entry = Entry {
            front: Frontmatter {
                id: self.id.into(),
                entry_type: self.kind,
                domain: "dom-a".into(),
                author: "human:test:me".into(),
                created: self.created,
                cites: self.cites,
                confidence: None,
                trust_tier: self.tier,
                review_by: None,
                supersedes: None,
                policy_context: vec![],
                classification: self.label,
                status: None,
                origin: None,
                session: None,
                reach: dm_core::Reach::Fleet,
                task: None,
                summary: None,
                title: Some(format!("Backoff note {}", self.id)),
            },
            body: format!("The backoff is exponential, says {}.\n", self.id),
        };
        let put = m.put_logged(&entry).await.unwrap();
        m.feed_from("dom-a", put).await.unwrap();
    }
}

/// The ids a search and a list return, sorted; the two must agree.
async fn found(m: &Memory) -> Vec<String> {
    let mut searched: Vec<String> = m
        .search("backoff", None, 50)
        .await
        .unwrap()
        .into_iter()
        .map(|h| h.id)
        .collect();
    let mut listed: Vec<String> = m
        .list(None, None, 50)
        .await
        .unwrap()
        .into_iter()
        .map(|h| h.id)
        .collect();
    searched.sort_unstable();
    listed.sort_unstable();
    assert_eq!(searched, listed, "search and list see the same entries");
    listed
}

/// The entry files a projection of `m` holds, by id, sorted.
async fn projected(m: &Memory, dir: &Path) -> Vec<String> {
    let done = m.project_into(dir).await.unwrap();
    let mut ids = Vec::new();
    for domain in ["dom-a", "dom-b"] {
        for f in std::fs::read_dir(dir.join(domain)).unwrap().flatten() {
            if let Some(id) = crate::projection::id_of(&f.path()) {
                ids.push(id.to_string());
            }
        }
    }
    ids.sort_unstable();
    assert_eq!(done.listed, ids.len(), "what is listed is what is written");
    ids
}

fn not_found(r: &Result<Entry, Error>) -> bool {
    matches!(r, Err(Error::NotFound(_)))
}

/// A deputy that counts the calls made of it.
struct Counting(AtomicUsize);

impl Deputy for Counting {
    fn model(&self) -> String {
        "counting".into()
    }
    fn complete<'a>(
        &'a self,
        _prompt: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok("{\"brief\": \"Nothing.\", \"citations\": []}".to_string()) })
    }
}

#[test]
fn an_arm_has_a_name_a_meaning_and_fixed_settings() {
    let names: Vec<&str> = Arm::ALL.iter().map(|a| a.name()).collect();
    assert_eq!(
        names,
        [
            "no-memory",
            "transcript-replay",
            "t0-only",
            "raw-only",
            "full"
        ],
        "the paper's order"
    );
    for arm in Arm::ALL {
        assert_eq!(arm.to_string().parse::<Arm>().unwrap(), arm);
        assert_eq!(serde_json::to_value(arm).unwrap(), arm.name());
        assert!(!arm.stands_for().is_empty());
        assert_eq!(Arm::of(&arm.settings()), Some(arm));
        let mut at_a_snapshot = arm.settings();
        at_a_snapshot.not_after = Some(SNAPSHOT);
        assert_eq!(Arm::of(&at_a_snapshot), Some(arm), "whatever the snapshot");
        let text = serde_json::to_string(&at_a_snapshot).unwrap();
        let back: Settings = serde_json::from_str(&text).unwrap();
        assert_eq!(back, at_a_snapshot, "{text}");
    }
    assert_eq!(Arm::Full.settings(), Settings::default());
    assert!("curated-only".parse::<Arm>().is_err());
    let odd = Settings {
        prefetch: false,
        ..Settings::default()
    };
    assert_eq!(Arm::of(&odd), None);
    let none: Settings = serde_json::from_str("{}").unwrap();
    assert_eq!(
        none,
        Settings::default(),
        "a field left out is production's"
    );
}

#[test]
fn the_arms_differ_in_what_the_paper_says_they_differ_in() {
    let s = |a: Arm| a.settings();
    assert!(!s(Arm::NoMemory).retrieves());
    assert_eq!(s(Arm::NoMemory).t0_served(), T0Contents::Off);
    assert!(!s(Arm::NoMemory).transcripts);
    assert_eq!(
        s(Arm::TranscriptReplay),
        Settings {
            transcripts: true,
            ..s(Arm::NoMemory)
        }
    );
    assert!(!s(Arm::T0Only).retrieves());
    assert_eq!(s(Arm::T0Only).t0_served(), T0Contents::Full);
    assert!(!s(Arm::T0Only).prefetches());
    assert!(s(Arm::RawOnly).retrieves() && s(Arm::RawOnly).prefetches());
    assert_eq!(s(Arm::RawOnly).max_tier, TrustTier::Raw);
    assert_eq!(s(Arm::RawOnly).t0_served(), T0Contents::Digest);
    // memory access off takes T0 with it, whatever the T0 switch says
    let off = Settings {
        memory: false,
        ..Settings::default()
    };
    assert_eq!(off.t0_served(), T0Contents::Off);
    assert!(!off.prefetches());
}

#[test]
fn flags_give_settings_only_when_one_is_given() {
    assert_eq!(from_flags(None, None).unwrap(), None);
    assert_eq!(
        from_flags(Some(""), Some("")).unwrap(),
        None,
        "empty is unset"
    );
    assert_eq!(
        from_flags(Some("raw-only"), None).unwrap(),
        Some(Arm::RawOnly.settings())
    );
    let at = from_flags(None, Some("2026-10-01T10:00:00.5Z")).unwrap();
    assert_eq!(
        at,
        Some(Settings {
            not_after: Some(SNAPSHOT),
            ..Settings::default()
        })
    );
    let both = from_flags(Some("no-memory"), Some("2026-10-01T12:00:00.5+02:00")).unwrap();
    assert_eq!(both.unwrap().not_after, Some(SNAPSHOT));
    assert!(from_flags(Some("everything"), None).is_err());
    assert!(from_flags(None, Some("yesterday")).is_err());
}

/// The filters a grant and a policy may give, to narrow from.
fn granted_filters() -> Vec<ReadFilter> {
    let wide = ReadFilter::for_domains(["dom-a", "dom-b"], Label::Restricted);
    let mut narrow = ReadFilter::for_domains(["dom-a"], Label::Internal);
    narrow.min_tier = TrustTier::Curated;
    narrow.max_tier = TrustTier::Curated;
    narrow.not_after = Some(SNAPSHOT - Duration::hours(1));
    let mut later = wide.clone();
    later.not_after = Some(SNAPSHOT + Duration::hours(1));
    vec![wide, narrow, later]
}

fn every_settings() -> Vec<Settings> {
    let mut all: Vec<Settings> = Arm::ALL.iter().map(|a| a.settings()).collect();
    for arm in Arm::ALL {
        all.push(Settings {
            not_after: Some(SNAPSHOT),
            ..arm.settings()
        });
    }
    all
}

/// Whether `f` admits no entry, at any time, that `granted` does not.
fn admits_no_more(f: &ReadFilter, granted: &ReadFilter) -> bool {
    let times = [
        SNAPSHOT,
        SNAPSHOT - Duration::hours(2),
        SNAPSHOT + Duration::hours(2),
    ];
    let mut holds = times
        .iter()
        .all(|at| granted.admits_at(*at) || !f.admits_at(*at));
    for domain in ["dom-a", "dom-b", "dom-c"] {
        for label in LABELS {
            for tier in TIERS {
                let was = granted.admits(domain, label, tier, true);
                holds &= was || !f.admits(domain, label, tier, true);
            }
        }
    }
    holds
}

/// Rule 1: an arm reads no more than the grants and the policy give.
#[test]
fn no_settings_admit_what_the_granted_filter_does_not() {
    for granted in granted_filters() {
        for s in every_settings() {
            let f = s.apply(granted.clone());
            assert!(f.handling <= granted.handling, "the handling is not raised");
            assert_eq!(f.domains, granted.domains, "no domain is added");
            assert!(f.min_tier >= granted.min_tier, "the floor is not lowered");
            assert!(f.max_tier <= granted.max_tier, "the ceiling is not raised");
            assert!(!f.include_inactive, "inactive entries stay out");
            assert!(admits_no_more(&f, &granted), "{s:?} over {granted:?}");
            assert!(admits_no_more(&s.bounds(granted.clone()), &granted));
        }
    }
}

#[test]
fn the_bounds_take_the_lower_tier_and_the_earlier_snapshot() {
    let granted = &granted_filters()[1];
    let raw = Arm::RawOnly.settings().bounds(granted.clone());
    assert_eq!(raw.max_tier, TrustTier::Raw);
    assert_eq!(raw.not_after, granted.not_after);
    let at = Settings {
        not_after: Some(SNAPSHOT),
        ..Settings::default()
    };
    assert_eq!(at.bounds(granted.clone()).not_after, granted.not_after);
    let later = &granted_filters()[2];
    assert_eq!(at.bounds(later.clone()).not_after, Some(SNAPSHOT));
    let wide = &granted_filters()[0];
    assert_eq!(at.bounds(wide.clone()).not_after, Some(SNAPSHOT));
    assert_eq!(at.apply(wide.clone()).max_tier, TrustTier::Verified);
    assert_eq!(Settings::default().apply(wide.clone()), *wide);
    assert!(Arm::NoMemory.settings().apply(wide.clone()).is_closed());
    assert!(Arm::T0Only.settings().apply(wide.clone()).is_closed());
    assert!(!Arm::T0Only.settings().bounds(wide.clone()).is_closed());
}

/// What a session that does not retrieve gets from every read below T0: nothing.
async fn reads_nothing(dir: &Path, m: &Memory, known: &str) {
    assert!(found(m).await.is_empty());
    assert!(not_found(&m.get(known).await), "an entry it could read");
    spends_no_deputy_call(m).await;
    let plan = prefetch::Plan::default();
    assert!(m.prefetch("backoff", &plan).await.unwrap().hits.is_empty());
    assert!(m.read_with(known, 5).await.unwrap().is_empty());
    assert!(projected(m, &dir.join("projection")).await.is_empty());
    serves_no_other_read(m, known).await;
    assert_eq!(m.label(), Label::Public, "nothing was read");
}

/// A recall of a session that does not retrieve: an empty pack, and no deputy call.
async fn spends_no_deputy_call(m: &Memory) {
    let counting = Arc::new(Counting(AtomicUsize::new(0)));
    let deputy: Arc<dyn Deputy> = counting.clone();
    m.set_deputy(&deputy);
    let asked = m.recall("what is the backoff", None, None, Budget::default());
    let r = asked.await.unwrap();
    let calls = counting.0.load(Ordering::SeqCst);
    assert_eq!(calls, 0, "no deputy call is spent");
    assert!(r.pack.pieces.is_empty());
    assert!(r.citations.is_empty());
    assert!(r.brief.is_none());
}

/// The reads beside search, get and recall serve nothing either.
async fn serves_no_other_read(m: &Memory, known: &str) {
    let pack = m.search_sections("backoff", None, 10).await.unwrap();
    assert!(pack.pieces.is_empty());
    assert!(m.list_archive(None, 10).await.unwrap().is_empty());
    assert!(m.archived(known).await.is_err());
    assert!(m.promotions(known).await.is_err());
}

/// Rules 2 and 4: with memory access off, no read path serves an entry.
#[tokio::test]
async fn with_memory_access_off_no_read_serves_an_entry() {
    let dir = tempfile::tempdir().unwrap();
    let owner = root(dir.path());
    Put::new("mem-a-fact", EntryType::Fact).into(&owner).await;
    Put::new("mem-a-handoff", EntryType::Handoff)
        .into(&owner)
        .await;
    let open = agent(dir.path(), None);
    assert_eq!(found(&open).await, ["mem-a-fact", "mem-a-handoff"]);
    assert_eq!(
        projected(&open, &dir.path().join("open")).await,
        ["mem-a-fact", "mem-a-handoff"]
    );
    for arm in [Arm::NoMemory, Arm::TranscriptReplay] {
        let m = agent(dir.path(), Some(arm.settings()));
        reads_nothing(dir.path(), &m, "mem-a-fact").await;
        // a domain the session may name gets an empty answer, and one it may not is
        // still refused
        assert!(m.list(Some("dom-b"), None, 10).await.unwrap().is_empty());
        assert!(m.list(Some("dom-c"), None, 10).await.is_err());
        let elsewhere = m.recall("backoff", Some("dom-c"), None, Budget::default());
        assert!(elsewhere.await.is_err());
    }
}

/// Rule 3: at the depth T0, nothing below T0 is served, and T0 is what it was.
#[tokio::test]
async fn at_the_depth_t0_only_t0_is_served_and_it_is_the_same_t0() {
    let dir = tempfile::tempdir().unwrap();
    let owner = root(dir.path());
    Put::new("mem-a-fact", EntryType::Fact).into(&owner).await;
    Put::new("mem-a-handoff", EntryType::Handoff)
        .into(&owner)
        .await;
    Put::new("mem-a-decision", EntryType::Decision)
        .tier(TrustTier::Verified)
        .into(&owner)
        .await;
    let whole = agent(dir.path(), None).t0(Some("dom-a")).await.unwrap();
    for held in ["mem-a-handoff", "mem-a-decision", "3 active entries"] {
        assert!(whole.contains(held), "{held} in:\n{whole}");
    }
    let m = agent(dir.path(), Some(Arm::T0Only.settings()));
    reads_nothing(dir.path(), &m, "mem-a-fact").await;
    assert_eq!(m.t0(Some("dom-a")).await.unwrap(), whole);
    assert_eq!(m.label(), Label::Internal, "T0 was read");
}

/// A task in `dom-a` that waits for a session: filed and admitted by the owner.
async fn open_task(owner: &Memory, title: &str) -> String {
    let spec = dm_ledger::TaskSpec {
        domain: "dom-a".into(),
        title: title.into(),
        body: "Do it.".into(),
        ..dm_ledger::TaskSpec::default()
    };
    let filed = owner.create_task(spec).await.unwrap().task;
    if filed.status == dm_ledger::Status::Proposed {
        owner.admit(&filed.id).await.unwrap();
    }
    filed.id
}

const RULE: &str = "# Decisions are superseded by a person\n\n```policy\nid = \"pol-L2-031\"\nlayer = 2\nrule = \"Decisions are superseded by a person, not an agent.\"\nintent = \"An agent rewrote a standing decision mid-task.\"\n```\n\n```rego\npackage amem.rules\nimport rego.v1\ndeny contains {\"policy\": \"pol-L2-031\", \"reason\": \"an agent superseded a decision\"} if {\n\tinput.principal.class == \"agent\"\n\tinput.write.supersedes_type == \"decision\"\n}\n```\n";

/// A rule of the record that a person verified, so the policy digest holds a line.
async fn verified_rule(dir: &Path, owner: &Memory) {
    let rule = WriteRequest {
        domain: Some("dom-a".into()),
        entry_type: EntryType::Policy,
        body: RULE.into(),
        ..WriteRequest::default()
    };
    let id = owner.write(rule).await.unwrap().id;
    let proposer = agent(dir, None);
    let proposed = proposer.promote(&id, TrustTier::Verified, "reviewed").await;
    let crate::Promotion::Proposed { task } = proposed.unwrap() else {
        panic!("a promotion to verified is a proposal")
    };
    owner.approve(&task.id).await.unwrap();
}

/// The policy digest of a T0, from its heading to the next.
fn digest_of(t0: &str) -> String {
    let from = t0.find("## Policy digest").unwrap();
    let to = t0.find("## Pointers").unwrap();
    t0[from..to].to_string()
}

/// Rule 5: T0 off holds nothing of the record; the digest holds the policy and the
/// tasks that wait, and no entry.
#[tokio::test]
async fn t0_off_says_nothing_of_the_record_and_the_digest_names_no_entry() {
    let dir = tempfile::tempdir().unwrap();
    let owner = root(dir.path());
    for (id, kind) in [
        ("mem-a-handoff", EntryType::Handoff),
        ("mem-a-decision", EntryType::Decision),
        ("mem-a-convention", EntryType::Convention),
    ] {
        Put::new(id, kind).into(&owner).await;
    }
    // a restricted handoff: a T0 that lists it raises the session label to restricted
    Put::new("mem-a-secret", EntryType::Handoff)
        .label(Label::Restricted)
        .into(&owner)
        .await;
    verified_rule(dir.path(), &owner).await;
    open_task(&owner, "Tidy the importer").await;
    let open = agent(dir.path(), None);
    let whole = open.t0(Some("dom-a")).await.unwrap();
    assert!(whole.contains("mem-a-secret"), "{whole}");
    assert_eq!(open.label(), Label::Restricted);
    assert!(
        whole.contains("pol-L2-031 (L2, enforced): Decisions are"),
        "{whole}"
    );

    let off = agent(dir.path(), Some(Arm::NoMemory.settings()));
    let reads_before = crate::reads::all(dir.path()).unwrap().len();
    let text = off.t0(Some("dom-a")).await.unwrap();
    assert_eq!(text, T0_OFF);
    for told in [
        "mem-",
        "Backoff",
        "active entries",
        "tasks waiting",
        "dom-a",
        "pol-L2",
    ] {
        assert!(!text.contains(told), "{told} in: {text}");
    }
    assert!(
        text.contains("session setting `t0`"),
        "it names the setting"
    );
    assert_eq!(crate::reads::all(dir.path()).unwrap().len(), reads_before);
    assert_eq!(off.label(), Label::Public);
    assert!(off.t0(Some("dom-c")).await.is_err(), "an ungranted domain");

    let digest = Settings {
        t0: T0Contents::Digest,
        ..Settings::default()
    };
    let m = agent(dir.path(), Some(digest));
    let text = m.t0(Some("dom-a")).await.unwrap();
    assert_eq!(
        digest_of(&text),
        digest_of(&whole),
        "the same policy digest"
    );
    assert_eq!(m.label(), Label::Internal, "it read the rule and no entry");
    assert!(text.contains("- 1 tasks waiting on the ledger"), "{text}");
    for left_out in [
        "mem-a-",
        "Backoff",
        "active entries",
        "## Now",
        "## Standing",
    ] {
        assert!(whole.contains(left_out), "{left_out} is in the whole T0");
        assert!(!text.contains(left_out), "{left_out} in:\n{text}");
    }
    let listed = m.list(None, None, 50).await.unwrap();
    assert_eq!(listed.len(), 5, "the digest leaves retrieval on");
    // the raw-only arm reads no verified entry, and its digest still gives the rule:
    // the policy in force is read as it is now
    let raw = agent(dir.path(), Some(Arm::RawOnly.settings()));
    let text = raw.t0(Some("dom-a")).await.unwrap();
    assert_eq!(digest_of(&text), digest_of(&whole));
}

/// The text of a rule is an entry's text: a refusal gives it to a session that reads
/// the policy digest, and withholds it from one whose memory access is off.
#[tokio::test]
async fn a_refusal_gives_the_rule_s_reason_only_where_memory_access_is_on() {
    let dir = tempfile::tempdir().unwrap();
    let owner = root(dir.path());
    Put::new("mem-a-decision", EntryType::Decision)
        .into(&owner)
        .await;
    verified_rule(dir.path(), &owner).await;
    let replace = || WriteRequest {
        domain: Some("dom-a".into()),
        entry_type: EntryType::Decision,
        supersedes: Some("mem-a-decision".into()),
        body: "Another decision.\n".into(),
        ..WriteRequest::default()
    };
    for (settings, told) in [
        (Settings::default(), true),
        (Arm::RawOnly.settings(), true),
        (Arm::T0Only.settings(), true),
        (Arm::NoMemory.settings(), false),
    ] {
        let m = agent(dir.path(), Some(settings.clone()));
        let refused = m.write(replace()).await.unwrap_err().to_string();
        assert!(
            refused.contains("pol-L2-031"),
            "the rule is named: {refused}"
        );
        let reason = refused.contains("an agent superseded a decision");
        assert_eq!(reason, told, "{settings:?}: {refused}");
    }
}

/// Rule 6: with the tier ceiling at raw, nothing above raw is read.
#[tokio::test]
async fn with_the_ceiling_at_raw_only_raw_entries_are_read() {
    let dir = tempfile::tempdir().unwrap();
    let owner = root(dir.path());
    Put::new("mem-a-fact", EntryType::Fact).into(&owner).await;
    Put::new("mem-a-handoff", EntryType::Handoff)
        .into(&owner)
        .await;
    Put::new("mem-a-curated", EntryType::Decision)
        .tier(TrustTier::Curated)
        .into(&owner)
        .await;
    Put::new("mem-a-verified", EntryType::Decision)
        .tier(TrustTier::Verified)
        .into(&owner)
        .await;
    let raw = Settings {
        max_tier: TrustTier::Raw,
        ..Settings::default()
    };
    let m = agent(dir.path(), Some(raw));
    assert_eq!(found(&m).await, ["mem-a-fact", "mem-a-handoff"]);
    assert!(m.get("mem-a-fact").await.is_ok());
    assert!(m.get("mem-a-handoff").await.is_ok());
    assert!(not_found(&m.get("mem-a-curated").await));
    assert!(not_found(&m.get("mem-a-verified").await));
    let t0 = m.t0(Some("dom-a")).await.unwrap();
    assert!(t0.contains("mem-a-handoff"), "{t0}");
    assert!(t0.contains("2 active entries"), "{t0}");
    assert!(!t0.contains("mem-a-curated") && !t0.contains("mem-a-verified"));
    assert_eq!(
        projected(&m, &dir.path().join("projection")).await,
        ["mem-a-fact", "mem-a-handoff"]
    );
    // the arm: the same retrieval, and a T0 that is the digest
    let arm = agent(dir.path(), Some(Arm::RawOnly.settings()));
    assert_eq!(found(&arm).await, ["mem-a-fact", "mem-a-handoff"]);
    let t0 = arm.t0(Some("dom-a")).await.unwrap();
    assert!(
        !t0.contains("mem-a-") && t0.contains("tasks waiting"),
        "{t0}"
    );
}

/// Rule 7: with a snapshot, nothing created after it is read, and what was created at
/// it is.
#[tokio::test]
async fn with_a_snapshot_nothing_created_after_it_is_read() {
    let dir = tempfile::tempdir().unwrap();
    let owner = root(dir.path());
    Put::new("mem-a-1-before", EntryType::Handoff)
        .into(&owner)
        .await;
    Put::new("mem-a-2-at", EntryType::Handoff)
        .created(SNAPSHOT)
        .into(&owner)
        .await;
    Put::new("mem-a-3-after", EntryType::Handoff)
        .created(SNAPSHOT + Duration::nanoseconds(1))
        .into(&owner)
        .await;
    let all = ["mem-a-1-before", "mem-a-2-at", "mem-a-3-after"];
    assert_eq!(found(&agent(dir.path(), None)).await, all);
    let at = Settings {
        not_after: Some(SNAPSHOT),
        ..Settings::default()
    };
    let m = agent(dir.path(), Some(at));
    let inside = ["mem-a-1-before", "mem-a-2-at"];
    assert_eq!(found(&m).await, inside);
    assert!(m.get("mem-a-2-at").await.is_ok());
    assert!(not_found(&m.get("mem-a-3-after").await));
    let t0 = m.t0(Some("dom-a")).await.unwrap();
    assert!(t0.contains("mem-a-1-before") && t0.contains("mem-a-2-at"));
    assert!(t0.contains("2 active entries"), "{t0}");
    assert!(!t0.contains("mem-a-3-after"), "{t0}");
    let files = projected(&m, &dir.path().join("projection")).await;
    assert_eq!(files, inside);
}

/// A live transcript of two turns in `dom-a`, the second of them `text`.
fn transcript(session: &str, label: Label, kept_at: OffsetDateTime, text: &str) -> Transcript {
    let turn = |turn: usize, role: &str, text: String| Turn {
        turn,
        role: role.into(),
        text,
    };
    Transcript {
        session: session.into(),
        domain: "dom-a".into(),
        harness: "claude".into(),
        origin: format!("{session}.jsonl"),
        classification: label,
        kept_at,
        retain_until: kept_at + Duration::days(30),
        state: State::Live,
        ended_at: None,
        turn_count: 2,
        turns: vec![
            turn(1, "user", format!("Asked in {session}.")),
            turn(2, "assistant", text.into()),
        ],
        excerpts: BTreeMap::new(),
        observed_by: crate::transcripts::OBSERVED_BY_HARNESS.into(),
    }
}

/// The ids that the `read` events of `m`'s session name in the log of `domain`.
fn reads_in_log(dir: &Path, domain: &str, m: &Memory) -> Vec<String> {
    let session = m.session_id.clone();
    crate::log::read(dir, domain, 0)
        .unwrap()
        .into_iter()
        .filter(|e| e.session.as_deref() == Some(session.as_str()))
        .filter_map(|e| match e.change {
            crate::log::Change::Read { ids } => Some(ids),
            _ => None,
        })
        .flatten()
        .collect()
}

async fn keep(m: &Memory, session: &str, label: Label, kept_at: OffsetDateTime, text: &str) {
    let t = transcript(session, label, kept_at, text);
    m.put_transcript(&t, None).await.unwrap();
}

fn replay_at(snapshot: OffsetDateTime) -> Settings {
    Settings {
        not_after: Some(snapshot),
        ..Arm::TranscriptReplay.settings()
    }
}

/// An agent's session that handles internal at most.
fn capped(dir: &Path, settings: Option<Settings>) -> Memory {
    let m = Memory::open(dir)
        .unwrap()
        .with_author("agent:test")
        .with_handling_cap(Label::Internal);
    if let Some(s) = settings {
        m.set_settings(s).unwrap();
    }
    m
}

/// Rule 8: what the transcript-replay arm puts in front of a prompt.
#[tokio::test]
async fn transcripts_are_replayed_under_the_handling_and_the_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let owner = root(dir.path());
    let (old, new) = (SNAPSHOT - Duration::hours(2), SNAPSHOT - Duration::hours(1));
    let late = SNAPSHOT + Duration::nanoseconds(1);
    keep(&owner, "sess-old", Label::Public, old, "Answer one.").await;
    keep(&owner, "sess-new", Label::Internal, new, "Answer two.").await;
    keep(&owner, "sess-top", Label::Restricted, new, "Answer three.").await;
    keep(&owner, "sess-late", Label::Public, late, "Answer four.").await;
    let mut swept = transcript("sess-swept", Label::Public, old, "Answer five.");
    swept.state = State::Swept;
    owner.put_transcript(&swept, None).await.unwrap();

    let off = capped(dir.path(), None);
    let none = off.transcript_context(None, TRANSCRIPT_BUDGET_TOKENS);
    assert_eq!(none.await.unwrap(), "", "the setting is off");
    assert_eq!(off.label(), Label::Public);

    let m = capped(dir.path(), Some(replay_at(SNAPSHOT)));
    let text = m.transcript_context(None, TRANSCRIPT_BUDGET_TOKENS);
    let text = text.await.unwrap();
    for served in ["Asked in sess-new.", "Answer two.", "Answer one."] {
        assert!(text.contains(served), "{served} in:\n{text}");
    }
    let (first, second) = (text.find("sess-new"), text.find("sess-old"));
    assert!(first < second, "newest first");
    for left_out in ["Answer three.", "Answer four.", "Answer five.", "sess-top"] {
        assert!(!text.contains(left_out), "{left_out} in:\n{text}");
    }
    assert!(approx_tokens(&text) <= TRANSCRIPT_BUDGET_TOKENS);
    assert_eq!(m.label(), Label::Internal, "the label of what was served");
    assert!(found(&m).await.is_empty(), "the arm still reads no entry");
    let logged = crate::reads::all(dir.path()).unwrap();
    let lines = logged.iter().filter(|r| r.tool == "transcripts").count();
    assert_eq!(lines, 1, "one line in the read log");
    // each transcript served is a read event in the log of its domain, and no other is
    let mut read = reads_in_log(dir.path(), "dom-a", &m);
    read.sort_unstable();
    assert_eq!(read, ["sess-new", "sess-old"]);
    // a snapshot at the older transcript leaves the newer one out, and the label lower
    let earlier = capped(dir.path(), Some(replay_at(old)));
    let text = earlier.transcript_context(Some("dom-a"), 8000).await;
    let text = text.unwrap();
    assert!(text.contains("sess-old") && !text.contains("sess-new"));
    assert_eq!(earlier.label(), Label::Public);
    let other = earlier.transcript_context(Some("dom-b"), 8000).await;
    assert_eq!(other.unwrap(), "", "no transcript in the other domain");
}

/// What a harness is given: the prompt alone, or the replayed transcripts and then the
/// prompt. A launch and a re-run of its tape both take the text from here.
#[tokio::test]
async fn the_prompt_given_has_the_replayed_transcripts_in_front() {
    let dir = tempfile::tempdir().unwrap();
    let owner = root(dir.path());
    let kept_at = SNAPSHOT - Duration::hours(1);
    keep(&owner, "sess-new", Label::Internal, kept_at, "Answer two.").await;
    let off = capped(dir.path(), None);
    let alone = off.prompt_given("What now?", TRANSCRIPT_BUDGET_TOKENS);
    assert_eq!(alone.await.unwrap(), "What now?");
    let m = capped(dir.path(), Some(replay_at(SNAPSHOT)));
    let replayed = m.transcript_context(None, TRANSCRIPT_BUDGET_TOKENS);
    let replayed = replayed.await.unwrap();
    assert!(replayed.contains("Answer two."));
    let given = m.prompt_given("What now?", TRANSCRIPT_BUDGET_TOKENS);
    assert_eq!(given.await.unwrap(), format!("{replayed}\nWhat now?"));
}

#[tokio::test]
async fn a_transcript_that_does_not_fit_the_budget_is_left_out_whole() {
    let dir = tempfile::tempdir().unwrap();
    let owner = root(dir.path());
    let old = SNAPSHOT - Duration::hours(2);
    let long = "word ".repeat(400);
    // one word of 70 KiB: few tokens, and more bytes than one argument should hold
    let wide = "x".repeat(70 * 1024);
    keep(&owner, "sess-short", Label::Public, old, "A short answer.").await;
    let newer = old + Duration::minutes(1);
    keep(&owner, "sess-long", Label::Internal, newer, &long).await;
    let newest = old + Duration::minutes(2);
    keep(&owner, "sess-wide", Label::Internal, newest, &wide).await;
    let m = agent(dir.path(), Some(replay_at(SNAPSHOT)));
    let text = m.transcript_context(None, 300).await.unwrap();
    assert!(text.contains("A short answer."), "{text}");
    assert!(!text.contains("sess-long") && !text.contains("sess-wide"));
    assert!(approx_tokens(&text) <= 300, "{}", approx_tokens(&text));
    assert_eq!(m.label(), Label::Public, "what was left out raised nothing");
    let roomy = agent(dir.path(), Some(replay_at(SNAPSHOT)));
    let text = roomy.transcript_context(None, 8000).await.unwrap();
    assert!(text.contains("sess-long") && text.contains("sess-short"));
    assert!(!text.contains("sess-wide"), "over the byte bound");
    assert!(text.len() < TRANSCRIPT_MAX_BYTES);
    assert!(approx_tokens(&text) <= 8000);
    let nothing = agent(dir.path(), Some(replay_at(SNAPSHOT)));
    assert_eq!(nothing.transcript_context(None, 10).await.unwrap(), "");
    assert_eq!(nothing.label(), Label::Public);
}

/// Rule 9: the settings are set once, a view carries them, and a policy loaded again
/// does not lift them.
#[tokio::test]
async fn settings_are_set_once_and_stay_with_the_session() {
    let dir = tempfile::tempdir().unwrap();
    let owner = root(dir.path());
    Put::new("mem-a-fact", EntryType::Fact).into(&owner).await;
    let m = agent(dir.path(), None);
    assert_eq!(m.settings(), Settings::default());
    assert_eq!(Arm::of(&m.settings()), Some(Arm::Full));
    m.set_settings(Arm::NoMemory.settings()).unwrap();
    let again = m.set_settings(Settings::default());
    assert!(matches!(&again, Err(Error::Denied(why)) if why.contains("set once")));
    assert_eq!(m.settings(), Arm::NoMemory.settings(), "the first stand");
    assert!(found(&m).await.is_empty());

    let view = m.for_request().unwrap();
    assert_eq!(view.settings(), Arm::NoMemory.settings());
    assert!(found(&view).await.is_empty());
    assert!(not_found(&view.get("mem-a-fact").await));
    assert_eq!(view.t0(Some("dom-a")).await.unwrap(), T0_OFF);

    m.reload_policy().await.unwrap();
    assert!(
        found(&m).await.is_empty(),
        "after the policy is loaded again"
    );
    assert!(not_found(&m.get("mem-a-fact").await));
    // a view of a T0-only session renders the T0 its session does
    let t0_only = agent(dir.path(), Some(Arm::T0Only.settings()));
    let whole = agent(dir.path(), None).t0(Some("dom-a")).await.unwrap();
    let view = t0_only.for_request().unwrap();
    assert_eq!(view.t0(Some("dom-a")).await.unwrap(), whole);
    assert!(found(&view).await.is_empty());
}

#[test]
fn a_retrieval_tool_is_refused_in_words_that_name_the_setting() {
    let dir = tempfile::tempdir().unwrap();
    root(dir.path());
    let open = agent(dir.path(), None);
    let off = agent(dir.path(), Some(Arm::NoMemory.settings()));
    let t0_only = agent(dir.path(), Some(Arm::T0Only.settings()));
    let raw = agent(dir.path(), Some(Arm::RawOnly.settings()));
    for tool in RETRIEVAL_TOOLS {
        assert!(open.admits_tool(tool).is_ok());
        assert!(raw.admits_tool(tool).is_ok());
        let why = off.admits_tool(tool).unwrap_err().to_string();
        assert!(why.contains("memory access is off"), "{why}");
        assert!(why.contains(&format!("`{tool}`")), "{why}");
        let why = t0_only.admits_tool(tool).unwrap_err().to_string();
        assert!(
            why.contains("disclosure depth of this session is T0"),
            "{why}"
        );
    }
    for tool in [
        "write",
        "t0",
        "settings",
        "create_task",
        "claim_next",
        "release",
    ] {
        assert!(off.admits_tool(tool).is_ok(), "{tool}");
        assert!(t0_only.admits_tool(tool).is_ok(), "{tool}");
    }
}

/// Writes and the task tools are not governed by the settings: a session that does not
/// read its own handoff back still writes it, and releases its task with it.
#[tokio::test]
async fn a_session_that_reads_nothing_still_writes_and_hands_off() {
    let dir = tempfile::tempdir().unwrap();
    let owner = root(dir.path());
    let at_a_snapshot = Settings {
        not_after: Some(SNAPSHOT),
        ..Settings::default()
    };
    for settings in [
        Arm::NoMemory.settings(),
        Arm::T0Only.settings(),
        at_a_snapshot,
    ] {
        let id = open_task(&owner, "Tidy").await;
        let m = agent(dir.path(), Some(settings));
        let claim = m.claim(&id, Duration::minutes(5)).await.unwrap();
        let handoff = m
            .write(WriteRequest {
                domain: Some("dom-a".into()),
                entry_type: EntryType::Handoff,
                body: "# Stopped\n\nThe importer is half tidy.\n".into(),
                ..WriteRequest::default()
            })
            .await
            .unwrap()
            .id;
        assert!(
            not_found(&m.get(&handoff).await),
            "its settings leave it out"
        );
        let released = m.release(&id, None, Some(&handoff)).await.unwrap();
        assert_eq!(released.handoff.as_deref(), Some(handoff.as_str()));
        let late = m.handoff_after(&id, Some(claim.generation), &handoff).await;
        assert!(late.is_ok(), "{late:?}");
        let missing = m.release(&id, None, Some("mem-a-nothere")).await;
        assert!(missing.is_err(), "a handoff that is not there");
        assert_eq!(m.written(), (1, 1));
        assert!(agent(dir.path(), None).get(&handoff).await.is_ok());
    }
}

async fn hand_off(m: &Memory, supersedes: Option<&str>) -> String {
    let written = m.write(WriteRequest {
        domain: Some("dom-a".into()),
        entry_type: EntryType::Handoff,
        body: "# Stopped\n\nThe importer is half tidy.\n".into(),
        supersedes: supersedes.map(str::to_string),
        ..WriteRequest::default()
    });
    written.await.unwrap().id
}

/// The handoff that a task tool names is checked, and it is not served: no line in
/// the read log and no entry in the manifest of a decision context names it. A
/// session whose settings leave the successor of a handoff out does not learn its id.
#[tokio::test]
async fn a_handoff_named_to_a_task_tool_is_checked_and_not_served() {
    let dir = tempfile::tempdir().unwrap();
    let owner = root(dir.path());
    let writer = agent(dir.path(), None);
    let old = hand_off(&writer, None).await;
    let new = hand_off(&writer, Some(&old)).await;
    let id = open_task(&owner, "Tidy").await;
    let m = agent(dir.path(), Some(Arm::NoMemory.settings()));
    m.claim(&id, Duration::minutes(5)).await.unwrap();
    let moved = m.release(&id, None, Some(&old)).await;
    assert!(matches!(moved, Err(Error::NotFound(_))), "{moved:?}");
    let own = hand_off(&m, None).await;
    let before = m.label();
    m.release(&id, None, Some(&own)).await.unwrap();
    assert!(m.label() >= before, "the label does not fall");
    assert!(m.reads.take_returned().is_empty(), "nothing was served");
    let logged = crate::reads::all(dir.path()).unwrap();
    assert_eq!(logged.iter().filter(|r| r.tool == "get").count(), 0);
    // the handoff is read on behalf of the release: the log of the domain says so
    assert!(reads_in_log(dir.path(), "dom-a", &m).contains(&own));
    // a session that reads the successor is told where the handoff moved
    let other = open_task(&owner, "Sweep").await;
    let full = agent(dir.path(), None);
    full.claim(&other, Duration::minutes(5)).await.unwrap();
    let moved = full.release(&other, None, Some(&old)).await;
    assert!(
        matches!(&moved, Err(Error::Moved { to, .. }) if *to == new),
        "{moved:?}"
    );
}

/// An erasure reads the record, where the index does not hold the snapshot: an entry
/// that the snapshot leaves out is not found, and no sign-off task tells that it is
/// there. A session that reads the entry is told that it needs an approval.
#[tokio::test]
async fn an_erasure_does_not_find_an_entry_after_the_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let owner = root(dir.path());
    Put::new("mem-a-late", EntryType::Fact)
        .created(SNAPSHOT + Duration::hours(1))
        .into(&owner)
        .await;
    let at_a_snapshot = Settings {
        not_after: Some(SNAPSHOT),
        ..Settings::default()
    };
    let m = agent(dir.path(), Some(at_a_snapshot));
    let erased = m.erase("mem-a-late", crate::Reason::Mistaken).await;
    assert!(matches!(erased, Err(Error::NotFound(_))), "{erased:?}");
    let now = agent(dir.path(), None);
    let erased = now.erase("mem-a-late", crate::Reason::Mistaken).await;
    assert!(matches!(erased, Err(Error::Denied(_))), "{erased:?}");
}

/// An export is a read: settings that read no entry export none, and a snapshot
/// bounds what an export holds.
#[tokio::test]
async fn an_export_holds_no_more_than_the_settings_read() {
    let dir = tempfile::tempdir().unwrap();
    let owner = root(dir.path());
    Put::new("mem-a-fact", EntryType::Fact).into(&owner).await;
    Put::new("mem-a-late", EntryType::Fact)
        .created(SNAPSHOT + Duration::hours(1))
        .into(&owner)
        .await;
    for arm in [Arm::NoMemory, Arm::T0Only] {
        let m = agent(dir.path(), Some(arm.settings()));
        let out = tempfile::tempdir().unwrap();
        let exported = m.export(Some("dom-a"), Label::Internal, out.path()).await;
        let why = exported.map(|e| e.manifest.entries.len());
        assert!(
            matches!(&why, Err(Error::Bundle(crate::bundle::Error::Invalid(w))) if w.contains("read no entry")),
            "{arm}: {why:?}"
        );
        assert_eq!(m.label(), Label::Public, "{arm}");
    }
    let ids_of = |settings: Option<Settings>| {
        let m = agent(dir.path(), settings);
        async move {
            let out = tempfile::tempdir().unwrap();
            let exported = m.export(Some("dom-a"), Label::Internal, out.path());
            let mut ids: Vec<String> = exported
                .await
                .unwrap()
                .manifest
                .entries
                .into_iter()
                .map(|e| e.id)
                .collect();
            ids.sort_unstable();
            ids
        }
    };
    assert_eq!(ids_of(None).await, ["mem-a-fact", "mem-a-late"]);
    let at_a_snapshot = Settings {
        not_after: Some(SNAPSHOT),
        ..Settings::default()
    };
    assert_eq!(ids_of(Some(at_a_snapshot)).await, ["mem-a-fact"]);
}

/// The prefetch switch alone: nothing is pushed, and retrieval stays.
#[tokio::test]
async fn with_prefetch_off_nothing_is_pushed_and_retrieval_stays() {
    let dir = tempfile::tempdir().unwrap();
    let owner = root(dir.path());
    Put::new("mem-a-fact", EntryType::Fact).into(&owner).await;
    Put::new("mem-a-partner", EntryType::Fact)
        .into(&owner)
        .await;
    // an earlier session searched the two up together, often enough to pair them
    let log = crate::reads::ReadLog::new(dir.path(), "sess-earlier");
    let pair = ["mem-a-fact".to_string(), "mem-a-partner".to_string()];
    for _ in 0..prefetch::Plan::default().together {
        log.served_ids("search", Some("q"), 2, 10, false, None, &pair);
    }
    let plan = prefetch::Plan::default();
    let on = agent(dir.path(), None);
    assert_eq!(on.prefetch("backoff", &plan).await.unwrap().hits.len(), 2);
    let with = on.read_with("mem-a-fact", 5).await.unwrap();
    assert_eq!(with.len(), 1, "the partner rides on the fetch");
    let off = Settings {
        prefetch: false,
        ..Settings::default()
    };
    let m = agent(dir.path(), Some(off));
    assert!(m.prefetch("backoff", &plan).await.unwrap().hits.is_empty());
    assert!(m.read_with("mem-a-fact", 5).await.unwrap().is_empty());
    assert_eq!(m.label(), Label::Public, "nothing was pushed");
    assert_eq!(found(&m).await.len(), 2, "retrieval is not the prefetch");
}

/// A turn is served by citation. With a snapshot, the citing entry and the transcript
/// must both be inside it.
#[tokio::test]
async fn with_a_snapshot_a_turn_is_served_only_from_inside_it() {
    let dir = tempfile::tempdir().unwrap();
    let owner = root(dir.path());
    let old = SNAPSHOT - Duration::hours(2);
    let late = SNAPSHOT + Duration::hours(1);
    keep(&owner, "sess-old", Label::Internal, old, "Answer one.").await;
    keep(&owner, "sess-late", Label::Internal, late, "Answer two.").await;
    // cited from inside the snapshot, and kept inside it
    Put::new("mem-a-early", EntryType::Fact)
        .cites("transcript:sess-old#1")
        .cites("transcript:sess-late#2")
        .into(&owner)
        .await;
    // cited only from outside the snapshot
    Put::new("mem-a-late", EntryType::Fact)
        .created(late)
        .cites("transcript:sess-old#2")
        .into(&owner)
        .await;
    let open = agent(dir.path(), None);
    for (session, turn) in [("sess-old", 1), ("sess-old", 2), ("sess-late", 2)] {
        let served = open.transcript_turn(session, turn).await;
        assert!(served.is_ok(), "{session}#{turn}: {served:?}");
    }
    assert!(open.transcript("sess-late").await.is_ok());
    let at = Settings {
        not_after: Some(SNAPSHOT),
        ..Settings::default()
    };
    let m = agent(dir.path(), Some(at));
    let inside = m.transcript_turn("sess-old", 1).await.unwrap();
    assert_eq!(inside.cited_by, ["mem-a-early"]);
    let cited_late = m.transcript_turn("sess-old", 2).await;
    assert!(
        cited_late.is_err(),
        "its only citation is after the snapshot"
    );
    let kept_late = m.transcript_turn("sess-late", 2).await;
    assert!(
        kept_late.is_err(),
        "the transcript was kept after the snapshot"
    );
    assert!(m.transcript("sess-late").await.is_err());
    let shown = m.transcript("sess-old").await.unwrap();
    let cited: Vec<usize> = shown.cited.keys().copied().collect();
    assert_eq!(cited, [1], "the citation after the snapshot is not shown");
}

const REVIEW: &str = "# Review a change\n\n```platform task review-change\npurpose = \"Read a change against the decisions it touches.\"\ntools = []\nscope = [\"dom-a\"]\nacceptance = [\"The handoff names each decision the change touches.\"]\noutputs = [\"handoff\"]\n```\n";

/// Filing a task is a write: a session that reads nothing still files one under a task
/// definition that the grants and the policy let it read.
#[tokio::test]
async fn a_session_that_reads_nothing_still_files_a_task_under_a_definition() {
    let dir = tempfile::tempdir().unwrap();
    root(dir.path());
    let path = dir.path().join(crate::GRANTS_FILE);
    let mut g = dm_policy::Grants::load(&path).unwrap();
    g.platform = Some("dom-b".into());
    std::fs::write(&path, g.render().unwrap()).unwrap();
    let owner = Memory::open(dir.path()).unwrap();
    let definition = WriteRequest {
        domain: Some("dom-b".into()),
        entry_type: EntryType::Convention,
        body: REVIEW.into(),
        ..WriteRequest::default()
    };
    let id = owner.write(definition).await.unwrap().id;
    // as a person other than its proposer would leave it
    let mut e = owner.store.get("dom-b", &id).await.unwrap().unwrap();
    e.front.trust_tier = TrustTier::Verified;
    owner.store.put(&e).await.unwrap();
    for arm in [Arm::NoMemory, Arm::T0Only, Arm::RawOnly, Arm::Full] {
        let m = agent(dir.path(), Some(arm.settings()));
        let spec = dm_ledger::TaskSpec {
            domain: "dom-a".into(),
            title: "Review the importer".into(),
            body: "Do it.".into(),
            definition: Some("review-change".into()),
            ..dm_ledger::TaskSpec::default()
        };
        let filed = m.create_task(spec).await;
        let filed = filed.unwrap_or_else(|e| panic!("{arm}: {e}")).task;
        let named = format!("review-change@{id}");
        assert_eq!(filed.spec.definition.as_deref(), Some(named.as_str()));
        assert_eq!(filed.spec.acceptance.len(), 1, "{arm}");
    }
}
