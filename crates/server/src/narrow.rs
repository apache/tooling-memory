//! Tests of a search that the caller narrows ([`Memory::search_with`],
//! [`Memory::search_sections_with`], [`Memory::pack_narrowed`]): the narrowing is inside
//! the session's filter, each hit has a snippet, and the label and the read log take what
//! was served.

use crate::recall::{Budget, Depth, Via};
use crate::testing::agents_read;
use crate::{reads, Memory, Narrow, WriteRequest};
use dm_core::{EntryType, Label, TrustTier};
use dm_index::Hit;
use std::path::Path;
use time::OffsetDateTime;

const QUERY: &str = "import job";
/// A word that only the restricted entry of [`shelf`] holds.
const SECRET: &str = "passphrase";

fn agent(dir: &Path, name: &str) -> Memory {
    Memory::open(dir).unwrap().with_author(name)
}

async fn wrote(m: &Memory, kind: EntryType, label: Label, body: &str, cites: &[&str]) -> String {
    m.write(WriteRequest {
        body: body.into(),
        entry_type: kind,
        classification: Some(label),
        cites: cites.iter().map(ToString::to_string).collect(),
        ..WriteRequest::default()
    })
    .await
    .unwrap()
    .id
}

/// The entries of [`shelf`], by what each one is.
struct Shelf {
    owner: Memory,
    /// A raw fact of `agent:a`.
    raw: String,
    /// A curated decision of `agent:a (model: x)`. It cites `raw`.
    decision: String,
    /// A raw fact of `agent:ab`, created a year ago.
    old: String,
    /// A restricted decision of the owner.
    secret: String,
}

impl Shelf {
    fn open(&self) -> Vec<&str> {
        let mut ids = vec![&*self.raw, &*self.decision, &*self.old];
        ids.sort_unstable();
        ids
    }
}

/// A root with four entries that a search for [`QUERY`] finds.
async fn shelf(dir: &Path) -> Shelf {
    use EntryType::{Decision, Fact};
    let owner = Memory::init(dir, "human:test:me", "dom-me").unwrap();
    let a = agent(dir, "agent:a");
    let body = "# Retry\n\nThe import job retries with backoff after a timeout.\n";
    let raw = wrote(&a, Fact, Label::Public, body, &[]).await;
    let a_x = agent(dir, "agent:a (model: x)");
    let body = "# Night run\n\nThe import job runs at night.\n";
    let decision = wrote(&a_x, Decision, Label::Public, body, &[&raw]).await;
    owner
        .promote(&decision, TrustTier::Curated, "Checked.")
        .await
        .unwrap();
    let ab = agent(dir, "agent:ab");
    let body = "# Host\n\nThe import job runs on the staging host.\n";
    let old = wrote(&ab, Fact, Label::Public, body, &[]).await;
    let mut e = owner.store.get("dom-me", &old).await.unwrap().unwrap();
    e.front.created -= time::Duration::days(365);
    let put = owner.put_logged(&e).await.unwrap();
    owner.feed_from("dom-me", put).await.unwrap();
    let body = format!("# Key\n\nThe import job reads the {SECRET} from the vault.\n");
    let secret = wrote(&owner, Decision, Label::Restricted, &body, &[]).await;
    Shelf {
        owner,
        raw,
        decision,
        old,
        secret,
    }
}

fn each_narrow() -> Vec<Narrow> {
    vec![
        Narrow::default(),
        Narrow {
            entry_type: Some(EntryType::Decision),
            ..Narrow::default()
        },
        Narrow {
            min_tier: Some(TrustTier::Raw),
            ..Narrow::default()
        },
        Narrow {
            min_tier: Some(TrustTier::Curated),
            ..Narrow::default()
        },
        Narrow {
            author: Some("agent:a".into()),
            ..Narrow::default()
        },
        Narrow {
            author: Some("human:test:me".into()),
            ..Narrow::default()
        },
        Narrow {
            since: Some(OffsetDateTime::now_utc() - time::Duration::days(1)),
            ..Narrow::default()
        },
    ]
}

fn ids(hits: &[Hit]) -> Vec<&str> {
    let mut ids: Vec<&str> = hits.iter().map(|h| h.id.as_str()).collect();
    ids.sort_unstable();
    ids
}

fn sorted(mut ids: Vec<&str>) -> Vec<&str> {
    ids.sort_unstable();
    ids
}

#[tokio::test]
async fn a_search_that_a_session_narrows_gives_the_entries_that_it_names_each_with_a_snippet() {
    let dir = tempfile::tempdir().unwrap();
    let s = shelf(dir.path()).await;
    let reader = agent(dir.path(), "agent:r").with_handling_cap(Label::Internal);
    let wide = reader.search(QUERY, None, 10).await.unwrap();
    assert_eq!(ids(&wide), s.open());
    let narrows = each_narrow();
    let want = [
        s.open(),
        vec![&*s.decision],
        s.open(),
        vec![&*s.decision],
        sorted(vec![&*s.raw, &*s.decision]),
        vec![],
        sorted(vec![&*s.raw, &*s.decision]),
    ];
    for (n, want) in narrows.iter().zip(want) {
        let hits = reader.search_with(QUERY, None, n, 10).await.unwrap();
        assert_eq!(ids(&hits), want, "{n:?}");
        for h in &hits {
            assert!(wide.iter().any(|w| w.id == h.id), "{n:?} only narrows");
            let snippet = h.snippet.as_deref().unwrap_or_default();
            assert!(snippet.contains("import job"), "{snippet:?}");
        }
    }
    // the best hit that passes, not the best hit and then nothing
    let first = reader.search(QUERY, None, 1).await.unwrap();
    for n in &narrows[1..] {
        let all = reader.search_with(QUERY, None, n, 10).await.unwrap();
        let one = reader.search_with(QUERY, None, n, 1).await.unwrap();
        assert_eq!(one.first().map(|h| &h.id), all.first().map(|h| &h.id));
    }
    let old = Narrow {
        author: Some("agent:ab".into()),
        ..Narrow::default()
    };
    assert_ne!(
        first[0].id, s.old,
        "the entry of agent:ab is not the best hit"
    );
    let one = reader.search_with(QUERY, None, &old, 1).await.unwrap();
    assert_eq!(ids(&one), [&*s.old]);
    // a listed hit has no snippet
    let listed = reader.list(None, None, 10).await.unwrap();
    assert!(!listed.is_empty() && listed.iter().all(|h| h.snippet.is_none()));
}

#[tokio::test]
async fn a_session_held_to_curated_entries_gets_no_raw_entry_when_it_asks_for_raw_ones() {
    let dir = tempfile::tempdir().unwrap();
    let s = shelf(dir.path()).await;
    let at_least = |tier| Narrow {
        min_tier: Some(tier),
        ..Narrow::default()
    };
    let free = agent(dir.path(), "agent:r").with_handling_cap(Label::Internal);
    let (curated, raw) = (at_least(TrustTier::Curated), at_least(TrustTier::Raw));
    let hits = free.search_with(QUERY, None, &curated, 10).await.unwrap();
    assert_eq!(ids(&hits), [&*s.decision]);
    let hits = free.search_with(QUERY, None, &raw, 10).await.unwrap();
    assert!(hits.iter().any(|h| h.id == s.raw));

    agents_read(dir.path(), "\"min_tier\": \"curated\"");
    let held = agent(dir.path(), "agent:r");
    for n in [Narrow::default(), at_least(TrustTier::Raw)] {
        let hits = held.search_with(QUERY, None, &n, 10).await.unwrap();
        assert!(hits.iter().any(|h| h.id == s.decision), "{n:?}");
        let raw: Vec<&Hit> = hits
            .iter()
            .filter(|h| h.trust_tier == TrustTier::Raw)
            .collect();
        assert!(raw.is_empty(), "{n:?}: {raw:?}");
        let pack = held.search_sections_with(QUERY, None, &n, 10).await;
        let pack = pack.unwrap();
        assert!(pack.pieces.iter().all(|p| p.trust_tier != TrustTier::Raw));
    }
}

#[tokio::test]
async fn an_entry_above_the_handling_of_a_session_gives_no_hit_and_no_snippet_however_narrowed() {
    let dir = tempfile::tempdir().unwrap();
    let s = shelf(dir.path()).await;
    // the owner handles it, and the snippet of its hit has the word
    let found = s.owner.search(SECRET, None, 10).await.unwrap();
    assert_eq!(ids(&found), [&*s.secret]);
    assert!(found[0].snippet.as_deref().unwrap().contains(SECRET));
    assert_eq!(s.owner.label(), Label::Restricted);

    let capped = agent(dir.path(), "agent:low").with_handling_cap(Label::Internal);
    for n in each_narrow() {
        for query in [QUERY, SECRET, "passphrase vault import"] {
            let hits = capped.search_with(query, None, &n, 10).await.unwrap();
            assert!(hits.iter().all(|h| h.id != s.secret), "{n:?}");
            let shown = |h: &Hit| h.snippet.as_deref().is_some_and(|t| t.contains(SECRET));
            assert!(!hits.iter().any(shown), "{n:?}");
            let pack = capped.search_sections_with(query, None, &n, 10).await;
            assert!(pack.unwrap().pieces.iter().all(|p| p.id != s.secret));
        }
    }
    assert_eq!(capped.label(), Label::Public);
}

/// The `search` lines that the session of `m` left in the read log.
fn searches(dir: &Path, m: &Memory) -> Vec<reads::Read> {
    let all = reads::all(dir).unwrap().into_iter();
    all.filter(|r| r.tool == "search" && r.session == m.session_id())
        .collect()
}

#[tokio::test]
async fn the_label_rises_to_each_hit_and_the_read_log_counts_the_snippets() {
    let dir = tempfile::tempdir().unwrap();
    let s = shelf(dir.path()).await;
    let body = "# Window\n\nThe import job has a window of two hours.\n";
    let internal = wrote(&s.owner, EntryType::Fact, Label::Internal, body, &[]).await;
    let reader = agent(dir.path(), "agent:r").with_handling_cap(Label::Internal);
    let decisions = Narrow {
        entry_type: Some(EntryType::Decision),
        ..Narrow::default()
    };
    let hits = reader.search_with(QUERY, None, &decisions, 10);
    assert_eq!(ids(&hits.await.unwrap()), [&*s.decision]);
    assert_eq!(reader.label(), Label::Public, "no internal hit was served");
    let facts = Narrow {
        entry_type: Some(EntryType::Fact),
        ..Narrow::default()
    };
    let hits = reader.search_with(QUERY, None, &facts, 10).await.unwrap();
    assert!(hits.iter().any(|h| h.id == internal));
    assert_eq!(reader.label(), Label::Internal);

    let lines = searches(dir.path(), &reader);
    assert_eq!(lines.len(), 2);
    let line = &lines[1];
    assert_eq!(line.query.as_deref(), Some(QUERY));
    assert_eq!(line.hits, hits.len());
    assert!(line.ms.is_some());
    let snippets: usize = hits
        .iter()
        .map(|h| dm_core::approx_tokens(h.snippet.as_deref().unwrap()))
        .sum();
    assert!(snippets > 0);
    let bare: Vec<Hit> = hits.iter().cloned().map(without_snippet).collect();
    assert_eq!(line.tokens, reads::hits_tokens(&bare) + snippets);
}

fn without_snippet(mut hit: Hit) -> Hit {
    hit.snippet = None;
    hit
}

#[tokio::test]
async fn a_narrowed_pack_has_a_body_only_of_an_entry_that_passes_and_its_links_are_not_narrowed() {
    let dir = tempfile::tempdir().unwrap();
    let s = shelf(dir.path()).await;
    let reader = agent(dir.path(), "agent:r").with_handling_cap(Label::Internal);
    let wide = reader.search_sections(QUERY, None, 10).await.unwrap();
    let with_body = |pieces: &[crate::recall::Piece]| {
        let held = pieces.iter().filter(|p| p.body.is_some());
        sorted(held.map(|p| p.id.as_str()).collect())
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>()
    };
    assert_eq!(with_body(&wide.pieces), s.open());

    let decisions = Narrow {
        entry_type: Some(EntryType::Decision),
        ..Narrow::default()
    };
    let pack = reader.search_sections_with(QUERY, None, &decisions, 10);
    let pack = pack.await.unwrap();
    assert_eq!(with_body(&pack.pieces), [s.decision.as_str()]);
    assert_eq!(pack.pieces.len(), 1, "a search follows no link");

    // a recall follows the link of the decision to a fact, which the narrowing names not
    let queries = [QUERY.to_string()];
    let pack = reader
        .pack_narrowed(
            &queries,
            None,
            Depth::Sections,
            Budget::default(),
            &decisions,
        )
        .await
        .unwrap();
    assert_eq!(with_body(&pack.pieces), [s.decision.as_str()]);
    let linked: Vec<&str> = pack
        .pieces
        .iter()
        .filter(|p| p.via == Via::Linked)
        .map(|p| p.id.as_str())
        .collect();
    assert_eq!(linked, [&*s.raw]);
    assert_eq!(pack.pieces.len(), 2);
}
