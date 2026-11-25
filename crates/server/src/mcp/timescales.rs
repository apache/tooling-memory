//! The tools for the long timescales: the registry, the proposals to a personal domain,
//! the outage as a session sees it, the evidence for the horizon scanner, and a recovery
//! bundle with its restore drill.
//!
//! Each tool is one call into [`crate::Memory`], and each refusal is the library's. No
//! tool restores into another root, and no tool declares or ends an outage: the server
//! of a session has access only inside its root. The command line calls the same
//! library functions for those.

use super::{mcp_err, ok, GetParams, McpServer, NoParams};
use crate::timescales::proposal_lines;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{schemars, tool, tool_router, ErrorData as McpError};
use serde::Deserialize;
use serde_json::json;
use time::{Date, OffsetDateTime};

/// Parameters of `registry_check`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct RegistryCheckParams {
    /// File one task for each flag. A flag that has a task gets no second task.
    #[serde(default)]
    pub file: Option<bool>,
}

/// Parameters of `answer_proposal`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct AnswerProposalParams {
    /// The id of the proposal, as `proposals` gives it.
    pub id: String,
    /// True admits the proposal into the record. False refuses it.
    pub admit: bool,
}

/// Parameters of `recovery_drill`.
#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct DrillParams {
    /// The name of a bundle, as `recovery_bundle` gave it. A name, and not a path.
    pub bundle: String,
}

fn today() -> Date {
    OffsetDateTime::now_utc().date()
}

#[tool_router(router = timescales_router, vis = "pub(super)")]
impl McpServer {
    #[tool(
        description = "Check the ownership registry against the grants: each granted domain has a unit with an owner who answers today, the person of each personal domain holds the grants, and each unit names a horizon scanner. `registry` is false when the root has no registry file: the owner of the grants then answers for each domain, and nothing is flagged. With `file`, each flag becomes one task for a person."
    )]
    async fn registry_check(
        &self,
        p: Parameters<RegistryCheckParams>,
    ) -> Result<CallToolResult, McpError> {
        let m = &self.memory;
        let check = m.registry_check(today()).await.map_err(mcp_err)?;
        let (filed, already) = if p.0.file.unwrap_or(false) {
            m.file_registry_flags(today()).await.map_err(mcp_err)?
        } else {
            (Vec::new(), 0)
        };
        ok(json!({
            "registry": check.registry, "flags": check.flags,
            "filed": filed, "already_filed": already,
        }))
    }

    #[tool(
        description = "Read the identifier of a person, `human:<authority>,<YYYY-MM-DD>:<user-id>` or `human:<system>:<id>`, and follow the assertions of the registry to the identifier that names the same person now (`is`). `authority`, `held` and `user_id` are the parts of that identifier. Entries that hold an earlier identifier are not rewritten: use this to read their author."
    )]
    async fn resolve_identity(&self, p: Parameters<GetParams>) -> Result<CallToolResult, McpError> {
        let found = self.memory.resolve_identity(&p.0.id).map_err(mcp_err)?;
        ok(json!(found))
    }

    #[tool(
        description = "For the owner; refused to any other author. The proposals to personal domains that wait for an answer: each with its id, its domain, its author, its time and its title, and no body. A write to a personal domain by anyone but its person is held as a proposal."
    )]
    async fn proposals(&self, _p: Parameters<NoParams>) -> Result<CallToolResult, McpError> {
        let held = self.memory.proposals().map_err(mcp_err)?;
        ok(json!({ "proposals": proposal_lines(&held) }))
    }

    #[tool(
        description = "For the owner; refused to any other author. Answer a proposal to a personal domain. With `admit` true, the proposal goes into the record as it was written, with its author. With `admit` false, the proposal is removed, and the same proposal is not held again."
    )]
    async fn answer_proposal(
        &self,
        p: Parameters<AnswerProposalParams>,
    ) -> Result<CallToolResult, McpError> {
        let p = p.0;
        let written = self
            .memory
            .answer_proposal(&p.id, p.admit)
            .await
            .map_err(mcp_err)?;
        ok(json!({ "id": p.id, "admitted": p.admit, "written": written }))
    }

    #[tool(
        description = "The outage as this session sees it. `outage` is null when no outage is declared. In an outage, reads and changes wait: work from the T0 that this session loaded (`t0_loaded` names the domains), stop work on a task at `confirmed_lease`, and write a handoff, which is queued. This tool answers in an outage too."
    )]
    async fn outage_status(&self, _p: Parameters<NoParams>) -> Result<CallToolResult, McpError> {
        ok(json!(self.memory.outage_status()))
    }

    #[tool(
        description = "For a person; refused to an agent. File the tasks of the horizon scanner of each unit for the period that holds today: an evidence task that an agent can claim, and a review task that requires a person and waits for the evidence task. The tasks of one period are filed once: `filed` is false when the period had them already."
    )]
    async fn horizon_schedule(&self, _p: Parameters<NoParams>) -> Result<CallToolResult, McpError> {
        let scheduled = self
            .memory
            .schedule_horizon(today())
            .await
            .map_err(mcp_err)?;
        ok(json!({ "scheduled": scheduled }))
    }

    #[tool(
        description = "For the owner; refused to any other author. Make a recovery bundle of this root in a new directory under `state/recovery/`: the record, the logs with their cursors, the events of the ledger, the writes held in quarantine and the configuration, with a digest for each file. Returns the `name` of the bundle, its path relative to the root, and the counts of the manifest. Copy the bundle off this machine yourself: no tool sends it."
    )]
    async fn recovery_bundle(&self, _p: Parameters<NoParams>) -> Result<CallToolResult, McpError> {
        let made = self.memory.recovery_bundle_here().await.map_err(mcp_err)?;
        ok(json!(made))
    }

    #[tool(
        description = "For the owner; refused to any other author. Run a restore drill on a bundle under `state/recovery/`, named by `bundle`: the bundle is restored into a scratch root under `state/recovery/`, and each of five cases is reported as `recovered`, `absent` or `failed`. A case is `recovered` only when the bundle held it. The record and the ledger of this root are not written: only the claim epoch file moves."
    )]
    async fn recovery_drill(&self, p: Parameters<DrillParams>) -> Result<CallToolResult, McpError> {
        let run = self
            .memory
            .drill_named(&p.0.bundle)
            .await
            .map_err(mcp_err)?;
        ok(json!(run))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::{kind_of, WriteParams};
    use crate::{tape, Memory, REGISTRY_FILE};
    use serde_json::Value;
    use std::path::Path;
    use std::sync::Arc;

    const OWNER: &str = "human:test,2026-11-01:me";
    const SCANNER: &str = "human:test,2026-11-01:scout";

    fn payload(r: &CallToolResult) -> Value {
        let text = r.content[0].as_text().expect("text block").text.clone();
        serde_json::from_str(&text).expect("json payload")
    }

    /// A server over the record at `dir`, for a session of `author`.
    fn server_of(dir: &Path, author: &str) -> McpServer {
        let memory = Memory::open(dir).unwrap().with_author(author);
        McpServer::new(Arc::new(memory))
    }

    /// A root with the registry `text`: the directory, and the owner's server.
    fn root_with(text: Option<&str>) -> (tempfile::TempDir, McpServer) {
        let dir = tempfile::tempdir().unwrap();
        Memory::init(dir.path(), OWNER, "dom-me").unwrap();
        if let Some(text) = text {
            std::fs::write(dir.path().join(REGISTRY_FILE), text).unwrap();
        }
        let owner = server_of(dir.path(), OWNER);
        (dir, owner)
    }

    fn unit(owner: bool, scanner: bool) -> String {
        let mut text = String::from("[units.u1]\ndomains = [\"dom-me\"]\n");
        if scanner {
            text += &format!("horizon_scanner = \"{SCANNER}\"\n");
        }
        if owner {
            text += &format!("[[units.u1.owners]]\nid = \"{OWNER}\"\n");
        }
        text
    }

    const NONE: fn() -> Parameters<NoParams> = || Parameters(NoParams {});

    fn check(file: bool) -> Parameters<RegistryCheckParams> {
        Parameters(RegistryCheckParams { file: Some(file) })
    }

    fn id(id: &str) -> Parameters<GetParams> {
        Parameters(GetParams { id: id.into() })
    }

    fn refused_with(e: &McpError, words: &str) {
        assert_eq!(e.code, McpError::invalid_params("", None).code, "{e:?}");
        assert!(e.message.contains(words), "{e:?}");
    }

    #[tokio::test]
    async fn registry_check_gives_the_flags_of_the_library_and_files_each_once() {
        let (dir, owner) = root_with(Some(&unit(false, true)));
        let found = payload(&owner.registry_check(check(false)).await.unwrap());
        let library = owner.memory.registry_check(today()).await.unwrap();
        assert_eq!(found["registry"], true);
        assert_eq!(
            found["flags"],
            serde_json::to_value(&library.flags).unwrap()
        );
        assert_eq!(
            found["flags"],
            json!([{ "flag": "ownerless", "domain": "dom-me" }])
        );
        assert_eq!(found["filed"], json!([]));
        let filed = payload(&owner.registry_check(check(true)).await.unwrap());
        assert_eq!(filed["filed"].as_array().unwrap().len(), 1, "{filed}");
        let again = payload(&owner.registry_check(check(true)).await.unwrap());
        assert_eq!(again["filed"], json!([]));
        assert_eq!(again["already_filed"], 1);
        // an agent reads the same flags: the check reads no entry
        let agent = server_of(dir.path(), "agent:x");
        let same = payload(&agent.registry_check(check(false)).await.unwrap());
        assert_eq!(same["flags"], found["flags"]);
        let (_bare, owner) = root_with(None);
        let none = payload(&owner.registry_check(check(true)).await.unwrap());
        assert_eq!(none["registry"], false);
        assert_eq!(none["flags"], json!([]));
    }

    #[tokio::test]
    async fn resolve_identity_gives_the_answer_of_the_library_and_refuses_what_is_no_person() {
        let old = "human:old,2019-05-01:me";
        let registry =
            format!("[[assertions]]\nwas = \"{old}\"\nis = \"{OWNER}\"\nsince = 2026-11-01\n");
        let (dir, owner) = root_with(Some(&registry));
        let found = payload(&owner.resolve_identity(id(old)).await.unwrap());
        let library = owner.memory.resolve_identity(old).unwrap();
        assert_eq!(found, serde_json::to_value(library).unwrap());
        assert_eq!(found["is"], OWNER);
        assert_eq!(found["held"], "2026-11-01");
        let agent = server_of(dir.path(), "agent:x");
        let same = payload(&agent.resolve_identity(id(old)).await.unwrap());
        assert_eq!(same, found);
        let refused = agent.resolve_identity(id("agent:x")).await.unwrap_err();
        refused_with(&refused, "a person is");
    }

    #[tokio::test]
    async fn proposals_and_answer_proposal_are_the_owner_s_and_refused_to_an_agent() {
        let registry = format!("[domains.dom-me]\nperson = \"{OWNER}\"\n");
        let (dir, owner) = root_with(Some(&registry));
        let agent = server_of(dir.path(), "agent:x");
        let write = |body: &str| {
            Parameters(WriteParams {
                body: body.into(),
                ..WriteParams::default()
            })
        };
        let first = "# Commits\n\nThe person prefers small commits that a reviewer reads.\n";
        let second = "# Reviews\n\nThe person reads a review in the morning.\n";
        let held = agent.write(write(first)).await.unwrap_err();
        assert!(held.message.contains("held as a proposal"), "{held:?}");
        agent.write(write(second)).await.unwrap_err();
        let refused = agent.proposals(NONE()).await.unwrap_err();
        refused_with(&refused, "only the owner");
        let listed = payload(&owner.proposals(NONE()).await.unwrap());
        let library = proposal_lines(&owner.memory.proposals().unwrap());
        assert_eq!(listed["proposals"], serde_json::to_value(&library).unwrap());
        assert_eq!(library.len(), 2);
        assert!(!listed.to_string().contains("small commits"), "{listed}");
        let answer = |id: &str, admit| {
            Parameters(AnswerProposalParams {
                id: id.into(),
                admit,
            })
        };
        let (one, two) = (library[0].id.as_str(), library[1].id.as_str());
        let refused = agent.answer_proposal(answer(one, true)).await.unwrap_err();
        refused_with(&refused, "only the owner");
        let admitted = payload(&owner.answer_proposal(answer(one, true)).await.unwrap());
        assert_eq!(admitted["admitted"], true);
        assert_eq!(admitted["written"]["id"], one);
        assert_eq!(owner.memory.get(one).await.unwrap().front.author, "agent:x");
        let dropped = payload(&owner.answer_proposal(answer(two, false)).await.unwrap());
        assert_eq!(dropped["admitted"], false);
        assert_eq!(dropped["written"], Value::Null);
        assert_eq!(owner.memory.proposals().unwrap().len(), 0);
        let gone = owner.answer_proposal(answer(two, true)).await.unwrap_err();
        refused_with(&gone, two);
    }

    #[tokio::test]
    async fn outage_status_answers_any_session_in_an_outage_with_what_the_library_says() {
        let (dir, owner) = root_with(None);
        let agent = server_of(dir.path(), "agent:x");
        let before = payload(&agent.outage_status(NONE()).await.unwrap());
        assert_eq!(before["outage"], Value::Null);
        agent.memory.t0(None).await.unwrap();
        owner.memory.declare_outage("the ledger is moved").unwrap();
        let during = payload(&agent.outage_status(NONE()).await.unwrap());
        let library = serde_json::to_value(agent.memory.outage_status()).unwrap();
        assert_eq!(during, library);
        assert_eq!(during["outage"]["reason"], "the ledger is moved");
        assert_eq!(during["t0_loaded"], json!(["dom-me"]));
        assert_eq!(during["confirmed_lease"], Value::Null);
        // a change waits, in the words of the library
        let waits = owner.recovery_bundle(NONE()).await.unwrap_err();
        refused_with(&waits, "an outage is declared");
    }

    #[tokio::test]
    async fn horizon_schedule_files_the_two_tasks_once_and_is_refused_to_an_agent() {
        let (dir, owner) = root_with(Some(&unit(true, true)));
        let first = payload(&owner.horizon_schedule(NONE()).await.unwrap());
        let scheduled = first["scheduled"].as_array().unwrap();
        assert_eq!(scheduled.len(), 1, "{first}");
        assert_eq!(scheduled[0]["unit"], "u1");
        assert_eq!(scheduled[0]["scanner"], SCANNER);
        assert_eq!(scheduled[0]["filed"], true);
        let library = owner.memory.schedule_horizon(today()).await.unwrap();
        assert_eq!(scheduled[0]["evidence"], library[0].evidence.as_str());
        assert_eq!(scheduled[0]["review"], library[0].review.as_str());
        let again = payload(&owner.horizon_schedule(NONE()).await.unwrap());
        assert_eq!(again["scheduled"][0]["filed"], false);
        let agent = server_of(dir.path(), "agent:x");
        let refused = agent.horizon_schedule(NONE()).await.unwrap_err();
        refused_with(&refused, "only a person schedules");
    }

    fn drill(bundle: &str) -> Parameters<DrillParams> {
        Parameters(DrillParams {
            bundle: bundle.into(),
        })
    }

    #[tokio::test]
    async fn recovery_bundle_stays_under_the_root_and_is_refused_to_an_agent() {
        let (dir, owner) = root_with(None);
        let made = payload(&owner.recovery_bundle(NONE()).await.unwrap());
        let name = made["name"].as_str().unwrap();
        assert_eq!(made["bundle"], format!("state/recovery/{name}"));
        let manifest = dir.path().join("state/recovery").join(name);
        let text = std::fs::read_to_string(manifest.join("recovery.yaml")).unwrap();
        let manifest = dm_core::recovery::RecoveryManifest::from_yaml(&text).unwrap();
        assert_eq!(made["files"], manifest.files.len());
        assert_eq!(made["domains"], 1);
        assert_eq!(made["ledger_events"], manifest.ledger.events);
        assert!(!made.to_string().contains(&dir.path().display().to_string()));
        let agent = server_of(dir.path(), "agent:x");
        let refused = agent.recovery_bundle(NONE()).await.unwrap_err();
        refused_with(&refused, "only the owner");
    }

    #[tokio::test]
    async fn recovery_drill_takes_a_name_and_reports_the_cases_of_the_library() {
        let (dir, owner) = root_with(None);
        let made = payload(&owner.recovery_bundle(NONE()).await.unwrap());
        let name = made["name"].as_str().unwrap();
        let run = payload(&owner.recovery_drill(drill(name)).await.unwrap());
        assert_eq!(run["bundle"], made["bundle"]);
        assert_eq!(run["passed"], true);
        assert_eq!(run["epoch"], 1);
        let cases = run["cases"].as_array().unwrap();
        assert_eq!(cases.len(), 5, "{run}");
        assert!(cases.iter().all(|c| c["outcome"] == "absent"), "{run}");
        let scratch = run["scratch"].as_str().unwrap();
        assert!(scratch.starts_with("state/recovery/drill-"), "{scratch}");
        assert!(dir.path().join(scratch).join("grants.toml").is_file());
        // the live root took no second owner, and the name is never a path
        let outside = tempfile::tempdir().unwrap();
        let path = outside.path().display().to_string();
        for bad in [path.as_str(), "../..", "a/b"] {
            let refused = owner.recovery_drill(drill(bad)).await.unwrap_err();
            refused_with(&refused, "is not a name");
        }
        let agent = server_of(dir.path(), "agent:x");
        let refused = agent.recovery_drill(drill(name)).await.unwrap_err();
        refused_with(&refused, "only the owner");
    }

    #[test]
    fn the_checks_of_the_long_timescales_only_read_and_the_rest_change() {
        let none = json!({});
        for tool in ["resolve_identity", "proposals", "outage_status"] {
            assert_eq!(kind_of(tool, &none), tape::Kind::Read, "{tool}");
        }
        assert_eq!(kind_of("registry_check", &none), tape::Kind::Read);
        let unfiled = json!({ "file": false });
        assert_eq!(kind_of("registry_check", &unfiled), tape::Kind::Read);
        let filed = json!({ "file": true });
        assert_eq!(kind_of("registry_check", &filed), tape::Kind::Mutation);
        for tool in [
            "answer_proposal",
            "horizon_schedule",
            "recovery_bundle",
            "recovery_drill",
        ] {
            assert_eq!(kind_of(tool, &none), tape::Kind::Mutation, "{tool}");
        }
    }
}
