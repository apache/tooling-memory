//! What the tests of several modules share: a root with a platform domain, a root with
//! a held-out domain and an evaluator beside it, and entries as a person's review would
//! leave them.

use crate::{Memory, WriteRequest};
use dm_core::TrustTier;
use std::path::Path;

/// The evaluator that [`held_out_root`] names.
pub(crate) const EVALUATOR: &str = "agent:evaluator";

/// A root whose platform is `dom-plat`, beside the owner's `dom-me`.
pub(crate) fn root(dir: &Path) -> Memory {
    Memory::init(dir, "human:test:me", "dom-me").unwrap();
    let path = dir.join(crate::GRANTS_FILE);
    let mut g = dm_policy::Grants::load(&path).unwrap();
    let d = g.domains["dom-me"].clone();
    g.domains.insert("dom-plat".into(), d);
    g.platform = Some("dom-plat".into());
    std::fs::write(&path, g.render().unwrap()).unwrap();
    Memory::open(dir).unwrap()
}

/// [`root`], with the held-out domain `dom-held` and the evaluator [`EVALUATOR`].
pub(crate) fn held_out_root(dir: &Path) -> Memory {
    root(dir);
    let path = dir.join(crate::GRANTS_FILE);
    let mut g = dm_policy::Grants::load(&path).unwrap();
    let d = g.domains["dom-me"].clone();
    g.domains.insert("dom-held".into(), d);
    g.held_out = Some("dom-held".into());
    g.evaluator = Some(EVALUATOR.into());
    std::fs::write(&path, g.render().unwrap()).unwrap();
    Memory::open(dir).unwrap()
}

/// A new session of this root for `author`.
pub(crate) fn as_author(dir: &Path, author: &str) -> Memory {
    Memory::open(dir).unwrap().with_author(author)
}

/// An entry of `dom-plat` with `body`, verified or raw; its id.
pub(crate) async fn written(m: &Memory, body: &str, verified: bool) -> String {
    written_in(m, "dom-plat", body, verified).await
}

/// An entry of `domain` with `body`, verified or raw; its id.
pub(crate) async fn written_in(m: &Memory, domain: &str, body: &str, verified: bool) -> String {
    let id = m
        .write(WriteRequest {
            body: body.into(),
            domain: Some(domain.into()),
            entry_type: dm_core::EntryType::Convention,
            ..WriteRequest::default()
        })
        .await
        .unwrap()
        .id;
    if verified {
        verify_in(m, domain, &id).await;
    }
    id
}

/// Leave the entry `id` of `dom-plat` as a person other than its proposer would.
pub(crate) async fn verify(m: &Memory, id: &str) {
    verify_in(m, "dom-plat", id).await;
}

/// Leave the entry `id` of `domain` as a person other than its proposer would.
pub(crate) async fn verify_in(m: &Memory, domain: &str, id: &str) {
    let mut e = m.store.get(domain, id).await.unwrap().unwrap();
    e.front.trust_tier = TrustTier::Verified;
    m.store.put(&e).await.unwrap();
}

/// The actions that the decision log of the root `dir` holds a refusal of for
/// `principal`, each one time, with the class of the principal in the line.
pub(crate) fn refused_actions(dir: &Path, principal: &str) -> Vec<(String, String)> {
    let log = std::fs::read_to_string(dir.join("state/decisions.jsonl")).unwrap_or_default();
    let mut actions = std::collections::BTreeSet::new();
    for line in log.lines() {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        if v["allow"] == false && v["principal"] == principal {
            let text = |key: &str| v[key].as_str().unwrap_or_default().to_string();
            actions.insert((text("action"), text("class")));
        }
    }
    actions.into_iter().collect()
}

/// The entries that the read events of `m`'s session name in the log of `domain`, in the
/// log's order.
pub(crate) fn read_in_log(m: &Memory, domain: &str) -> Vec<String> {
    let events = crate::log::read(m.root(), domain, 0).unwrap();
    let own = events
        .into_iter()
        .filter(|e| e.session.as_deref() == Some(m.session_id.as_str()));
    own.filter_map(|e| match e.change {
        crate::log::Change::Read { ids, .. } => Some(ids),
        _ => None,
    })
    .flatten()
    .collect()
}
