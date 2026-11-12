//! The checkable perimeter: admission of filed tasks, and the gates on writes, as
//! Rego evaluated through regorus with the grants file as data. The built-in policy
//! is `rego/amem.rego` in this crate; a root may carry its own under `policy/*.rego`,
//! which replaces it. At scale the same files run in OPA; here they run in process.
//! Every decision is returned with the policy's version, a digest of its text, so a
//! decision log says which rules produced it.

use crate::{Error, Grants};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// The built-in policy.
pub const BUILTIN: &str = include_str!("../rego/amem.rego");

/// Who is asking, by class. The owner is the grants file's owner; a human is any
/// other person; an agent is a harness session; a tap is a connector; the system is
/// the launcher acting on the owner's behalf, the curation cycle for one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Class {
    /// The grants file's owner.
    Owner,
    /// Any other person.
    Human,
    /// A harness session.
    Agent,
    /// A connector the wrapper hosts.
    Tap,
    /// The launcher acting on the owner's behalf.
    System,
}

impl Class {
    /// The class of an author string, given the owner: `human:<system>:<name>`,
    /// `agent:<harness>`, `tap:<name>`, anything else the system.
    #[must_use]
    pub fn of(author: &str, owner: &str) -> Class {
        if author == owner {
            Class::Owner
        } else if author.starts_with("human:") {
            Class::Human
        } else if author.starts_with("agent:") {
            Class::Agent
        } else if author.starts_with("tap:") {
            Class::Tap
        } else {
            Class::System
        }
    }
}

/// What is being asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// File a task on the ledger.
    FileTask,
    /// Write an entry into the record.
    Write,
    /// Erase an entry.
    Erase,
    /// Read: the session's filter, asked once.
    Read,
    /// Promote an entry's trust tier.
    Promote,
    /// Send a model call: spend against a destination's limits.
    Spend,
    /// Send a tool's outbound request: a fetch or a search, every redirect hop asked
    /// again.
    Fetch,
}

/// How a task is being filed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Filing {
    /// The system on the owner's behalf: the survey, the auditor, the canaries.
    Own,
    /// A session filing from a task it holds: a subagent, a continuation.
    Held,
    /// A session filing on its own account.
    Session,
}

/// The principal.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Principal {
    /// The author string.
    pub id: String,
    /// Its class.
    pub class: Class,
}

/// The task being filed.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TaskInput {
    /// What the task asks of its claimant.
    pub requirements: Vec<String>,
    /// Where it came from.
    pub origin: Option<String>,
}

/// The task the filer holds, when it files from one.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ParentInput {
    /// What the held task asked of its claimant: the ceiling on what may be filed from it.
    pub requirements: Vec<String>,
}

/// The write being gated.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct WriteInput {
    /// The entry this one would replace.
    pub supersedes: Option<String>,
    /// Its trust tier: raw, curated or verified.
    pub supersedes_tier: Option<String>,
    /// Its author.
    pub supersedes_author: Option<String>,
    /// How many entries this session has superseded already.
    pub session_supersedes: usize,
    /// What the injection scan found in the body, by kind; empty when clean.
    #[serde(default)]
    pub findings: Vec<String>,
    /// The entry's type.
    #[serde(default)]
    pub entry_type: String,
    /// The type of the entry it replaces.
    #[serde(default)]
    pub supersedes_type: Option<String>,
}

/// The promotion being decided.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PromoteInput {
    /// `direct`, `propose` or `approve`.
    pub phase: String,
    /// The tier asked for: `curated` or `verified`.
    pub to: String,
    /// The entry's author.
    pub author: String,
    /// Who proposed it; for `approve`, the proposal task's filer.
    pub proposer: String,
}

/// The spend being decided: a model call about to leave for a destination.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SpendInput {
    /// The destination.
    pub destination: String,
    /// Tokens this session has spent there.
    pub session_tokens: u64,
    /// Tokens the root has spent there in the last day.
    pub day_tokens: u64,
}

/// A tool's outbound request, as the host is about to send it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FetchInput {
    /// The tool tap, by its name in the grants.
    pub tool: String,
    /// `GET`, `POST`.
    pub method: String,
    /// `https`, `http`.
    pub scheme: String,
    /// The host, lowercased, without a port.
    pub host: String,
    /// The port, when the URL names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// The path and query. Data leaves in it as surely as in a body.
    pub path: String,
    /// Whether any address the host resolves to is private, loopback or link-local:
    /// a name pointed inward to reach what the machine can reach and the tool may not.
    pub private_address: bool,
    /// Whether every address it resolves to is this machine's loopback.
    pub loopback_address: bool,
    /// The calling session's label: what it has read, and so what a URL or a query
    /// from it may carry.
    pub label: dm_core::Label,
    /// Which request of the call this is: 0 for the first, then one per redirect.
    pub hop: u32,
}

/// One request.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Input {
    /// What is asked.
    pub action: Action,
    /// By whom.
    pub principal: Principal,
    /// On which domain.
    pub domain: String,
    /// How a task is filed, for `file_task`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filing: Option<Filing>,
    /// The task filed, for `file_task`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<TaskInput>,
    /// The task held, for a filing from one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<ParentInput>,
    /// The write, for `write`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write: Option<WriteInput>,
    /// The promotion, for `promote`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promote: Option<PromoteInput>,
    /// The task this session holds, which an exception may be scoped to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held_task: Option<String>,
    /// The outbound request, for `fetch`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetch: Option<FetchInput>,
    /// The spend, for `spend`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spend: Option<SpendInput>,
}

/// Who has to look before this goes further.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Review {
    /// Nobody; it goes through.
    #[default]
    None,
    /// A person.
    Human,
    /// An approver who is not the proposer.
    Approver,
}

/// The answer.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Decision {
    /// Whether the action goes ahead at all.
    pub allow: bool,
    /// For a filing, whether the task is admitted as filed.
    pub admit: bool,
    /// Who has to look before it goes further.
    #[serde(default)]
    pub review: Review,
    /// Why, in a sentence the actor can act on.
    pub reason: String,
    /// The record's policies this decision rests on: those that refused it, or the
    /// exceptions it used.
    #[serde(default, deserialize_with = "as_texts")]
    pub cites: Vec<String>,
    /// The digest of the policy text that decided.
    #[serde(default)]
    pub policy: String,
    /// Each rule that refused, with what a caller needs to act on the refusal: the
    /// rule, why, a reason code, and what would pass.
    #[serde(
        default,
        deserialize_with = "as_denials",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub denials: Vec<Denial>,
    /// What the advisory rules would have refused, had they been enforced. They refuse
    /// nothing: this is how a rule is watched before it is given teeth.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub advisories: Vec<Denial>,
}

/// One rule's refusal, as its module states it. A module is a person's or an agent's
/// text: a field it leaves out is empty, and one that is not a string is read as its
/// JSON, so no module's denial can make a decision fail to parse.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Denial {
    /// The rule.
    #[serde(default, deserialize_with = "as_text")]
    pub policy: String,
    /// Why, in a sentence.
    #[serde(default, deserialize_with = "as_text")]
    pub reason: String,
    /// A short code for the reason, the same for every refusal of its kind.
    #[serde(
        default,
        deserialize_with = "as_some_text",
        skip_serializing_if = "Option::is_none"
    )]
    pub code: Option<String>,
    /// What would pass.
    #[serde(
        default,
        deserialize_with = "as_some_text",
        skip_serializing_if = "Option::is_none"
    )]
    pub remedy: Option<String>,
}

fn text_of(v: serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s,
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// A denial out of whatever a module put in its `deny` set: an object's fields, or
/// for anything else its JSON as the reason.
fn denial_of(v: serde_json::Value) -> Denial {
    match v {
        serde_json::Value::Object(_) => serde_json::from_value(v).unwrap_or_else(|e| Denial {
            policy: String::new(),
            reason: e.to_string(),
            code: None,
            remedy: None,
        }),
        other => Denial {
            policy: String::new(),
            reason: text_of(other),
            code: None,
            remedy: None,
        },
    }
}

fn as_denials<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<Denial>, D::Error> {
    let list = Vec::<serde_json::Value>::deserialize(d)?;
    Ok(list.into_iter().map(denial_of).collect())
}

fn as_texts<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    let list = Vec::<serde_json::Value>::deserialize(d)?;
    Ok(list.into_iter().map(text_of).collect())
}

fn as_text<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    serde_json::Value::deserialize(d).map(text_of)
}

fn as_some_text<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    Ok((!v.is_null()).then(|| text_of(v)))
}

impl Denial {
    /// The refusal in a sentence a caller can act on: the rule, why, the code, and what
    /// would pass.
    #[must_use]
    pub fn said(&self) -> String {
        let mut out = format!("{}: {}", self.policy, self.reason);
        if let Some(code) = &self.code {
            out.push_str(&format!(" (code {code})"));
        }
        if let Some(remedy) = &self.remedy {
            out.push_str(&format!(". To pass: {}.", remedy.trim_end_matches('.')));
        }
        out
    }
}

/// The entries of the enforced rules over a ceiling, which is a domain's. In each
/// domain the enforced rules are counted oldest first, and each one past the ceiling is
/// held back. A rule of the L0 core is never held back and takes no place. Nor does a
/// rule a person signed off: a signature admits that rule and does not raise the
/// ceiling. A new rule can thus never push an older one out.
fn over_ceiling(entries: &[crate::record::Entry], grants: &Grants) -> Vec<String> {
    let Some(ceiling) = grants.gates.max_enforced_rules else {
        return Vec::new();
    };
    let signed = |e: &crate::record::Entry| {
        e.meta
            .signed_off_by
            .as_deref()
            .is_some_and(|by| matches!(Class::of(by, &grants.owner), Class::Owner | Class::Human))
    };
    let mut by_domain: std::collections::BTreeMap<&str, Vec<&crate::record::Entry>> =
        std::collections::BTreeMap::new();
    for e in entries.iter().filter(|e| {
        e.state() == crate::record::Status::Enforced
            && !e.meta.is_exception()
            && !e.meta.is_core()
            && !signed(e)
    }) {
        by_domain.entry(e.domain.as_str()).or_default().push(e);
    }
    let mut over = Vec::new();
    for counted in by_domain.values_mut() {
        counted.sort_by(|a, b| (a.created, &a.entry_id).cmp(&(b.created, &b.entry_id)));
        over.extend(counted.iter().skip(ceiling).map(|e| e.entry_id.clone()));
    }
    over.sort();
    over
}

/// What a session may read, as the policy computes it: the conditions an entry must
/// meet, pushed into the index's query. Narrowed against the grants by the caller.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadScope {
    /// The domains readable.
    pub domains: Vec<String>,
    /// The highest classification readable.
    pub handling: dm_core::Label,
    /// The lowest trust tier returned.
    pub min_tier: dm_core::TrustTier,
}

impl ReadScope {
    /// The filter the index applies: this scope intersected with what the grants
    /// allow, so a policy can narrow a session's reads and never widen them.
    #[must_use]
    pub fn narrow(&self, granted: &dm_core::ReadFilter) -> dm_core::ReadFilter {
        let mut f = granted.clone();
        f.domains = granted
            .domains
            .iter()
            .filter(|d| self.domains.contains(d))
            .cloned()
            .collect();
        if f.domains.is_empty() {
            // an empty list means every domain to the index; a scope that leaves none
            // must read nothing, so it names a domain no entry has
            f.domains = vec![String::new()];
        }
        f.handling = granted.handling.min(self.handling);
        f.min_tier = granted.min_tier.max(self.min_tier);
        f
    }
}

/// A compiled policy over one grants file.
#[derive(Clone)]
pub struct Policy {
    engine: regorus::Engine,
    text: String,
    version: String,
    source: String,
    /// The record's policy entries this was compiled with, enforced or not.
    entries: Vec<crate::record::Entry>,
    /// Modules a verified entry carried that would not load, and why.
    refused: Vec<(String, String)>,
    /// The entries of the rules the ceiling holds at advisory.
    held_back: Vec<String>,
    /// The data the record gave the engine, and when each exception in it lapses: an
    /// expiry is read against the clock at each decision, never compiled in.
    data: serde_json::Value,
    lapses: Vec<(String, time::OffsetDateTime)>,
    /// One engine for each advisory rule: the base with that rule's module alone. What
    /// it denies is reported and never joins the decision, and a module that fails
    /// takes no other rule's report with it.
    watch: Vec<(String, regorus::Engine)>,
}

impl std::fmt::Debug for Policy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Policy")
            .field("version", &self.version)
            .field("source", &self.source)
            .finish()
    }
}

fn digest(text: &str) -> String {
    dm_core::digest(text)[..16].to_string()
}

impl Policy {
    /// The built-in policy over these grants.
    ///
    /// # Errors
    ///
    /// When the policy will not compile, which the built-in one always does.
    pub fn builtin(grants: &Grants) -> Result<Policy, Error> {
        Policy::compile(BUILTIN, "builtin", grants)
    }

    /// The root's own policy, every `policy/*.rego` under it in name order, or the
    /// built-in one when there is none.
    ///
    /// # Errors
    ///
    /// A policy that will not compile, or a file that cannot be read.
    pub fn load(root: &Path, grants: &Grants) -> Result<Policy, Error> {
        let dir = root.join("policy");
        match Policy::root_text(root)? {
            Some(text) => Policy::compile(&text, &dir.display().to_string(), grants),
            None => Policy::builtin(grants),
        }
    }

    /// The text of a root's own policy, its `policy/*.rego` in the order of their
    /// names; none when the root has no such file and the built-in policy stands.
    ///
    /// # Errors
    ///
    /// A policy file that cannot be read.
    pub fn root_text(root: &Path) -> Result<Option<String>, Error> {
        let dir = root.join("policy");
        let mut files: Vec<_> = std::fs::read_dir(&dir)
            .map(|d| {
                d.filter_map(Result::ok)
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|x| x == "rego"))
                    .collect()
            })
            .unwrap_or_default();
        if files.is_empty() {
            return Ok(None);
        }
        files.sort();
        let mut text = String::new();
        for f in &files {
            text.push_str(&std::fs::read_to_string(f).map_err(|e| Error::Io {
                path: f.display().to_string(),
                source: e,
            })?);
            text.push('\n');
        }
        Ok(Some(text))
    }

    /// This policy with the record's policy entries: each verified entry's module is
    /// loaded beside the base, in its own confined package, and the policies and the
    /// exceptions in force are data. A module that will not load is refused, named,
    /// and left out; the base stands.
    ///
    /// # Errors
    ///
    /// When the data cannot be added, which a well-formed grants file never causes.
    pub fn with_record(
        mut self,
        grants: &Grants,
        entries: Vec<crate::record::Entry>,
        now: time::OffsetDateTime,
    ) -> Result<Policy, Error> {
        let mut texts = self.text.clone();
        let over = over_ceiling(&entries, grants);
        let advisory =
            |e: &crate::record::Entry| e.advisory() || (e.enforced() && over.contains(&e.entry_id));
        // an advisory rule's module is evaluated with every decision, in an engine of
        // its own: what it denies is reported, and whatever rule it names it cannot
        // refuse
        let base = self.engine.clone();
        let mut watch = Vec::new();
        for e in entries.iter().filter(|e| e.enforced() || e.advisory()) {
            let Some(rego) = &e.rego else { continue };
            if !crate::record::module_is_confined(rego) {
                self.refused
                    .push((e.meta.id.clone(), "not in package amem.rules".into()));
                continue;
            }
            let name = format!("{}.rego", e.entry_id);
            let mut trial = if advisory(e) {
                base.clone()
            } else {
                self.engine.clone()
            };
            // the version says which modules are loaded and what each one does: the
            // same module refuses as an enforced rule and reports as an advisory one
            let how = if advisory(e) { "advisory" } else { "enforced" };
            match trial.add_policy(name, rego.clone()) {
                Ok(_) => {
                    texts.push_str(&format!("\n# {} {how}\n{rego}", e.meta.id));
                    if advisory(e) {
                        watch.push((e.meta.id.clone(), trial));
                    } else {
                        self.engine = trial;
                    }
                }
                Err(err) => self.refused.push((e.meta.id.clone(), err.to_string())),
            }
        }
        // the rules in force, for what an exception may lift: a rule that is in no
        // decision says nothing here, so an unreviewed entry cannot mark a rule as core
        let policies: Vec<_> = entries
            .iter()
            .filter(|e| !e.meta.is_exception() && e.state() == crate::record::Status::Enforced)
            .map(|e| {
                serde_json::json!({
                    "id": e.meta.id, "layer": e.meta.layer, "core": e.meta.is_core(),
                })
            })
            .collect();
        let exceptions: Vec<_> = entries
            .iter()
            .filter(|e| e.in_force(now))
            .map(|e| {
                serde_json::json!({
                    "id": e.meta.id, "breaches": e.meta.breaches, "scope_task": e.meta.scope_task,
                })
            })
            .collect();
        let data = serde_json::json!({
            "grants": grants, "policies": policies, "exceptions": exceptions,
        });
        self.engine = Policy::with_data(self.engine, &data)?;
        self.watch = watch
            .into_iter()
            .map(|(id, engine)| Ok((id, Policy::with_data(engine, &data)?)))
            .collect::<Result<_, Error>>()?;
        self.version = digest(&texts);
        self.lapses = entries
            .iter()
            .filter(|e| e.in_force(now))
            .filter_map(|e| Some((e.meta.id.clone(), e.lapses()?)))
            .collect();
        self.data = data;
        self.entries = entries;
        self.held_back = over;
        Ok(self)
    }

    /// Where a rule is in its life under this policy: as its entry says, or advisory
    /// when the ceiling on enforced rules holds it back.
    #[must_use]
    pub fn state_of(&self, rule: &crate::record::Entry) -> crate::record::Status {
        if self.held_back.contains(&rule.entry_id) {
            crate::record::Status::Advisory
        } else {
            rule.state()
        }
    }

    /// The rules held at advisory because the ceiling on enforced rules is reached and
    /// no person signed them off, by policy id.
    #[must_use]
    pub fn over_ceiling(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .entries
            .iter()
            .filter(|e| self.held_back.contains(&e.entry_id))
            .map(|e| e.meta.id.clone())
            .collect();
        ids.sort();
        ids.dedup();
        ids
    }

    /// What the advisory rules deny for a request, each module asked on its own. A
    /// module that fails to evaluate reports that under its rule, and changes no
    /// decision and no other rule's report. A denial that names no rule is its
    /// module's rule's.
    fn advisories(&self, input: &Input) -> Vec<Denial> {
        let Ok(input) = serde_json::to_string(input) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (id, watch) in &self.watch {
            let mut engine = watch.clone();
            let said = regorus::Value::from_json_str(&input)
                .map_err(|e| e.to_string())
                .and_then(|value| {
                    engine.set_input(value);
                    engine
                        .eval_rule("data.amem.rules.deny".to_string())
                        .map_err(|e| e.to_string())
                })
                .and_then(|v| v.to_json_str().map_err(|e| e.to_string()))
                .and_then(|j| {
                    serde_json::from_str::<Vec<serde_json::Value>>(&j).map_err(|e| e.to_string())
                });
            match said {
                Ok(list) => out.extend(list.into_iter().map(|v| {
                    let mut d = denial_of(v);
                    if d.policy.is_empty() {
                        d.policy.clone_from(id);
                    }
                    d
                })),
                Err(why) => out.push(Denial {
                    policy: id.clone(),
                    reason: format!("the advisory module did not evaluate: {why}"),
                    code: Some("advisory-failed".into()),
                    remedy: None,
                }),
            }
        }
        out.sort_by(|a, b| (&a.policy, &a.reason).cmp(&(&b.policy, &b.reason)));
        out
    }

    fn with_data(
        mut engine: regorus::Engine,
        data: &serde_json::Value,
    ) -> Result<regorus::Engine, Error> {
        engine.clear_data();
        let data = regorus::Value::from_json_str(&data.to_string())
            .map_err(|e| Error::Policy(e.to_string()))?;
        engine
            .add_data(data)
            .map_err(|e| Error::Policy(e.to_string()))?;
        Ok(engine)
    }

    /// The record's policy entries this was compiled with.
    #[must_use]
    pub fn entries(&self) -> &[crate::record::Entry] {
        &self.entries
    }

    /// Modules that would not load, by policy id, and why.
    #[must_use]
    pub fn refused(&self) -> &[(String, String)] {
        &self.refused
    }

    fn compile(text: &str, source: &str, grants: &Grants) -> Result<Policy, Error> {
        let mut engine = regorus::Engine::new();
        // one file per package in the text, as `amem.rego`; a root's files are
        // concatenated and must share the package
        engine
            .add_policy("amem.rego".into(), text.to_string())
            .map_err(|e| Error::Policy(e.to_string()))?;
        let data = serde_json::json!({ "grants": grants });
        let data = regorus::Value::from_json_str(&data.to_string())
            .map_err(|e| Error::Policy(e.to_string()))?;
        engine
            .add_data(data)
            .map_err(|e| Error::Policy(e.to_string()))?;
        Ok(Policy {
            engine,
            text: text.to_string(),
            version: digest(text),
            source: source.to_string(),
            entries: Vec::new(),
            refused: Vec::new(),
            held_back: Vec::new(),
            data: serde_json::Value::Null,
            lapses: Vec::new(),
            watch: Vec::new(),
        })
    }

    /// The policy text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// A digest of the text: what a decision log cites.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Where the text came from: `builtin`, or the directory.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// The read scope for one principal: `data.amem.read_filter`.
    ///
    /// # Errors
    ///
    /// When the policy fails to evaluate or its answer is not a scope.
    pub fn read_scope(&self, input: &Input) -> Result<ReadScope, Error> {
        let json = self.eval(
            "data.amem.read_filter",
            input,
            time::OffsetDateTime::now_utc(),
        )?;
        serde_json::from_str(&json).map_err(|e| Error::Policy(format!("{e}: {json}")))
    }

    /// The engine as it stands at `now`: without the exceptions that have lapsed since
    /// the policy was compiled.
    fn engine_at(&self, now: time::OffsetDateTime) -> Result<regorus::Engine, Error> {
        let lapsed: Vec<&str> = self
            .lapses
            .iter()
            .filter(|(_, at)| *at <= now)
            .map(|(id, _)| id.as_str())
            .collect();
        if lapsed.is_empty() {
            return Ok(self.engine.clone());
        }
        let mut data = self.data.clone();
        if let Some(list) = data["exceptions"].as_array_mut() {
            list.retain(|e| !lapsed.contains(&e["id"].as_str().unwrap_or_default()));
        }
        Policy::with_data(self.engine.clone(), &data)
    }

    fn eval(&self, rule: &str, input: &Input, now: time::OffsetDateTime) -> Result<String, Error> {
        let mut engine = self.engine_at(now)?;
        let value = regorus::Value::from_json_str(
            &serde_json::to_string(input).map_err(|e| Error::Policy(e.to_string()))?,
        )
        .map_err(|e| Error::Policy(e.to_string()))?;
        engine.set_input(value);
        engine
            .eval_rule(rule.to_string())
            .map_err(|e| Error::Policy(e.to_string()))?
            .to_json_str()
            .map_err(|e| Error::Policy(e.to_string()))
    }

    /// Decide one request.
    ///
    /// # Errors
    ///
    /// When the policy fails to evaluate or its decision is not the shape expected.
    pub fn decide(&self, input: &Input) -> Result<Decision, Error> {
        self.decide_at(input, time::OffsetDateTime::now_utc())
    }

    /// Decide one request as the clock reads `now`: an exception that has lapsed by
    /// then lets nothing through, whenever the policy was compiled.
    ///
    /// # Errors
    ///
    /// As [`Policy::decide`].
    pub fn decide_at(&self, input: &Input, now: time::OffsetDateTime) -> Result<Decision, Error> {
        let json = self.eval("data.amem.decision", input, now)?;
        let mut d: Decision =
            serde_json::from_str(&json).map_err(|e| Error::Policy(format!("{e}: {json}")))?;
        d.policy = self.version.clone();
        d.advisories = self.advisories(input);
        if !d.allow && !d.denials.is_empty() {
            let said: Vec<String> = d.denials.iter().map(Denial::said).collect();
            d.reason = said.join("; ");
        }
        Ok(d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grants() -> Grants {
        let mut g = Grants::starter("human:test:me", "dom-t");
        g.domains
            .get_mut("dom-t")
            .map(|d| d.admit_own = true)
            .unwrap_or_default();
        g.gates.max_supersedes_per_session = 3;
        g
    }

    fn file(class: Class, filing: Filing, reqs: &[&str], parent: Option<&[&str]>) -> Decision {
        let p = Policy::builtin(&grants()).unwrap();
        p.decide(&Input {
            action: Action::FileTask,
            principal: Principal {
                id: "x".into(),
                class,
            },
            domain: "dom-t".into(),
            filing: Some(filing),
            task: Some(TaskInput {
                requirements: reqs.iter().map(|s| (*s).to_string()).collect(),
                origin: None,
            }),
            parent: parent.map(|r| ParentInput {
                requirements: r.iter().map(|s| (*s).to_string()).collect(),
            }),
            write: None,
            promote: None,
            held_task: None,
            spend: None,
            fetch: None,
        })
        .unwrap()
    }

    fn write(class: Class, tier: Option<&str>, done: usize) -> Decision {
        let p = Policy::builtin(&grants()).unwrap();
        p.decide(&Input {
            action: Action::Write,
            principal: Principal {
                id: "x".into(),
                class,
            },
            domain: "dom-t".into(),
            filing: None,
            task: None,
            parent: None,
            write: Some(WriteInput {
                supersedes: tier.map(|_| "mem-t-old".into()),
                supersedes_tier: tier.map(str::to_string),
                supersedes_author: None,
                session_supersedes: done,
                findings: Vec::new(),
                ..WriteInput::default()
            }),
            promote: None,
            held_task: None,
            spend: None,
            fetch: None,
        })
        .unwrap()
    }

    #[test]
    fn the_systems_own_filing_is_admitted_where_the_domain_admits_its_own() {
        let d = file(Class::System, Filing::Own, &[], None);
        assert!(d.allow && d.admit, "{d:?}");
        assert_eq!(d.review, Review::None);
        let d = file(Class::System, Filing::Own, &["human"], None);
        assert!(d.admit && d.review == Review::Human, "{d:?}");
    }

    #[test]
    fn irreversible_work_is_never_admitted_as_filed() {
        let d = file(Class::System, Filing::Own, &["irreversible"], None);
        assert!(d.allow && !d.admit && d.review == Review::Human, "{d:?}");
        let d = file(
            Class::Agent,
            Filing::Held,
            &["irreversible"],
            Some(&["irreversible"]),
        );
        assert!(!d.admit, "even within what the filer holds: {d:?}");
    }

    #[test]
    fn a_session_on_its_own_account_is_born_proposed() {
        let d = file(Class::Agent, Filing::Session, &[], None);
        assert!(d.allow && !d.admit, "{d:?}");
    }

    #[test]
    fn requirements_the_filer_does_not_hold_are_left_for_triage() {
        let d = file(
            Class::Agent,
            Filing::Held,
            &["github"],
            Some(&["github", "shell"]),
        );
        assert!(d.allow && d.admit, "{d:?}");
        let d = file(Class::Agent, Filing::Held, &["deploy"], Some(&["github"]));
        assert!(d.allow && !d.admit && d.review == Review::Human, "{d:?}");
        assert!(d.reason.contains("does not hold"), "{d:?}");
    }

    #[test]
    fn an_ungranted_domain_is_refused() {
        let p = Policy::builtin(&grants()).unwrap();
        let d = p
            .decide(&Input {
                action: Action::FileTask,
                principal: Principal {
                    id: "x".into(),
                    class: Class::Owner,
                },
                domain: "dom-other".into(),
                filing: Some(Filing::Own),
                task: Some(TaskInput::default()),
                parent: None,
                write: None,
                promote: None,
                held_task: None,
                spend: None,
                fetch: None,
            })
            .unwrap();
        assert!(!d.allow, "{d:?}");
    }

    #[test]
    fn replacing_a_verified_entry_needs_an_approver() {
        let d = write(Class::Agent, Some("verified"), 0);
        assert!(!d.allow && d.review == Review::Approver, "{d:?}");
        let d = write(Class::Owner, Some("verified"), 0);
        assert!(d.allow, "{d:?}");
        let d = write(Class::Agent, Some("raw"), 0);
        assert!(d.allow, "{d:?}");
    }

    #[test]
    fn the_blast_radius_gate_closes_at_the_grants_number() {
        assert!(write(Class::Agent, Some("raw"), 2).allow);
        let d = write(Class::Agent, Some("raw"), 3);
        assert!(!d.allow && d.review == Review::Human, "{d:?}");
        assert!(d.reason.contains("blast radius"), "{d:?}");
        // a plain write past the number is still a write
        assert!(write(Class::Agent, None, 10).allow);
        // the owner is not an agent
        assert!(write(Class::Owner, Some("raw"), 10).allow);
    }

    #[test]
    fn a_body_the_scan_flagged_is_quarantined_from_anyone_but_the_owner() {
        let p = Policy::builtin(&grants()).unwrap();
        let ask = |class| {
            p.decide(&Input {
                action: Action::Write,
                principal: Principal {
                    id: "x".into(),
                    class,
                },
                domain: "dom-t".into(),
                filing: None,
                task: None,
                parent: None,
                write: Some(WriteInput {
                    findings: vec!["instruction".into(), "broadcast".into()],
                    ..WriteInput::default()
                }),
                promote: None,
                held_task: None,
                spend: None,
                fetch: None,
            })
            .unwrap()
        };
        let d = ask(Class::Agent);
        assert!(!d.allow && d.review == Review::Human, "{d:?}");
        assert!(
            d.reason.contains("quarantined") && d.reason.contains("broadcast"),
            "{d:?}"
        );
        assert!(ask(Class::Owner).allow);
    }

    #[test]
    fn erasure_is_the_owners() {
        let p = Policy::builtin(&grants()).unwrap();
        let ask = |class| {
            p.decide(&Input {
                action: Action::Erase,
                principal: Principal {
                    id: "x".into(),
                    class,
                },
                domain: "dom-t".into(),
                filing: None,
                task: None,
                parent: None,
                write: None,
                promote: None,
                held_task: None,
                spend: None,
                fetch: None,
            })
            .unwrap()
        };
        assert!(ask(Class::Owner).allow);
        assert!(!ask(Class::Agent).allow);
    }

    fn read_input(class: Class) -> Input {
        Input {
            action: Action::Read,
            principal: Principal {
                id: "x".into(),
                class,
            },
            domain: String::new(),
            filing: None,
            task: None,
            parent: None,
            write: None,
            promote: None,
            held_task: None,
            spend: None,
            fetch: None,
        }
    }

    #[test]
    fn the_built_in_read_scope_is_the_grants() {
        let g = grants();
        let p = Policy::builtin(&g).unwrap();
        let scope = p.read_scope(&read_input(Class::Agent)).unwrap();
        assert_eq!(scope.domains, vec!["dom-t".to_string()]);
        assert_eq!(scope.handling, g.handling);
        assert_eq!(scope.min_tier, dm_core::TrustTier::Raw);
        assert_eq!(scope.narrow(&g.read_filter()), g.read_filter());
    }

    #[test]
    fn a_policy_narrows_reads_and_cannot_widen_them() {
        let g = grants();
        let text = BUILTIN.replace(
            "read_filter := base_filter\n",
            "read_filter := object.union(base_filter, {\"handling\": \"public\", \"min_tier\": \"curated\", \"domains\": [\"dom-t\", \"dom-elsewhere\"]}) if class == \"agent\"\n\nread_filter := base_filter if class != \"agent\"\n",
        );
        assert_ne!(text, BUILTIN, "the rule was replaced");
        let p = Policy::compile(&text, "test", &g).unwrap();
        let f = p
            .read_scope(&read_input(Class::Agent))
            .unwrap()
            .narrow(&g.read_filter());
        assert_eq!(f.handling, dm_core::Label::Public);
        assert_eq!(f.min_tier, dm_core::TrustTier::Curated);
        assert_eq!(
            f.domains,
            vec!["dom-t".to_string()],
            "a domain the grants lack is not added"
        );
        let owner = p
            .read_scope(&read_input(Class::Owner))
            .unwrap()
            .narrow(&g.read_filter());
        assert_eq!(owner, g.read_filter());
    }

    #[test]
    fn a_scope_with_no_domains_reads_nothing_rather_than_everything() {
        let g = grants();
        let none = ReadScope {
            domains: Vec::new(),
            handling: dm_core::Label::Restricted,
            min_tier: dm_core::TrustTier::Raw,
        };
        let f = none.narrow(&g.read_filter());
        assert_eq!(f.domains, vec![String::new()]);
    }

    fn rule_entry(
        id: &str,
        layer: u8,
        tier: dm_core::TrustTier,
        extra: &str,
        rego: bool,
    ) -> crate::record::Entry {
        let module = format!(
            "package amem.rules\nimport rego.v1\ndeny contains {{\"policy\": \"{id}\", \"reason\": \"an agent superseded a decision\"}} if {{\n\tinput.principal.class == \"agent\"\n\tinput.write.supersedes_type == \"decision\"\n}}"
        );
        let mut body = format!("```policy\nid = \"{id}\"\nlayer = {layer}\n{extra}\n```\n");
        if rego {
            body.push_str(&format!("\n```rego\n{module}\n```\n"));
        }
        crate::record::parse(&format!("mem-p-{id}"), tier, &body).unwrap()
    }

    fn supersede_decision(p: &Policy, held: Option<&str>) -> Decision {
        p.decide(&supersede_input(held)).unwrap()
    }

    fn supersede_input(held: Option<&str>) -> Input {
        Input {
            action: Action::Write,
            principal: Principal {
                id: "agent:x".into(),
                class: Class::Agent,
            },
            domain: "dom-t".into(),
            filing: None,
            task: None,
            parent: None,
            write: Some(WriteInput {
                supersedes: Some("mem-t-d".into()),
                supersedes_tier: Some("raw".into()),
                supersedes_type: Some("decision".into()),
                entry_type: "decision".into(),
                ..WriteInput::default()
            }),
            promote: None,
            held_task: held.map(str::to_string),
            spend: None,
            fetch: None,
        }
    }

    /// An expiry is read against the clock at each decision: a policy compiled while
    /// an exception was in force refuses once the exception has lapsed.
    #[test]
    fn an_exception_lapses_at_its_expiry_whenever_the_policy_was_compiled() {
        use dm_core::TrustTier::Verified;
        let g = grants();
        let rule = rule_entry("pol-L2-031", 2, Verified, "", true);
        let exception = |id: &str, expires: &str| {
            rule_entry(
                id,
                2,
                Verified,
                &format!(
                    "breaches = \"pol-L2-031\"\nscope_task = \"task-t-1\"\nexpires = \"{expires}\""
                ),
                false,
            )
        };
        let compiled = time::macros::datetime!(2026-11-01 00:00 UTC);
        let short = exception("exc-2026-010", "2026-11-02T00:00:00Z");
        let p = Policy::builtin(&g)
            .unwrap()
            .with_record(&g, vec![rule.clone(), short.clone()], compiled)
            .unwrap();
        let input = supersede_input(Some("task-t-1"));
        let before = time::macros::datetime!(2026-11-01 12:00 UTC);
        let at = time::macros::datetime!(2026-11-02 00:00 UTC);
        assert!(p.decide_at(&input, before).unwrap().allow);
        let d = p.decide_at(&input, at).unwrap();
        assert!(!d.allow, "{d:?}");
        assert_eq!(d.cites, ["pol-L2-031"]);
        // an exception that still runs is kept when another lapses
        let long = exception("exc-2026-011", "2026-12-01T00:00:00Z");
        let p = Policy::builtin(&g)
            .unwrap()
            .with_record(&g, vec![rule, short, long], compiled)
            .unwrap();
        let d = p.decide_at(&input, at).unwrap();
        assert!(d.allow, "{d:?}");
        assert_eq!(d.cites, ["exc-2026-011"]);
    }

    fn with(g: &Grants, entries: Vec<crate::record::Entry>) -> Policy {
        Policy::builtin(g)
            .unwrap()
            .with_record(g, entries, time::OffsetDateTime::now_utc())
            .unwrap()
    }

    /// A rule's life: proposed and retired rules are in no decision, an advisory rule is
    /// reported and refuses nothing, and an enforced one refuses.
    #[test]
    fn a_rule_refuses_only_once_enforced_and_is_watched_while_advisory() {
        use dm_core::TrustTier::{Curated, Raw, Verified};
        let g = grants();
        let rule = |tier, status: &str| {
            rule_entry(
                "pol-L2-031",
                2,
                tier,
                &format!("status = \"{status}\""),
                true,
            )
        };
        // below verified a rule is in no decision: nobody's module is evaluated that a
        // person has not verified
        for (tier, status) in [
            (Raw, "advisory"),
            (Raw, "enforced"),
            (Curated, "advisory"),
            (Curated, "enforced"),
            (Verified, "proposed"),
            (Verified, "retired"),
        ] {
            let d = supersede_decision(&with(&g, vec![rule(tier, status)]), None);
            assert!(
                d.allow && d.advisories.is_empty(),
                "{status} at {tier}: {d:?}"
            );
        }
        let d = supersede_decision(&with(&g, vec![rule(Verified, "advisory")]), None);
        assert!(d.allow, "{d:?}");
        assert_eq!(d.advisories.len(), 1, "{d:?}");
        assert_eq!(d.advisories[0].policy, "pol-L2-031");
        assert!(d.cites.is_empty() && d.denials.is_empty(), "{d:?}");
        let d = supersede_decision(&with(&g, vec![rule(Verified, "enforced")]), None);
        assert!(!d.allow && d.advisories.is_empty(), "{d:?}");
    }

    /// A refusal says what would pass: the rule's module gives a code and a remedy, and
    /// the decision carries them.
    #[test]
    fn a_denial_carries_the_rule_s_reason_code_and_remedy() {
        let g = grants();
        let body = "```policy\nid = \"pol-L2-040\"\nlayer = 2\n```\n\n```rego\npackage amem.rules\nimport rego.v1\ndeny contains {\"policy\": \"pol-L2-040\", \"reason\": \"an agent superseded a decision\", \"code\": \"decision-by-agent\", \"remedy\": \"propose the change to the decision's owner\"} if {\n\tinput.principal.class == \"agent\"\n\tinput.write.supersedes_type == \"decision\"\n}\n```\n";
        let rule = crate::record::parse("mem-p-40", dm_core::TrustTier::Verified, body).unwrap();
        let d = supersede_decision(&with(&g, vec![rule]), None);
        assert!(!d.allow);
        assert_eq!(
            d.denials,
            vec![Denial {
                policy: "pol-L2-040".into(),
                reason: "an agent superseded a decision".into(),
                code: Some("decision-by-agent".into()),
                remedy: Some("propose the change to the decision's owner".into()),
            }]
        );
    }

    /// A rule written at `day` of November 2026.
    fn dated(mut e: crate::record::Entry, day: u8) -> crate::record::Entry {
        let at = time::macros::datetime!(2026-11-01 00:00 UTC) + time::Duration::days(day.into());
        e.created = Some(at);
        e
    }

    /// The ceiling: the enforced rules of a domain past it, oldest first, are held at
    /// advisory until a person signs one off, and a signature admits that rule alone.
    #[test]
    fn a_rule_over_the_ceiling_is_advisory_until_a_person_signs_it_off() {
        use crate::record::Status::{Advisory, Enforced};
        use dm_core::TrustTier::Verified;
        let mut g = grants();
        g.gates.max_enforced_rules = Some(1);
        // the newer rule is the one held back, whatever its id: a new rule pushes no
        // older one out
        let old = dated(rule_entry("pol-L2-900", 2, Verified, "", true), 1);
        let new = dated(rule_entry("a-new-rule", 2, Verified, "", true), 2);
        let p = with(&g, vec![new.clone(), old.clone()]);
        assert_eq!(p.over_ceiling(), ["a-new-rule"]);
        let d = supersede_decision(&p, None);
        assert_eq!(d.cites, ["pol-L2-900"], "the older refuses: {d:?}");
        assert_eq!(d.advisories.len(), 1);
        assert_eq!(d.advisories[0].policy, "a-new-rule");
        // signed off by a person, the newer is enforced, and takes no place
        let by = |who: &str| {
            let extra = format!("signed_off_by = \"{who}\"");
            dated(rule_entry("a-new-rule", 2, Verified, &extra, true), 2)
        };
        let third = dated(rule_entry("pol-L2-903", 2, Verified, "", true), 3);
        let p = with(&g, vec![old.clone(), by("human:test:lead"), third.clone()]);
        assert_eq!(
            p.over_ceiling(),
            ["pol-L2-903"],
            "the signature raised nothing"
        );
        let mut cites = supersede_decision(&p, None).cites;
        cites.sort();
        assert_eq!(cites, ["a-new-rule", "pol-L2-900"]);
        assert_eq!(p.state_of(&p.entries()[2]), Advisory);
        assert_eq!(p.state_of(&p.entries()[0]), Enforced);
        // a sign-off is a person's: an agent's name, or none, admits nothing
        for who in ["agent:a", ""] {
            let p = with(&g, vec![old.clone(), by(who)]);
            assert_eq!(p.over_ceiling(), ["a-new-rule"], "signed by {who:?}");
        }
        // the ceiling is a domain's: the same rules in two domains are each under it
        let mut elsewhere = new.clone();
        elsewhere.domain = "dom-other".into();
        assert!(with(&g, vec![old.clone(), elsewhere])
            .over_ceiling()
            .is_empty());
        // a rule with no module is counted as any enforced rule is
        let plain = dated(rule_entry("pol-L2-800", 2, Verified, "", false), 0);
        assert_eq!(
            with(&g, vec![plain, old.clone()]).over_ceiling(),
            ["pol-L2-900"]
        );
        // with no ceiling, nothing is held back
        g.gates.max_enforced_rules = None;
        assert!(with(&g, vec![old, new, third]).over_ceiling().is_empty());
    }

    /// The L0 core is never held back by the ceiling, at any ceiling, and takes no
    /// place under it.
    #[test]
    fn the_ceiling_never_holds_back_the_l0_core() {
        use dm_core::TrustTier::Verified;
        let mut g = grants();
        let core = dated(rule_entry("pol-L0-001", 0, Verified, "", true), 5);
        let older = dated(rule_entry("a-new-rule", 2, Verified, "", true), 1);
        for ceiling in [0, 1] {
            g.gates.max_enforced_rules = Some(ceiling);
            let p = with(&g, vec![core.clone(), older.clone()]);
            assert!(!p.over_ceiling().contains(&"pol-L0-001".to_string()));
            let d = supersede_decision(&p, None);
            assert!(
                d.cites.contains(&"pol-L0-001".to_string()),
                "{ceiling}: {d:?}"
            );
        }
        g.gates.max_enforced_rules = Some(1);
        assert!(with(&g, vec![core, older]).over_ceiling().is_empty());
    }

    /// A rule with its own module text, at a status.
    fn rule_with(id: &str, status: &str, module: &str) -> crate::record::Entry {
        let body = format!(
            "```policy\nid = \"{id}\"\nlayer = 2\nstatus = \"{status}\"\n```\n\n```rego\npackage amem.rules\nimport rego.v1\n{module}\n```\n"
        );
        crate::record::parse(
            &format!("mem-p-{id}-{status}"),
            dm_core::TrustTier::Verified,
            &body,
        )
        .unwrap()
    }

    /// An advisory module refuses nothing, whatever rule its denial names: another
    /// rule's id, an id nobody has, or none at all.
    #[test]
    fn an_advisory_module_refuses_nothing_whatever_it_names() {
        use dm_core::TrustTier::Verified;
        let g = grants();
        let enforced = rule_entry("pol-L2-001", 2, Verified, "", false);
        for named in [
            "\"policy\": \"pol-L2-001\", ",
            "\"policy\": \"nobody\", ",
            "",
        ] {
            let module = format!("deny contains {{{named}\"reason\": \"always\"}} if true");
            let watch = rule_with("pol-L2-050", "advisory", &module);
            let p = with(&g, vec![enforced.clone(), watch]);
            let d = supersede_decision(&p, None);
            assert!(d.allow, "{named}: {d:?}");
            assert!(d.cites.is_empty() && d.denials.is_empty(), "{named}: {d:?}");
            assert_eq!(d.advisories.len(), 1, "{named}: {d:?}");
        }
        // a second entry that reuses an enforced rule's id as advisory switches nothing off
        let real = rule_entry("pol-L2-031", 2, Verified, "", true);
        let shadow = rule_with(
            "pol-L2-031",
            "advisory",
            "deny contains {\"policy\": \"x\"} if false",
        );
        let d = supersede_decision(&with(&g, vec![real, shadow]), None);
        assert!(!d.allow, "{d:?}");
        assert_eq!(d.cites, ["pol-L2-031"]);
    }

    /// A denial is a module's text: a field that is not a string is read as its JSON,
    /// and a missing one is empty. The decision is made either way.
    #[test]
    fn a_malformed_denial_does_not_break_the_decision() {
        let g = grants();
        let odd = rule_with(
            "pol-L2-060",
            "enforced",
            "deny contains {\"reason\": \"no id\", \"code\": 7, \"remedy\": [\"a\"]} if true",
        );
        let d = supersede_decision(&with(&g, vec![odd.clone()]), None);
        assert!(!d.allow, "{d:?}");
        assert_eq!(d.denials[0].code.as_deref(), Some("7"));
        assert_eq!(d.denials[0].remedy.as_deref(), Some("[\"a\"]"));
        assert_eq!(d.denials[0].policy, "");
        // the same module as advisory: reported, and everything goes ahead
        let watch = rule_with(
            "pol-L2-060",
            "advisory",
            odd.rego
                .as_deref()
                .unwrap()
                .split_once("rego.v1\n")
                .unwrap()
                .1,
        );
        let d = supersede_decision(&with(&g, vec![watch]), None);
        assert!(d.allow && d.advisories.len() == 1, "{d:?}");
        // a denial whose rule is not a string, and one that is no object at all
        for said in [
            "{\"policy\": {\"a\": 1}, \"reason\": 3}",
            "\"x\"",
            "7",
            "[1, 2]",
        ] {
            let module = format!("deny contains {said} if true");
            let d = supersede_decision(
                &with(&g, vec![rule_with("pol-L2-061", "enforced", &module)]),
                None,
            );
            assert!(!d.allow, "{said}: {d:?}");
            assert_eq!(d.denials.len(), 1, "{said}: {d:?}");
        }
    }

    /// Each advisory module is asked on its own: one that fails, or that has no
    /// `deny`, reports that and takes no other rule's report with it.
    #[test]
    fn a_bad_advisory_module_takes_no_other_report_with_it() {
        let g = grants();
        let good = rule_with(
            "pol-L2-070",
            "advisory",
            "deny contains {\"policy\": \"pol-L2-070\", \"reason\": \"always\"} if true",
        );
        for bad in ["deny contains \"x\" if true", "deny := 5", "allow := true"] {
            let p = with(
                &g,
                vec![good.clone(), rule_with("pol-L2-071", "advisory", bad)],
            );
            let d = supersede_decision(&p, None);
            assert!(d.allow, "{bad}: {d:?}");
            assert!(
                d.advisories
                    .iter()
                    .any(|a| a.policy == "pol-L2-070" && a.reason == "always"),
                "{bad}: {d:?}"
            );
            assert!(
                d.advisories.iter().any(|a| a.policy == "pol-L2-071"),
                "{bad}: {d:?}"
            );
        }
        // the version says what each module does
        let module = "deny contains {\"reason\": \"always\"} if true";
        let advisory = with(&g, vec![rule_with("pol-L2-072", "advisory", module)]);
        let enforced = with(&g, vec![rule_with("pol-L2-072", "enforced", module)]);
        assert_ne!(advisory.version(), enforced.version());
        assert_ne!(advisory.version(), Policy::builtin(&g).unwrap().version());
    }

    /// An exception whose block says retired is in force for nothing.
    #[test]
    fn a_retired_exception_lets_nothing_through() {
        use dm_core::TrustTier::Verified;
        let g = grants();
        let rule = rule_entry("pol-L2-031", 2, Verified, "", true);
        let exc = rule_entry(
            "exc-2026-020",
            2,
            Verified,
            "breaches = \"pol-L2-031\"\nscope_task = \"task-t-1\"\nexpires = \"2099-01-01T00:00:00Z\"\nstatus = \"retired\"",
            false,
        );
        assert!(!supersede_decision(&with(&g, vec![rule, exc]), Some("task-t-1")).allow);
    }

    /// The core of L0 takes no exception; an L0 rule marked as outside the core takes one.
    #[test]
    fn only_the_marked_core_of_l0_takes_no_exception() {
        use dm_core::TrustTier::Verified;
        let g = grants();
        let exc = rule_entry(
            "exc-2026-003",
            0,
            Verified,
            "breaches = \"pol-L0-002\"\nscope_task = \"task-t-1\"\nexpires = \"2099-01-01T00:00:00Z\"",
            false,
        );
        let outside = rule_entry("pol-L0-002", 0, Verified, "core = false", true);
        let p = with(&g, vec![outside, exc.clone()]);
        assert!(supersede_decision(&p, Some("task-t-1")).allow);
        let core = rule_entry("pol-L0-002", 0, Verified, "core = true", true);
        let p = with(&g, vec![core, exc]);
        assert!(!supersede_decision(&p, Some("task-t-1")).allow);
    }

    #[test]
    fn a_verified_rule_from_the_record_refuses_and_a_scoped_exception_lets_one_task_through() {
        use dm_core::TrustTier::{Raw, Verified};
        let g = grants();
        let now = time::OffsetDateTime::now_utc();
        // raw: inert
        let p = Policy::builtin(&g)
            .unwrap()
            .with_record(&g, vec![rule_entry("pol-L2-031", 2, Raw, "", true)], now)
            .unwrap();
        assert!(
            supersede_decision(&p, None).allow,
            "a raw rule is not enforced"
        );
        // verified: enforced, cited
        let rule = rule_entry("pol-L2-031", 2, Verified, "", true);
        let p = Policy::builtin(&g)
            .unwrap()
            .with_record(&g, vec![rule.clone()], now)
            .unwrap();
        let d = supersede_decision(&p, Some("task-t-1"));
        assert!(!d.allow, "{d:?}");
        assert_eq!(d.cites, vec!["pol-L2-031".to_string()]);
        assert!(d.reason.contains("pol-L2-031"), "{d:?}");
        assert_ne!(
            p.version(),
            Policy::builtin(&g).unwrap().version(),
            "the rule is in the version"
        );
        // an exception in force, scoped to one task
        let exc = rule_entry(
            "exc-2026-001",
            2,
            Verified,
            "breaches = \"pol-L2-031\"\nscope_task = \"task-t-1\"\nexpires = \"2099-01-01T00:00:00Z\"",
            false,
        );
        let p = Policy::builtin(&g)
            .unwrap()
            .with_record(&g, vec![rule.clone(), exc.clone()], now)
            .unwrap();
        let d = supersede_decision(&p, Some("task-t-1"));
        assert!(d.allow, "{d:?}");
        assert_eq!(
            d.cites,
            vec!["exc-2026-001".to_string()],
            "the override is cited"
        );
        assert!(
            !supersede_decision(&p, Some("task-t-2")).allow,
            "another task is not covered"
        );
        assert!(!supersede_decision(&p, None).allow);
        // an L0 rule takes no exception
        let core = rule_entry("pol-L0-001", 0, Verified, "", true);
        let exc0 = rule_entry(
            "exc-2026-002",
            0,
            Verified,
            "breaches = \"pol-L0-001\"\nscope_task = \"task-t-1\"\nexpires = \"2099-01-01T00:00:00Z\"",
            false,
        );
        let p = Policy::builtin(&g)
            .unwrap()
            .with_record(&g, vec![core, exc0], now)
            .unwrap();
        assert!(
            !supersede_decision(&p, Some("task-t-1")).allow,
            "L0 is not overridable"
        );
    }

    #[test]
    fn a_module_outside_the_rules_package_is_refused_and_the_base_stands() {
        let g = grants();
        let body = "```policy\nid = \"pol-L2-099\"\nlayer = 2\n```\n\n```rego\npackage amem\nimport rego.v1\nbase_decision := {\"allow\": true}\n```\n";
        let e = crate::record::parse("mem-p-x", dm_core::TrustTier::Verified, body).unwrap();
        let p = Policy::builtin(&g)
            .unwrap()
            .with_record(&g, vec![e], time::OffsetDateTime::now_utc())
            .unwrap();
        assert_eq!(p.refused().len(), 1);
        let d = write(Class::Agent, Some("verified"), 0);
        assert!(!d.allow, "the base gate still holds: {d:?}");
    }

    #[test]
    fn a_model_call_over_a_spend_limit_does_not_leave() {
        let mut g = grants();
        g.spend.insert(
            "llmao".into(),
            crate::SpendLimit {
                session_tokens: Some(1000),
                day_tokens: Some(5000),
            },
        );
        let p = Policy::builtin(&g).unwrap();
        let ask = |dest: &str, session: u64, day: u64| {
            p.decide(&Input {
                action: Action::Spend,
                principal: Principal {
                    id: "agent:x".into(),
                    class: Class::Agent,
                },
                domain: String::new(),
                filing: None,
                task: None,
                parent: None,
                write: None,
                promote: None,
                held_task: None,
                spend: Some(SpendInput {
                    destination: dest.into(),
                    session_tokens: session,
                    day_tokens: day,
                }),
                fetch: None,
            })
            .unwrap()
        };
        assert!(ask("llmao", 999, 100).allow);
        let d = ask("llmao", 1000, 100);
        assert!(!d.allow && d.reason.contains("this session"), "{d:?}");
        let d = ask("llmao", 10, 5000);
        assert!(!d.allow && d.reason.contains("last day"), "{d:?}");
        assert!(
            ask("anthropic", 1_000_000, 1_000_000).allow,
            "no limit set there"
        );
    }

    fn fetch_grants() -> Grants {
        let mut g = grants();
        g.taps.insert(
            "fetch".into(),
            crate::TapGrant {
                domain: "dom-t".into(),
                hosts: vec!["docs.python.org".into(), "*.apache.org".into()],
                ceiling: dm_core::Label::Public,
                credential: None,
                tool: true,
                trusted: Some(crate::Trusted {
                    hosts: vec!["*.apache.org".into()],
                    ceiling: dm_core::Label::Internal,
                }),
                methods: Vec::new(),
                credential_header: None,
                loopback: false,
                config: None,
                results: None,
                fresh: false,
                hosted: None,
                browser: false,
            },
        );
        g
    }

    fn fetch(p: &Policy, f: FetchInput) -> Decision {
        p.decide(&Input {
            action: Action::Fetch,
            principal: Principal {
                id: "agent:x".into(),
                class: Class::Agent,
            },
            domain: String::new(),
            filing: None,
            task: None,
            parent: None,
            write: None,
            promote: None,
            held_task: None,
            spend: None,
            fetch: Some(f),
        })
        .unwrap()
    }

    fn to(host: &str, label: dm_core::Label) -> FetchInput {
        FetchInput {
            tool: "fetch".into(),
            method: "GET".into(),
            scheme: "https".into(),
            host: host.into(),
            port: None,
            path: "/".into(),
            private_address: false,
            loopback_address: false,
            label,
            hop: 0,
        }
    }

    #[test]
    fn a_tool_reaches_its_granted_hosts_at_the_label_they_are_trusted_with() {
        use dm_core::Label::{Internal, Public, Restricted};
        let p = Policy::builtin(&fetch_grants()).unwrap();
        assert!(fetch(&p, to("docs.python.org", Public)).allow);
        assert!(fetch(&p, to("lists.apache.org", Public)).allow, "a pattern");
        for host in ["evil.example", "apache.org.evil.example", "evilapache.org"] {
            let d = fetch(&p, to(host, Public));
            assert!(!d.allow && d.reason.contains("not a host"), "{host}: {d:?}");
            assert_eq!(d.cites, vec!["pol-fetch-reach".to_string()]);
        }
        // a session that has read internal entries: only the trusted hosts
        let d = fetch(&p, to("docs.python.org", Internal));
        assert!(!d.allow && d.reason.contains("has read internal"), "{d:?}");
        assert!(fetch(&p, to("lists.apache.org", Internal)).allow);
        assert!(!fetch(&p, to("lists.apache.org", Restricted)).allow);
    }

    #[test]
    fn a_tool_sends_nothing_in_the_clear_inward_or_by_another_method() {
        use dm_core::Label::Public;
        let mut g = fetch_grants();
        let p = Policy::builtin(&g).unwrap();
        let clear = FetchInput {
            scheme: "http".into(),
            ..to("docs.python.org", Public)
        };
        assert!(fetch(&p, clear).reason.contains("https only"));
        let inward = FetchInput {
            private_address: true,
            ..to("docs.python.org", Public)
        };
        let d = fetch(&p, inward);
        assert!(
            !d.allow && d.reason.contains("does not reach inward"),
            "{d:?}"
        );
        let post = FetchInput {
            method: "POST".into(),
            ..to("docs.python.org", Public)
        };
        assert!(fetch(&p, post).reason.contains("not a method"));
        let not_tool = FetchInput {
            tool: "github-issues".into(),
            ..to("docs.python.org", Public)
        };
        assert!(fetch(&p, not_tool).reason.contains("not granted as a tool"));
        // loopback, for a fake in development, only when the grant says so
        let local = FetchInput {
            scheme: "http".into(),
            private_address: true,
            loopback_address: true,
            ..to("localhost", Public)
        };
        assert!(!fetch(&p, local.clone()).allow);
        if let Some(t) = g.taps.get_mut("fetch") {
            t.loopback = true;
            t.hosts.push("localhost".into());
        }
        let p = Policy::builtin(&g).unwrap();
        assert!(fetch(&p, local).allow);
    }

    #[test]
    fn a_decision_names_its_policy() {
        let d = file(Class::Owner, Filing::Own, &[], None);
        assert_eq!(d.policy, digest(BUILTIN));
    }

    #[test]
    fn classes_come_from_the_author_string() {
        assert_eq!(Class::of("human:test:me", "human:test:me"), Class::Owner);
        assert_eq!(Class::of("human:test:eve", "human:test:me"), Class::Human);
        assert_eq!(Class::of("agent:codex", "human:test:me"), Class::Agent);
        assert_eq!(Class::of("tap:github", "human:test:me"), Class::Tap);
        assert_eq!(Class::of("curation", "human:test:me"), Class::System);
    }
}
