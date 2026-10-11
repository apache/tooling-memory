//! The platform as a domain: what configures the agents, held in memory under the same
//! rules as what they know, and loaded only once verified.
//!
//! The grants may name one domain the platform's (`platform = "dom-platform"`). Its
//! entries carry artifacts, each a fenced block in the body:
//!
//! ````text
//! ```platform agent claude
//! Before you change a migration, read its decision entry.
//! ```
//! ````
//!
//! The info string is `platform <kind> <name>`. Four kinds are read today: an `agent`
//! definition, instructions added to a harness's work prompt (named for the harness's
//! binary, or `default`); a `template`, the opening of the work prompt (`work`); an
//! `eval` suite, a golden suite's TOML; and a `harness` configuration, JSON, of which
//! the default `model` is read. An artifact is loaded only from an active entry a person
//! verified, as a policy entry is enforced only once verified, so an agent cannot write
//! the instructions the next agent runs under; the newest verified one wins. Everything
//! else in the domain is listed, with why it is not loaded.

use crate::{Error, Memory};
use dm_core::{Entry, TrustTier};
use serde::Serialize;

/// The kinds of artifact the launcher reads.
pub const KINDS: &[&str] = &["agent", "template", "eval", "harness"];

/// One artifact, as an entry holds it.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Artifact {
    /// `agent`, `template`, `eval` or `harness`.
    pub kind: String,
    /// What it is for: a harness, `work`, a suite's name.
    pub name: String,
    /// The block's text.
    pub text: String,
    /// The entry it is in.
    pub entry: String,
    /// When that entry was written.
    pub created: String,
}

/// An artifact in the listing, and whether it is loaded.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Listed {
    /// The artifact.
    #[serde(flatten)]
    pub artifact: Artifact,
    /// Its entry's trust tier.
    pub trust_tier: String,
    /// Whether the launcher loads it.
    pub loaded: bool,
    /// Why not, when it is not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why_not: Option<String>,
}

/// The artifacts in an entry's body: each fenced block whose info string is
/// `platform <kind> <name>`.
#[must_use]
pub fn artifacts_in(e: &Entry) -> Vec<Artifact> {
    let mut out = Vec::new();
    let mut open: Option<(String, String, String, Vec<&str>)> = None;
    for line in e.body.lines() {
        let trimmed = line.trim_start();
        match &mut open {
            None => {
                if let Some(info) = trimmed.strip_prefix("```") {
                    let mut words = info.split_whitespace();
                    if words.next() == Some("platform") {
                        if let (Some(kind), Some(name)) = (words.next(), words.next()) {
                            let fence = "```".to_string();
                            open = Some((kind.to_string(), name.to_string(), fence, Vec::new()));
                        }
                    }
                }
            }
            Some((kind, name, fence, lines)) => {
                if trimmed.trim_end() == fence.as_str() {
                    out.push(Artifact {
                        kind: kind.clone(),
                        name: name.clone(),
                        text: lines.join("\n"),
                        entry: e.front.id.clone(),
                        created: e
                            .front
                            .created
                            .format(&time::format_description::well_known::Rfc3339)
                            .unwrap_or_default(),
                    });
                    open = None;
                } else {
                    lines.push(line);
                }
            }
        }
    }
    out
}

/// Why an entry's artifacts are not loaded, if they are not.
fn why_not(e: &Entry, kind: &str) -> Option<String> {
    if !KINDS.contains(&kind) {
        return Some(format!("{kind} is not a kind the launcher reads"));
    }
    if !e.is_active() {
        return Some("its entry is not active".into());
    }
    if e.front.trust_tier != TrustTier::Verified {
        return Some(format!(
            "its entry is {}; an artifact loads once a person verifies it",
            e.front.trust_tier
        ));
    }
    None
}

impl Memory {
    /// The platform's domain, when the grants name one they hold.
    #[must_use]
    pub fn platform_domain(&self) -> Option<String> {
        self.grants
            .platform
            .clone()
            .filter(|d| self.grants.domains.contains_key(d))
    }

    async fn platform_entries(&self) -> Result<Vec<Entry>, Error> {
        let Some(domain) = self.platform_domain() else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for id in self.store.list(&domain).await? {
            if let Some(e) = self.store.get(&domain, &id).await? {
                out.push(e);
            }
        }
        Ok(out)
    }

    /// Every artifact in the platform's domain, newest first, and whether each is
    /// loaded. The owner's view.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for anyone but the owner, or a store error.
    pub async fn platform(&self) -> Result<Vec<Listed>, Error> {
        if self.author != self.grants.owner {
            return Err(Error::Denied(
                "the platform's listing is the owner's".into(),
            ));
        }
        let mut out = Vec::new();
        for e in self.platform_entries().await? {
            for a in artifacts_in(&e) {
                let why = why_not(&e, &a.kind);
                out.push(Listed {
                    trust_tier: e.front.trust_tier.to_string(),
                    loaded: why.is_none(),
                    why_not: why,
                    artifact: a,
                });
            }
        }
        out.sort_by(|a, b| b.artifact.created.cmp(&a.artifact.created));
        Ok(out)
    }

    /// The artifact of `kind` named `name` the launcher loads: from an active, verified
    /// entry in the platform's domain, the newest; none when there is none. Read as the
    /// record holds it, whoever asks: the launcher loads it before any session reads.
    ///
    /// # Errors
    ///
    /// A store error.
    pub async fn platform_artifact(
        &self,
        kind: &str,
        name: &str,
    ) -> Result<Option<Artifact>, Error> {
        let mut found: Option<Artifact> = None;
        for e in self.platform_entries().await? {
            for a in artifacts_in(&e) {
                let wanted = a.kind == kind && a.name == name && why_not(&e, &a.kind).is_none();
                if wanted && found.as_ref().is_none_or(|f| a.created > f.created) {
                    found = Some(a);
                }
            }
        }
        Ok(found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WriteRequest;

    const BODY: &str = "# Claude here\n\nHow Claude works in this repository.\n\n\
```platform agent claude\nRead the decision entry before a migration.\n```\n\n\
```toml\nnot = \"an artifact\"\n```\n\n\
```platform harness claude\n{\"model\": \"claude-opus-5-5\"}\n```\n\n\
```platform eval unterminated\nnever closed\n";

    /// A root whose platform is `dom-plat`, beside the owner's `dom-me`.
    fn root(dir: &std::path::Path) -> Memory {
        Memory::init(dir, "human:test:me", "dom-me").unwrap();
        let path = dir.join(crate::GRANTS_FILE);
        let mut g = dm_policy::Grants::load(&path).unwrap();
        let d = g.domains["dom-me"].clone();
        g.domains.insert("dom-plat".into(), d);
        g.platform = Some("dom-plat".into());
        std::fs::write(&path, g.render().unwrap()).unwrap();
        Memory::open(dir).unwrap()
    }

    async fn written(m: &Memory, body: &str, verified: bool) -> String {
        let id = m
            .write(WriteRequest {
                body: body.into(),
                domain: Some("dom-plat".into()),
                entry_type: dm_core::EntryType::Convention,
                ..WriteRequest::default()
            })
            .await
            .unwrap()
            .id;
        if verified {
            // as a person other than its proposer would leave it
            let mut e = m.store.get("dom-plat", &id).await.unwrap().unwrap();
            e.front.trust_tier = TrustTier::Verified;
            m.store.put(&e).await.unwrap();
        }
        id
    }

    #[test]
    fn artifacts_are_the_platform_blocks_closed_in_the_body() {
        let e = Entry::parse(&format!(
            "---\nid: mem-plat-1\ntype: convention\ndomain: dom-plat\nauthor: human:test:me\n\
             created: 2026-10-01T09:00:00Z\ntrust_tier: raw\nclassification: internal\n---\n{BODY}"
        ))
        .unwrap();
        let a = artifacts_in(&e);
        let kinds: Vec<(&str, &str)> = a
            .iter()
            .map(|x| (x.kind.as_str(), x.name.as_str()))
            .collect();
        assert_eq!(kinds, [("agent", "claude"), ("harness", "claude")]);
        assert_eq!(a[0].text, "Read the decision entry before a migration.");
        assert_eq!(a[1].text, "{\"model\": \"claude-opus-5-5\"}");
    }

    #[tokio::test]
    async fn only_a_verified_artifact_loads_and_the_newest_wins() {
        let dir = tempfile::tempdir().unwrap();
        let m = root(dir.path());
        let raw = written(&m, BODY, false).await;
        assert!(m
            .platform_artifact("agent", "claude")
            .await
            .unwrap()
            .is_none());
        let listed = m.platform().await.unwrap();
        assert!(listed.iter().all(|l| !l.loaded), "{listed:?}");
        assert!(listed[0].why_not.as_deref().unwrap().contains("verifies"));
        let first = written(&m, BODY, true).await;
        let a = m
            .platform_artifact("agent", "claude")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(a.entry, first);
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        let newer = written(&m, &BODY.replace("migration", "schema change"), true).await;
        let a = m
            .platform_artifact("agent", "claude")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(a.entry, newer);
        assert!(a.text.contains("schema change"));
        assert_ne!(raw, newer);
        // what is in another domain is not the platform's
        m.write(WriteRequest {
            body: "```platform agent codex\nnot here\n```\n".into(),
            domain: Some("dom-me".into()),
            ..WriteRequest::default()
        })
        .await
        .unwrap();
        assert!(m
            .platform_artifact("agent", "codex")
            .await
            .unwrap()
            .is_none());
        let agent = Memory::open(dir.path()).unwrap().with_author("agent:test");
        assert!(
            agent.platform().await.is_err(),
            "the listing is the owner's"
        );
    }

    #[test]
    fn a_platform_domain_the_owner_does_not_hold_is_refused() {
        let mut g = dm_policy::Grants::starter("human:test:me", "dom-me");
        g.platform = Some("dom-else".into());
        let text = g.render().unwrap();
        assert!(dm_policy::Grants::parse(&text).is_err());
        g.platform = Some("dom-me".into());
        let back = dm_policy::Grants::parse(&g.render().unwrap()).unwrap();
        assert_eq!(back.platform.as_deref(), Some("dom-me"));
    }
}
