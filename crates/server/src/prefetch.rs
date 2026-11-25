//! Prefetch: rule 4 of the paper's progressive disclosure, applied where a task is
//! claimed. The launcher knows the task before the harness starts, so the summaries a
//! first search would return, and the entries the read log says are retrieved with
//! them, can arrive in the prompt instead of costing the harness a turn: push the
//! predictable. Summaries only, never bodies; the harness opens what it needs with
//! `get`. The prefetch runs under the session's compiled read filter like any search,
//! and the co-retrieved partners are looked up through that same filter, so nothing
//! reaches a prompt that a `search` by the same session would not have shown. It is
//! logged as one read, tool `prefetch`, so its precision can be measured and an
//! association that does not earn its place can be cut.

use crate::{reads, Error, Memory};
use dm_index::Hit;
use std::collections::BTreeMap;

/// How a prefetch is sized.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plan {
    /// Hits of the task's own search.
    pub search_k: usize,
    /// Entries in all, search hits and partners together.
    pub cap: usize,
    /// How many times a pair must have been retrieved together to count.
    pub together: usize,
    /// Tokens the summaries may cost, as a page of hits is costed.
    pub budget_tokens: usize,
}

impl Default for Plan {
    fn default() -> Self {
        Plan {
            search_k: 5,
            cap: 8,
            together: 3,
            budget_tokens: 600,
        }
    }
}

/// What a prefetch chose.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Prefetched {
    /// The query the task was searched for by.
    pub query: String,
    /// The entries, search hits first in rank order, then partners.
    pub hits: Vec<Hit>,
    /// For a partner, the search hit it was retrieved with, and how often.
    pub with: BTreeMap<String, (String, usize)>,
    /// What the summaries cost, as the read log counts a page of hits.
    pub tokens: usize,
}

/// The lowest weight at which an entry is related to another: under it, the two came
/// back together from too small a share of the searches for the first.
pub const RELATED_MIN_P: f64 = 0.25;

/// An entry related to another, as a fetch of that other entry serves it.
#[derive(Clone, Debug, PartialEq)]
pub struct Related {
    /// The summary of the related entry.
    pub hit: Hit,
    /// Its weight: the share of the searches that returned the fetched entry among
    /// their first five hits that returned this entry there too.
    pub p: f64,
}

/// The query a task is searched for by: its title and the first words of its body,
/// enough to rank on without turning a long body into a query of everything.
#[must_use]
pub fn task_query(title: &str, body: &str) -> String {
    title
        .split_whitespace()
        .chain(body.split_whitespace())
        .take(48)
        .collect::<Vec<_>>()
        .join(" ")
}

/// The entries retrieved with `anchor` at least `min` times, most often first.
fn partners(
    pairs: &BTreeMap<(String, String), usize>,
    anchor: &str,
    min: usize,
) -> Vec<(String, usize)> {
    let mut out: Vec<(String, usize)> = pairs
        .iter()
        .filter(|(_, n)| **n >= min)
        .filter_map(|((a, b), n)| {
            if a == anchor {
                Some((b.clone(), *n))
            } else if b == anchor {
                Some((a.clone(), *n))
            } else {
                None
            }
        })
        .collect();
    out.sort_by(|x, y| y.1.cmp(&x.1).then_with(|| x.0.cmp(&y.0)));
    out
}

impl Prefetched {
    fn full(&self, plan: &Plan) -> bool {
        self.hits.len() >= plan.cap
    }

    fn holds(&self, id: &str) -> bool {
        self.hits.iter().any(|h| h.id == id)
    }

    /// Take a hit if it fits the cap and the budget; one that does not is skipped, and
    /// a smaller one after it may still fit.
    fn offer(&mut self, hit: Hit, plan: &Plan) -> bool {
        let cost = reads::hits_tokens(std::slice::from_ref(&hit));
        if self.full(plan) || self.holds(&hit.id) || self.tokens + cost > plan.budget_tokens {
            return false;
        }
        self.tokens += cost;
        self.hits.push(hit);
        true
    }

    /// The ids, in the order served.
    #[must_use]
    pub fn ids(&self) -> Vec<String> {
        self.hits.iter().map(|h| h.id.clone()).collect()
    }
}

/// The entries related to `id`, each with its weight: the partners that came back with
/// `id` from at least `min` searches and from at least the share [`RELATED_MIN_P`] of
/// the `searches` that returned `id`. Highest weight first, then by id.
fn weighted(
    pairs: &BTreeMap<(String, String), usize>,
    searches: usize,
    id: &str,
    min: usize,
) -> Vec<(String, f64)> {
    if searches == 0 {
        return Vec::new();
    }
    let share = |n: usize| {
        let n = u32::try_from(n).unwrap_or(u32::MAX);
        let of = u32::try_from(searches).unwrap_or(u32::MAX);
        f64::from(n) / f64::from(of)
    };
    let mut out: Vec<(String, f64)> = partners(pairs, id, min)
        .into_iter()
        .map(|(partner, n)| (partner, share(n)))
        .filter(|(_, p)| *p >= RELATED_MIN_P)
        .collect();
    out.sort_by(|x, y| y.1.total_cmp(&x.1).then_with(|| x.0.cmp(&y.0)));
    out
}

impl Memory {
    /// The entries related to `id`, each with its weight `p`: the entries that the read
    /// log shows searched up with it at least `Plan::default().together` times, and in
    /// at least the share [`RELATED_MIN_P`] of the searches that returned `id`. `p` is
    /// that share: the searches whose first five hits hold both entries, over the
    /// searches whose first five hits hold `id`. Highest `p` first, at most `max`. Each
    /// one is looked up under this session's filter, so an association mined from other
    /// sessions' reads never widens what this one sees. The paper's rule: when B usually
    /// follows A, B's summary rides on A's fetch, a few tokens in place of a search
    /// round. The session's label rises as for a search, and the read log names the
    /// summaries as one `read_with`.
    ///
    /// # Errors
    ///
    /// An index error.
    pub async fn related(&self, id: &str, max: usize) -> Result<Vec<Related>, Error> {
        if !self.settings().prefetches() {
            return Ok(Vec::new());
        }
        let f = self.session_filter()?;
        let log = reads::all(&self.root).unwrap_or_default();
        let pairs = reads::co_retrieved(&log, |_| true);
        let searches = reads::searches_with(&log).get(id).copied().unwrap_or(0);
        let mut out = Vec::new();
        {
            let index = self.index.lock().await;
            for (partner, p) in weighted(&pairs, searches, id, Plan::default().together) {
                if out.len() >= max {
                    break;
                }
                if let Some(meta) = index.meta(&partner, &f)? {
                    out.push(Related { hit: meta.hit, p });
                }
            }
        }
        if !out.is_empty() {
            let hits: Vec<Hit> = out.iter().map(|r| r.hit.clone()).collect();
            self.observe(&hits)?;
            self.reads.hits("read_with", Some(id), &hits);
        }
        Ok(out)
    }

    /// The summaries to append to a fetch of `id`: [`Memory::related`], without the
    /// weights.
    ///
    /// # Errors
    ///
    /// An index error.
    pub async fn read_with(&self, id: &str, max: usize) -> Result<Vec<Hit>, Error> {
        let related = self.related(id, max).await?;
        Ok(related.into_iter().map(|r| r.hit).collect())
    }

    /// Summaries likely needed for a task: the top hits of a search on `query`, then
    /// the entries the read log says are co-retrieved with them, inside the plan's cap
    /// and budget. Everything is admitted by the session's read filter, the label rises
    /// as for a search, and the read log records it as one `prefetch`.
    ///
    /// # Errors
    ///
    /// The policy's read scope cannot be computed, or an index error.
    pub async fn prefetch(&self, query: &str, plan: &Plan) -> Result<Prefetched, Error> {
        let f = self.session_filter()?;
        let mut out = Prefetched {
            query: query.to_string(),
            ..Prefetched::default()
        };
        if !self.settings().prefetches() {
            return Ok(out);
        }
        let pairs = reads::co_retrieved(&reads::all(&self.root).unwrap_or_default(), |_| true);
        {
            let index = self.index.lock().await;
            let found = index.search(query, &f, plan.search_k)?;
            for h in found {
                out.offer(h, plan);
            }
            // the search hits that fitted, in rank order, are what partners attach to
            let anchors = out.ids();
            for anchor in &anchors {
                for (id, n) in partners(&pairs, anchor, plan.together) {
                    if out.full(plan) {
                        break;
                    }
                    if out.holds(&id) {
                        continue;
                    }
                    // the partner is looked up under the same filter: an association
                    // mined from other sessions' reads never widens what this one sees
                    if let Some(meta) = index.meta(&id, &f)? {
                        if out.offer(meta.hit, plan) {
                            out.with.insert(id, (anchor.clone(), n));
                        }
                    }
                }
            }
        }
        self.observe(&out.hits)?;
        let ids = out.ids();
        self.reads.served_ids(
            "prefetch",
            Some(query),
            ids.len(),
            out.tokens,
            false,
            None,
            &ids,
        );
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WriteRequest;
    use dm_core::Label;

    async fn write(m: &Memory, body: &str, label: Label) -> String {
        m.write(WriteRequest {
            body: body.into(),
            classification: Some(label),
            ..WriteRequest::default()
        })
        .await
        .unwrap()
        .id
    }

    fn searched_together(root: &std::path::Path, ids: &[&String], times: usize) {
        let log = reads::ReadLog::new(root, "sess-earlier");
        let ids: Vec<String> = ids.iter().map(|i| (*i).clone()).collect();
        for _ in 0..times {
            log.served_ids("search", Some("q"), ids.len(), 10, false, None, &ids);
        }
    }

    /// One earlier search that returned `ids`, in that order.
    fn searched(root: &std::path::Path, ids: &[&String]) {
        searched_together(root, ids, 1);
    }

    #[tokio::test]
    async fn a_related_entry_has_the_share_of_the_searches_that_returned_both_as_its_weight() {
        let dir = tempfile::tempdir().unwrap();
        let m = Memory::init(dir.path(), "human:test:me", "dom-me").unwrap();
        let a = write(
            &m,
            "# Retry backoff\n\nOperation R retries.\n",
            Label::Public,
        )
        .await;
        let often = write(&m, "# Jitter\n\nTwenty percent.\n", Label::Public).await;
        let twice = write(&m, "# Burst\n\nThe June burst.\n", Label::Public).await;
        // four searches returned `a`: three of them with `often`, two with `twice`
        searched_together(dir.path(), &[&a, &often, &twice], 2);
        searched(dir.path(), &[&often, &a]);
        searched(dir.path(), &[&a]);
        // a search that returned neither, and a list, change nothing
        searched(dir.path(), &[&twice]);
        let log = reads::ReadLog::new(dir.path(), "sess-earlier");
        log.served_ids(
            "list",
            None,
            2,
            10,
            false,
            None,
            &[a.clone(), twice.clone()],
        );
        let reader = Memory::open(dir.path()).unwrap().with_author("agent:r");
        let related = reader.related(&a, 5).await.unwrap();
        assert_eq!(related.len(), 1, "{related:?}");
        assert_eq!(related[0].hit.id, often);
        assert!((related[0].p - 0.75).abs() < 1e-9, "{}", related[0].p);
        // the summaries are one read, by the name that the read log has for them
        let logged: Vec<_> = reads::all(dir.path())
            .unwrap()
            .into_iter()
            .filter(|r| r.tool == "read_with")
            .collect();
        assert_eq!(logged.len(), 1);
        assert_eq!(logged[0].ids, [often.as_str()]);
        assert_eq!(
            reader.read_with(&a, 5).await.unwrap(),
            [related[0].hit.clone()]
        );
        // seen from the other entry: three of its three searches returned `a`
        let back = reader.related(&often, 5).await.unwrap();
        assert_eq!(back.len(), 1, "{back:?}");
        assert!((back[0].p - 1.0).abs() < 1e-9, "{}", back[0].p);
        assert!(reader.related(&a, 0).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_entry_under_the_lowest_weight_is_not_related_and_the_order_is_by_weight() {
        let dir = tempfile::tempdir().unwrap();
        let m = Memory::init(dir.path(), "human:test:me", "dom-me").unwrap();
        let a = write(
            &m,
            "# Retry backoff\n\nOperation R retries.\n",
            Label::Public,
        )
        .await;
        let seldom = write(&m, "# Burst\n\nThe June burst.\n", Label::Public).await;
        let often = write(&m, "# Jitter\n\nTwenty percent.\n", Label::Public).await;
        let half = write(&m, "# Header\n\nX-Retry-Ctx.\n", Label::Public).await;
        // thirteen searches returned `a`: three with `seldom`, which is under a quarter
        searched_together(dir.path(), &[&a, &seldom, &half], 3);
        searched_together(dir.path(), &[&a, &often, &half], 4);
        searched_together(dir.path(), &[&a, &often], 6);
        let reader = Memory::open(dir.path()).unwrap().with_author("agent:r");
        let related = reader.related(&a, 5).await.unwrap();
        let ids: Vec<&str> = related.iter().map(|r| r.hit.id.as_str()).collect();
        assert_eq!(ids, [often.as_str(), half.as_str()], "{related:?}");
        assert!((related[0].p - 10.0 / 13.0).abs() < 1e-9);
        assert!((related[1].p - 7.0 / 13.0).abs() < 1e-9);
        // at the lowest weight an entry is related: three of twelve searches
        let b = write(&m, "# Queue\n\nThe queue is bounded.\n", Label::Public).await;
        let edge = write(&m, "# Depth\n\nThe depth is 64.\n", Label::Public).await;
        searched_together(dir.path(), &[&b, &edge], 3);
        searched_together(dir.path(), &[&b], 9);
        let related = reader.related(&b, 5).await.unwrap();
        assert_eq!(related.len(), 1, "{related:?}");
        assert!((related[0].p - RELATED_MIN_P).abs() < 1e-9);
    }

    #[tokio::test]
    async fn an_entry_that_the_filter_of_the_session_does_not_admit_is_not_related() {
        let dir = tempfile::tempdir().unwrap();
        let m = Memory::init(dir.path(), "human:test:me", "dom-me").unwrap();
        let a = write(
            &m,
            "# Retry backoff\n\nOperation R retries.\n",
            Label::Public,
        )
        .await;
        let internal = write(&m, "# Jitter\n\nTwenty percent.\n", Label::Internal).await;
        searched_together(dir.path(), &[&a, &internal], 3);
        let capped = Memory::open(dir.path())
            .unwrap()
            .with_author("agent:low")
            .with_handling_cap(Label::Public);
        assert!(capped.related(&a, 5).await.unwrap().is_empty());
        assert_eq!(capped.label(), Label::Public);
        let reader = Memory::open(dir.path()).unwrap().with_author("agent:r");
        let related = reader.related(&a, 5).await.unwrap();
        assert_eq!(related.len(), 1, "{related:?}");
        assert_eq!(related[0].hit.id, internal);
        assert_eq!(reader.label(), Label::Internal);
    }

    #[test]
    fn a_task_query_is_its_title_and_the_start_of_its_body() {
        let body = "word ".repeat(100);
        let q = task_query("Fix the  import", &body);
        assert!(q.starts_with("Fix the import word"), "{q}");
        assert_eq!(q.split_whitespace().count(), 48);
    }

    #[test]
    fn partners_are_those_retrieved_together_often_enough_most_often_first() {
        let mut pairs = BTreeMap::new();
        pairs.insert(("a".to_string(), "b".to_string()), 3);
        pairs.insert(("a".to_string(), "c".to_string()), 5);
        pairs.insert(("a".to_string(), "d".to_string()), 2);
        pairs.insert(("b".to_string(), "c".to_string()), 9);
        assert_eq!(
            partners(&pairs, "a", 3),
            vec![("c".to_string(), 5), ("b".to_string(), 3)]
        );
    }

    #[tokio::test]
    async fn prefetch_serves_search_hits_and_co_retrieved_partners_and_logs_one_read() {
        let dir = tempfile::tempdir().unwrap();
        let m = Memory::init(dir.path(), "human:test:me", "dom-me").unwrap();
        let backoff = write(
            &m,
            "# Retry backoff\n\nOperation R retries.\n",
            Label::Public,
        )
        .await;
        let header = write(
            &m,
            "# Context header\n\nX-Retry-Ctx carries state.\n",
            Label::Public,
        )
        .await;
        let once = write(&m, "# Burst incident\n\nThe June burst.\n", Label::Public).await;
        searched_together(dir.path(), &[&backoff, &header], 3);
        searched_together(dir.path(), &[&backoff, &once], 2);
        drop(m);
        let agent = Memory::open(dir.path()).unwrap().with_author("agent:a");
        let p = agent
            .prefetch("retry backoff", &Plan::default())
            .await
            .unwrap();
        assert_eq!(p.ids(), vec![backoff.clone(), header.clone()], "{p:?}");
        assert_eq!(p.with.get(&header), Some(&(backoff.clone(), 3)));
        let logged: Vec<_> = reads::all(dir.path())
            .unwrap()
            .into_iter()
            .filter(|r| r.tool == "prefetch")
            .collect();
        assert_eq!(logged.len(), 1);
        assert_eq!(logged[0].session, agent.session_id());
        assert_eq!(logged[0].ids, p.ids());
        assert_eq!(logged[0].tokens, p.tokens);
        let tight = Plan {
            cap: 1,
            ..Plan::default()
        };
        assert_eq!(
            agent
                .prefetch("retry backoff", &tight)
                .await
                .unwrap()
                .ids()
                .len(),
            1
        );
        let none = Plan {
            budget_tokens: 5,
            ..Plan::default()
        };
        assert!(agent
            .prefetch("retry backoff", &none)
            .await
            .unwrap()
            .hits
            .is_empty());
    }

    #[tokio::test]
    async fn prefetch_respects_a_narrowed_read_filter_for_hits_and_partners() {
        let dir = tempfile::tempdir().unwrap();
        let owner = Memory::init(dir.path(), "human:test:me", "dom-me").unwrap();
        let public = write(
            &owner,
            "# Backoff\n\nBackoff is exponential, public.\n",
            Label::Public,
        )
        .await;
        let internal = write(
            &owner,
            "# Backoff internal\n\nBackoff is exponential, internal.\n",
            Label::Internal,
        )
        .await;
        let partner = write(
            &owner,
            "# Jitter\n\nJitter is twenty percent, internal.\n",
            Label::Internal,
        )
        .await;
        searched_together(dir.path(), &[&public, &partner], 4);
        drop(owner);
        // the root's own policy: the built-in one with agents narrowed to public
        let text = dm_policy::decide::BUILTIN.replace(
            "read_filter := base_filter\n",
            "read_filter := object.union(base_filter, {\"handling\": \"public\"}) if class == \"agent\"\n\nread_filter := base_filter if class != \"agent\"\n",
        );
        std::fs::create_dir_all(dir.path().join("policy")).unwrap();
        std::fs::write(dir.path().join("policy/local.rego"), text).unwrap();
        let agent = Memory::open(dir.path())
            .unwrap()
            .with_author("agent:narrowed");
        let p = agent
            .prefetch("backoff exponential", &Plan::default())
            .await
            .unwrap();
        assert_eq!(
            p.ids(),
            vec![public.clone()],
            "neither the internal hit nor the internal partner: {p:?}"
        );
        assert!(!p.ids().contains(&internal) && !p.ids().contains(&partner));
        assert_eq!(agent.label(), Label::Public);
        // the owner, whose filter admits internal, gets both the hit and the partner
        let owner = Memory::open(dir.path()).unwrap();
        let p = owner
            .prefetch("backoff exponential", &Plan::default())
            .await
            .unwrap();
        assert!(
            p.ids().contains(&internal) && p.ids().contains(&partner),
            "{p:?}"
        );
    }
}
