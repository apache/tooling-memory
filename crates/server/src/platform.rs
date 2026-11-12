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
//! The info string is `platform <kind> <name>`. Five kinds are read today: a `task`
//! definition, TOML, the record for one kind of work ([`TaskDefinition`]); an `agent`
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
pub const KINDS: &[&str] = &["agent", "template", "eval", "harness", "task"];

/// What a completion of a task needs before it counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Autonomy {
    /// A person's verdict. Where every definition starts.
    #[default]
    Person,
    /// A review by a session on another model than the author's.
    Review,
    /// The acceptance checks alone.
    Checks,
}

/// A task definition: the record for one kind of work. A task names one, and what a
/// session that claims the task may use comes from it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskDefinition {
    /// Its name, from the block's info string.
    #[serde(skip_deserializing)]
    pub name: String,
    /// The entry it is in. The entry's id is the definition's version: an entry is never
    /// edited, so a task that names `<name>@<entry>` names one text for good.
    #[serde(skip_deserializing)]
    pub entry: String,
    /// The classification of that entry: a task that takes its checks is no lower.
    #[serde(skip_deserializing)]
    pub classification: dm_core::Label,
    /// What the work is for, in a sentence: review reads the tools against it.
    #[serde(default)]
    pub purpose: String,
    /// The capabilities the work needs, as a task's requirements name them
    /// (`tool:fetch`). A session that holds a task of this definition calls no tool
    /// outside them.
    #[serde(default)]
    pub tools: Vec<String>,
    /// The domains the work reads.
    #[serde(default)]
    pub scope: Vec<String>,
    /// The checks that say a task of this kind is done. A task that gives none of its
    /// own takes these.
    #[serde(default)]
    pub acceptance: Vec<String>,
    /// What the work leaves behind.
    #[serde(default)]
    pub outputs: Vec<String>,
    /// What a completion needs before it counts.
    #[serde(default)]
    pub autonomy: Autonomy,
}

impl TaskDefinition {
    /// How a task names this definition: `<name>@<entry>`.
    #[must_use]
    pub fn reference(&self) -> String {
        format!("{}@{}", self.name, self.entry)
    }
}

/// The definition an entry holds under `name`, when the entry is active and verified
/// and the block reads as one.
fn definition_in(e: &Entry, name: &str) -> Option<Result<TaskDefinition, ()>> {
    if !e.is_active() || e.front.trust_tier != TrustTier::Verified {
        return None;
    }
    let block = artifacts_in(e)
        .into_iter()
        .find(|a| a.kind == "task" && a.name == name)?;
    let parsed = toml::from_str::<TaskDefinition>(&block.text)
        .ok()
        // a tool is named as a task's requirements name it
        .filter(|d| {
            d.tools
                .iter()
                .all(|t| t.starts_with("tool:") && t.len() > 5)
        });
    Some(
        parsed
            .map(|mut d| {
                d.name = name.to_string();
                d.entry.clone_from(&e.front.id);
                d.classification = e.front.classification;
                d
            })
            .ok_or(()),
    )
}

/// The name in a task's `definition`, with or without its `@<entry>`.
#[must_use]
pub fn definition_name(reference: &str) -> &str {
    reference.split('@').next().unwrap_or(reference)
}

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

    /// The task definition named `name` that is in force: the one in the newest active
    /// entry a person verified.
    ///
    /// # Errors
    ///
    /// A store error, or [`Error::Denied`] when the block in force does not read as a
    /// definition. The refusal names the definition and says nothing of its text.
    pub async fn task_definition(&self, name: &str) -> Result<Option<TaskDefinition>, Error> {
        let mut newest: Option<Entry> = None;
        for e in self.platform_entries().await? {
            let later = newest
                .as_ref()
                .is_none_or(|n| e.front.created > n.front.created);
            if later && definition_in(&e, name).is_some() {
                newest = Some(e);
            }
        }
        newest
            .and_then(|e| definition_in(&e, name))
            .transpose()
            .map_err(|()| unreadable(name))
    }

    /// The definition a task names as `<name>@<entry>`, while that entry is still active
    /// and verified; none once it was replaced, demoted or erased.
    ///
    /// # Errors
    ///
    /// A store error, or [`Error::Denied`] when the block does not read as a definition.
    pub async fn pinned_definition(&self, named: &str) -> Result<Option<TaskDefinition>, Error> {
        let name = definition_name(named);
        let Some((_, entry)) = named.split_once('@') else {
            return self.task_definition(name).await;
        };
        let Some(domain) = self.platform_domain() else {
            return Ok(None);
        };
        let Some(e) = self.store.get(&domain, entry).await? else {
            return Ok(None);
        };
        definition_in(&e, name)
            .transpose()
            .map_err(|()| unreadable(name))
    }

    /// Bind a task to the definition it names. The definition must be in force, and the
    /// session must be one that may read its entry. The task then names it with its
    /// entry, takes its acceptance criteria if it gave none, is classified no lower than
    /// the entry, and requires what the definition declares, so only a session that has
    /// those claims it.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] when no definition of that name is in force for this session.
    /// A definition the session may not read is refused in the same words as one that
    /// does not exist.
    pub(crate) async fn bind_definition(
        &self,
        spec: &mut dm_ledger::TaskSpec,
    ) -> Result<(), Error> {
        let Some(named) = spec.definition.clone() else {
            return Ok(());
        };
        let name = definition_name(&named);
        let none = || {
            Error::Denied(format!(
                "no task definition {name} is in force: a verified entry of the platform's domain defines one"
            ))
        };
        let def = match self.pinned_definition(&named).await {
            Ok(Some(def)) => def,
            Ok(None) | Err(Error::Denied(_)) => return Err(none()),
            Err(e) => return Err(e),
        };
        // read as any entry is: refused above the session's handling, and the session's
        // label rises to what it read
        if self.get(&def.entry).await.is_err() {
            return Err(none());
        }
        spec.definition = Some(def.reference());
        spec.classification = spec.classification.max(def.classification);
        if spec.acceptance.is_empty() {
            spec.acceptance.clone_from(&def.acceptance);
        }
        for tool in &def.tools {
            if !spec.requirements.contains(tool) {
                spec.requirements.push(tool.clone());
            }
        }
        Ok(())
    }

    /// Why triage does not admit a task on its own, when what it lacks is a definition
    /// or a way to check it: the missing items, for whoever filed it. None when nothing
    /// is missing.
    #[must_use]
    pub fn admission_gap(&self, spec: &dm_ledger::TaskSpec) -> Option<String> {
        if spec.definition.is_none() {
            return self
                .grants
                .gates
                .require_definition
                .then(|| "it names no task definition".to_string());
        }
        spec.acceptance
            .is_empty()
            .then(|| "it has no acceptance criteria, its own or its definition's".to_string())
    }

    /// Whether the definition a task names is out of force: replaced, demoted or erased
    /// since the task was filed. Such a task is not admitted by anyone.
    pub(crate) async fn definition_gone(
        &self,
        spec: &dm_ledger::TaskSpec,
    ) -> Result<Option<String>, Error> {
        let Some(named) = &spec.definition else {
            return Ok(None);
        };
        Ok(match self.pinned_definition(named).await {
            Ok(Some(_)) => None,
            Ok(None) | Err(Error::Denied(_)) => {
                Some(format!("its task definition {named} is no longer in force"))
            }
            Err(e) => return Err(e),
        })
    }

    /// Whether the task this session holds lets it call `tool`: a task under a
    /// definition reaches only the tools that definition declares, as the task named
    /// it. A session that holds no task, or a task under no definition, is held to the
    /// grants alone.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] when the definition does not declare the tool or is no longer
    /// in force; a ledger or store error when the task or the definition cannot be
    /// read, since a check that could not look has not passed.
    pub(crate) async fn definition_allows(&self, tool: &str) -> Result<(), Error> {
        let Some(held) = self.held.get() else {
            return Ok(());
        };
        let task = self.ledger.lock().await.get(held).await?;
        let Some(named) = task.spec.definition else {
            return Ok(());
        };
        let Some(def) = self.pinned_definition(&named).await? else {
            return Err(Error::Denied(format!(
                "the task definition {named} is no longer in force, so its task reaches no tool"
            )));
        };
        if def.tools.iter().any(|t| t == &format!("tool:{tool}")) {
            Ok(())
        } else {
            Err(Error::Denied(format!(
                "the task definition {named} does not declare tool:{tool}; a task reaches only the tools its definition declares"
            )))
        }
    }

    /// What this session can do, as the ledger matches it to a task's requirements: the
    /// manifest it was given, and each tool the grants give it.
    #[must_use]
    pub(crate) fn capabilities(&self) -> Vec<String> {
        let mut all = self.manifest.clone();
        for (name, tap) in &self.grants.taps {
            let tool = format!("tool:{name}");
            if tap.tool && !all.contains(&tool) {
                all.push(tool);
            }
        }
        all
    }
}

/// The refusal for a block that is no task definition: its name, and nothing of its text.
fn unreadable(name: &str) -> Error {
    Error::Denied(format!(
        "the task definition {name} does not read as one: it has a key this code does not know, or a tool that is not `tool:<name>`"
    ))
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
    const REVIEW: &str = "# Review a change\n\n```platform task review-change\npurpose = \"Read a change against the decisions it touches.\"\ntools = [\"tool:fetch\"]\nscope = [\"dom-me\"]\nacceptance = [\"The handoff names each decision the change touches.\"]\noutputs = [\"handoff\"]\n```\n";

    fn task(title: &str, definition: Option<&str>) -> dm_ledger::TaskSpec {
        dm_ledger::TaskSpec {
            domain: "dom-me".into(),
            title: title.into(),
            body: "Do it.".into(),
            definition: definition.map(str::to_string),
            ..dm_ledger::TaskSpec::default()
        }
    }

    async fn verify(m: &Memory, id: &str) {
        let mut e = m.store.get("dom-plat", id).await.unwrap().unwrap();
        e.front.trust_tier = TrustTier::Verified;
        m.store.put(&e).await.unwrap();
    }

    /// A session of this root that holds `id`.
    fn holding(dir: &std::path::Path, id: &str) -> Memory {
        let session = Memory::open(dir).unwrap();
        session.set_held_task(id);
        session
    }

    #[tokio::test]
    async fn a_task_definition_is_in_force_once_verified_and_the_newest_one_is() {
        let dir = tempfile::tempdir().unwrap();
        let m = root(dir.path());
        written(&m, REVIEW, false).await;
        assert_eq!(m.task_definition("review-change").await.unwrap(), None);
        let first = written(&m, REVIEW, true).await;
        let def = m.task_definition("review-change").await.unwrap().unwrap();
        assert_eq!(def.reference(), format!("review-change@{first}"));
        assert_eq!(def.tools, ["tool:fetch"]);
        assert_eq!(
            def.autonomy,
            Autonomy::Person,
            "where every definition starts"
        );
        // two verified entries define it: the newer is in force, and the older is still
        // what a task that named it means
        let wider = REVIEW.replace("[\"tool:fetch\"]", "[\"tool:fetch\", \"tool:search\"]");
        let second = written(&m, &wider, true).await;
        let def = m.task_definition("review-change").await.unwrap().unwrap();
        assert_eq!(def.entry, second);
        assert_eq!(def.tools, ["tool:fetch", "tool:search"]);
        let pinned = format!("review-change@{first}");
        let old = m.pinned_definition(&pinned).await.unwrap().unwrap();
        assert_eq!(old.tools, ["tool:fetch"]);
    }

    #[tokio::test]
    async fn a_block_that_is_no_definition_is_refused_and_its_text_is_not_quoted() {
        let dir = tempfile::tempdir().unwrap();
        let m = root(dir.path());
        for body in [
            "```platform task broken\nsecret_key_bluebird = \"TOPSECRET\"\n```\n",
            "```platform task broken\ntools = [\"fetch\"]\n```\n",
        ] {
            let id = written(&m, body, true).await;
            let refused = m.task_definition("broken").await;
            assert!(
                matches!(&refused, Err(Error::Denied(why)) if why.contains("broken") && !why.contains("bluebird")),
                "{refused:?}"
            );
            let bound = m.create_task(task("Uses it", Some("broken"))).await;
            assert!(matches!(&bound, Err(Error::Denied(why)) if !why.contains("bluebird")));
            m.store.demote("dom-plat", &id).await.unwrap();
        }
    }

    #[tokio::test]
    async fn a_task_takes_its_definition_s_entry_checks_and_tools() {
        let dir = tempfile::tempdir().unwrap();
        let m = root(dir.path());
        let entry = written(&m, REVIEW, true).await;
        let filed = m
            .create_task(task("Review the importer", Some("review-change")))
            .await
            .unwrap()
            .task;
        let named = format!("review-change@{entry}");
        assert_eq!(filed.spec.definition.as_deref(), Some(named.as_str()));
        assert_eq!(
            filed.spec.acceptance,
            ["The handoff names each decision the change touches."]
        );
        assert_eq!(filed.spec.requirements, ["tool:fetch"]);
        // its own checks stand in place of the definition's
        let mut own = task("Review the exporter", Some(&named));
        own.acceptance = vec!["The handoff lists the files read.".into()];
        let own = m.create_task(own).await.unwrap().task;
        assert_eq!(own.spec.acceptance, ["The handoff lists the files read."]);
        // a name nothing verified defines is refused, and nothing is filed
        let unknown = m.create_task(task("Deploy", Some("deploy"))).await;
        assert!(matches!(&unknown, Err(Error::Denied(why)) if why.contains("deploy")));
        let l = m.ledger.lock().await;
        let all = l
            .list("dom-me", dm_ledger::Status::Proposed)
            .await
            .unwrap()
            .len()
            + l.list("dom-me", dm_ledger::Status::Open)
                .await
                .unwrap()
                .len();
        assert_eq!(all, 2);
    }

    /// A definition above a session's handling is, to that session, one that does not
    /// exist, and a task that takes its checks is classified as its entry is.
    #[tokio::test]
    async fn a_definition_is_read_under_the_filter_and_its_task_is_no_lower() {
        let dir = tempfile::tempdir().unwrap();
        let m = root(dir.path());
        let id = m
            .write(WriteRequest {
                body: REVIEW.into(),
                domain: Some("dom-plat".into()),
                entry_type: dm_core::EntryType::Convention,
                classification: Some(dm_core::Label::Restricted),
                ..WriteRequest::default()
            })
            .await
            .unwrap()
            .id;
        verify(&m, &id).await;
        let low = Memory::open(dir.path())
            .unwrap()
            .with_handling_cap(dm_core::Label::Public);
        let hidden = low.create_task(task("Review", Some("review-change"))).await;
        let missing = low.create_task(task("Review", Some("no-such"))).await;
        let words = |r: &Result<crate::Created, Error>| match r {
            Err(Error::Denied(why)) => why.replace("review-change", "X").replace("no-such", "X"),
            other => format!("{:?}", other.as_ref().map(|c| &c.task.id)),
        };
        assert_eq!(words(&hidden), words(&missing), "the same refusal for both");
        assert!(words(&hidden).contains("no task definition X is in force"));
        let filed = m
            .create_task(task("Review", Some("review-change")))
            .await
            .unwrap()
            .task;
        assert_eq!(filed.spec.classification, dm_core::Label::Restricted);
    }

    #[tokio::test]
    async fn where_the_owner_asks_for_it_a_task_without_a_definition_is_not_admitted_on_its_own() {
        let dir = tempfile::tempdir().unwrap();
        root(dir.path());
        let path = dir.path().join(crate::GRANTS_FILE);
        let mut g = dm_policy::Grants::load(&path).unwrap();
        g.gates.require_definition = true;
        std::fs::write(&path, g.render().unwrap()).unwrap();
        let m = Memory::open(dir.path()).unwrap();
        let entry = written(&m, REVIEW, true).await;
        let bare = m.file_own(task("Tidy", None)).await.unwrap().task;
        assert_eq!(bare.status, dm_ledger::Status::Proposed);
        assert_eq!(
            m.admission_gap(&bare.spec).as_deref(),
            Some("it names no task definition")
        );
        // the owner is the triage, and admits it by hand
        assert_eq!(
            m.admit(&bare.id).await.unwrap().status,
            dm_ledger::Status::Open
        );
        let defined = m
            .file_own(task("Review", Some("review-change")))
            .await
            .unwrap()
            .task;
        assert_eq!(
            defined.status,
            dm_ledger::Status::Open,
            "admitted on its own"
        );
        // a definition with no checks gives a task nothing to be checked by
        written(
            &m,
            "```platform task loose\npurpose = \"Anything.\"\n```\n",
            true,
        )
        .await;
        let loose = m.file_own(task("Loose", Some("loose"))).await.unwrap().task;
        assert_eq!(loose.status, dm_ledger::Status::Proposed);
        assert!(m.admission_gap(&loose.spec).unwrap().contains("acceptance"));
        // and nobody admits a task whose definition has left force since it was filed
        let mut waits = task("Later", Some("review-change"));
        waits.requirements = vec!["human".into()];
        let waits = m.create_task(waits).await.unwrap().task;
        m.store.demote("dom-plat", &entry).await.unwrap();
        if waits.status == dm_ledger::Status::Proposed {
            let refused = m.admit(&waits.id).await;
            assert!(
                matches!(&refused, Err(Error::Denied(why)) if why.contains("no longer in force")),
                "{refused:?}"
            );
        }
        assert_eq!(
            m.definition_gone(&waits.spec).await.unwrap(),
            Some(format!(
                "its task definition review-change@{entry} is no longer in force"
            ))
        );
    }

    #[tokio::test]
    async fn a_session_that_holds_a_defined_task_reaches_only_the_tools_it_declares() {
        let dir = tempfile::tempdir().unwrap();
        let m = root(dir.path());
        let entry = written(&m, REVIEW, true).await;
        let defined = m
            .file_own(task("Review", Some("review-change")))
            .await
            .unwrap()
            .task;
        let bare = m.file_own(task("Tidy", None)).await.unwrap().task;
        let on_defined = std::sync::Arc::new(holding(dir.path(), &defined.id));
        // through the call a tool goes through: refused for the definition before the
        // grants are asked, and for the grants when the definition lets it by
        let undeclared = on_defined.use_tool("search", serde_json::json!({})).await;
        assert!(
            matches!(&undeclared, Err(Error::Denied(why)) if why.contains("does not declare tool:search")),
            "{undeclared:?}"
        );
        let declared = on_defined.use_tool("fetch", serde_json::json!({})).await;
        assert!(
            matches!(&declared, Err(Error::Denied(why)) if why.contains("no tool fetch is granted")),
            "{declared:?}"
        );
        // a task under no definition is held as before, and so is no task at all
        holding(dir.path(), &bare.id)
            .definition_allows("search")
            .await
            .unwrap();
        m.definition_allows("search").await.unwrap();
        // a wider definition verified later widens nothing for a task that named the first
        let wider = REVIEW.replace("[\"tool:fetch\"]", "[\"tool:fetch\", \"tool:search\"]");
        written(&m, &wider, true).await;
        assert!(on_defined.definition_allows("search").await.is_err());
        // once the entry it named is out of force, the task reaches no tool
        m.store.demote("dom-plat", &entry).await.unwrap();
        let gone = on_defined.definition_allows("fetch").await;
        assert!(matches!(&gone, Err(Error::Denied(why)) if why.contains("no longer in force")));
        // and a check that cannot read the task has not passed
        assert!(holding(dir.path(), "task-me-nothere")
            .definition_allows("fetch")
            .await
            .is_err());
    }

    #[tokio::test]
    async fn what_a_defined_task_files_runs_under_its_definition() {
        let dir = tempfile::tempdir().unwrap();
        let m = root(dir.path());
        let entry = written(&m, REVIEW, true).await;
        let named = format!("review-change@{entry}");
        let parent = m
            .file_own(task("Review", Some("review-change")))
            .await
            .unwrap()
            .task;
        let session = holding(dir.path(), &parent.id);
        let own = session
            .create_task(task("A follow-up", None))
            .await
            .unwrap()
            .task;
        assert_eq!(own.spec.definition.as_deref(), Some(named.as_str()));
        assert_eq!(own.spec.acceptance, parent.spec.acceptance);
        let fanned = m
            .fan_out(&parent.id, &[("Read one".into(), "Read the first.".into())])
            .await
            .unwrap();
        let child = &fanned.children[0];
        assert_eq!(child.spec.definition.as_deref(), Some(named.as_str()));
        let continuation = fanned.continuation.unwrap();
        assert_eq!(
            continuation.spec.definition.as_deref(),
            Some(named.as_str())
        );
        assert_eq!(continuation.spec.acceptance, parent.spec.acceptance);
    }

    #[tokio::test]
    async fn a_session_s_capabilities_hold_the_tools_the_grants_give_it() {
        let dir = tempfile::tempdir().unwrap();
        root(dir.path());
        let path = dir.path().join(crate::GRANTS_FILE);
        let text = std::fs::read_to_string(&path).unwrap();
        let with_tool = format!(
            "{text}\n[taps.fetch]\ndomain = \"dom-me\"\nhosts = []\nceiling = \"public\"\ntool = true\n"
        );
        std::fs::write(&path, with_tool).unwrap();
        let m = Memory::open(dir.path())
            .unwrap()
            .with_manifest(vec!["human".into()]);
        assert_eq!(m.capabilities(), ["human", "tool:fetch"]);
        written(&m, REVIEW, true).await;
        // so a task that requires what its definition declares is one this session claims
        let t = m
            .file_own(task("Review", Some("review-change")))
            .await
            .unwrap()
            .task;
        let claimed = m.claim(&t.id, time::Duration::minutes(5)).await.unwrap();
        assert_eq!(claimed.task.id, t.id);
    }
}
