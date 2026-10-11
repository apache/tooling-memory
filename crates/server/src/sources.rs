//! Sources from outside: what a tool tap, fetch or search, brings in when an agent
//! calls it mid-turn.
//!
//! A tool is a WASI component the binary hosts; this crate only asks it, through
//! [`Tools`], so the server carries no runtime. Everything around the call is here:
//! the grant, the policy's `fetch` decision on every request the tool makes (the host
//! asks [`Memory::check_fetch`]), the scan of what comes back, and keeping it. What a
//! tool returns is data, never instructions: it is scanned as a write is, and from
//! anyone but the owner an answer that carries instructions, a broadcast, tool syntax,
//! hidden text, a payload or a secret is held for the owner, never served. What is
//! served is kept under `record/<domain>/.sources/` at the tool's results label, never
//! indexed, cited by entries as `source:<id>`, and swept on the domain's transcript
//! clock unless a live entry cites it. The same call made again while its source is
//! live is answered from it, so a page fetched once serves every later session.

use crate::{lock, Error, Memory};
use dm_core::Label;
use dm_policy::scan;
use dm_store::Area;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use time::{Duration, OffsetDateTime};

/// How an entry cites a source: `source:<id>`.
pub const CITATION: &str = "source:";
/// Who observed a source: the launcher's tap host, which made every request itself.
pub const OBSERVED_BY_TAP: &str = "tap";

/// What a tool's answer future is.
pub type Answer<'a> = Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>>;

/// Runs a tool tap. The binary implements it over the WASI host; a test, over a fake.
pub trait Tools: Send + Sync {
    /// Call `tool` with the owner's `config` and the agent's `args`, both JSON, for the
    /// session `memory` is; the host asks [`Memory::check_fetch`] before each request.
    fn call(
        &self,
        memory: Arc<Memory>,
        tool: String,
        config: String,
        args: String,
    ) -> Answer<'static>;
}

/// One answer from outside, as the record keeps it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Source {
    /// `src-` and the start of the call's digest: the same call is the same source.
    pub id: String,
    /// The tool, by its name in the grants.
    pub tool: String,
    /// The agent's arguments.
    pub args: Value,
    /// Where it is kept: the tool's domain.
    pub domain: String,
    /// The tool's results label.
    pub classification: Label,
    /// The session that called.
    pub session: String,
    /// Its author.
    pub author: String,
    /// When it was fetched.
    #[serde(with = "time::serde::rfc3339")]
    pub kept_at: OffsetDateTime,
    /// When it runs out, unless a live entry cites it.
    #[serde(with = "time::serde::rfc3339")]
    pub retain_until: OffsetDateTime,
    /// Who observed it: the tap host.
    pub observed_by: String,
    /// What the scan found in it, by kind.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<String>,
    /// Held for the owner: never served to anyone else.
    #[serde(default)]
    pub held: bool,
    /// The owner's release of a held answer: served from then on, findings and all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub released: Option<Released>,
    /// The tool's answer.
    pub answer: Value,
}

/// Who released a held answer, and when.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Released {
    /// The owner.
    pub by: String,
    /// When.
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
}

/// Where releases are a line each, under the root.
pub const LOG: &str = "state/sources.jsonl";

fn file(id: &str) -> String {
    format!("{id}.json")
}

/// The source id a call has: the same tool and arguments, the same source.
#[must_use]
pub fn source_id(tool: &str, args: &Value) -> String {
    let d = dm_core::digest(&format!("{tool}\n{args}"));
    format!("src-{}", &d[..16])
}

/// Every string in a JSON value, joined: what the scan reads of an answer.
fn texts(v: &Value, out: &mut String) {
    match v {
        Value::String(s) => {
            out.push_str(s);
            out.push('\n');
        }
        Value::Array(a) => a.iter().for_each(|x| texts(x, out)),
        Value::Object(o) => o.values().for_each(|x| texts(x, out)),
        _ => {}
    }
}

/// An answer as the agent gets it: the tool's own fields, and how to cite it.
fn served(s: &Source, cached: bool) -> Value {
    let mut v = s.answer.clone();
    if !v.is_object() {
        v = json!({ "answer": v });
    }
    v["source"] = json!(s.id);
    v["cite_as"] = json!(format!("{CITATION}{}", s.id));
    v["classification"] = json!(s.classification.to_string());
    v["observed_by"] = json!(s.observed_by);
    v["cached"] = json!(cached);
    if s.released.is_some() {
        // the agent is told what the scan found, and that a person let it through
        v["released_by_owner"] = json!(true);
        v["findings"] = json!(s.findings);
    }
    v
}

impl Memory {
    /// Give this session its tools: the binary's WASI host.
    pub fn set_tools(&self, tools: Arc<dyn Tools>) {
        let _ = self.tools.set(tools);
    }

    /// Whether a tool's request may leave, and log the decision either way: every
    /// outbound request a tool makes is a line in the decision log, with where it went.
    /// The session's label goes in, since what a URL or a query carries leaves with it.
    ///
    /// # Errors
    ///
    /// When the policy fails to evaluate.
    pub fn check_fetch(
        &self,
        mut fetch: dm_policy::decide::FetchInput,
    ) -> Result<dm_policy::decide::Decision, Error> {
        fetch.label = self.label();
        self.decide(&dm_policy::decide::Input {
            action: dm_policy::decide::Action::Fetch,
            principal: self.principal(),
            domain: String::new(),
            filing: None,
            task: None,
            parent: None,
            write: None,
            promote: None,
            held_task: None,
            spend: None,
            fetch: Some(fetch),
        })
    }

    /// Call a tool for this session: from its source when the same call is live, else
    /// through the tool, with every request decided by the policy; the answer is
    /// scanned, kept as a source, and served with how to cite it. An answer the scan
    /// finds instructions in is held for the owner and not served.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] when the tool is not granted, the server runs no tools, the
    /// session may not read the tool's results label, or the answer is held;
    /// [`Error::Tool`] when the tool fails or a request it needed was refused.
    pub async fn use_tool(self: &Arc<Self>, tool: &str, args: Value) -> Result<Value, Error> {
        let (grant, label) = self.tool_grant(tool)?;
        let id = source_id(tool, &args);
        let now = OffsetDateTime::now_utc();
        if let Some(s) = self.source(&grant.domain, &id).await? {
            if !s.held
                && !grant.fresh
                && s.retain_until > now
                && lock(&self.session).observe(s.classification)
            {
                return Ok(served(&s, true));
            }
        }
        let runner = self
            .tools
            .get()
            .cloned()
            .ok_or_else(|| Error::Denied("this session runs no tools".into()))?;
        let answer = runner
            .call(
                self.clone(),
                tool.to_string(),
                grant.config.clone().unwrap_or_else(|| "{}".into()),
                args.to_string(),
            )
            .await
            .map_err(Error::Tool)?;
        let answer: Value = serde_json::from_str(&answer)
            .map_err(|e| Error::Tool(format!("{tool} answered with no JSON: {e}")))?;
        let mut text = String::new();
        texts(&answer, &mut text);
        let findings = scan::kinds(&scan::scan(&text));
        let held = !findings.is_empty() && self.principal().id != self.grants.owner;
        let days = self
            .grants
            .domains
            .get(&grant.domain)
            .map_or(30, dm_policy::DomainGrant::transcript_retention_days);
        let s = Source {
            id: id.clone(),
            tool: tool.to_string(),
            args,
            domain: grant.domain.clone(),
            classification: label,
            session: self.session_id.clone(),
            author: self.author.clone(),
            kept_at: now,
            retain_until: now + Duration::days(i64::from(days)),
            observed_by: OBSERVED_BY_TAP.to_string(),
            findings: findings.clone(),
            held,
            released: None,
            answer,
        };
        self.put_source(&s).await?;
        if held {
            return Err(Error::Denied(format!(
                "held: what {tool} returned carries {}, which is not knowledge; it is kept as {id} for the owner and not served",
                findings.join(", ")
            )));
        }
        if !lock(&self.session).observe(label) {
            return Err(Error::Denied(format!(
                "{tool}'s results are {label}, above this session's ceiling"
            )));
        }
        Ok(served(&s, false))
    }

    /// A tool's grant and its results label, when it is granted as a tool and this
    /// root handles what it returns.
    fn tool_grant(&self, tool: &str) -> Result<(&dm_policy::TapGrant, Label), Error> {
        let grant = self
            .grants
            .taps
            .get(tool)
            .filter(|g| g.tool)
            .ok_or_else(|| {
                Error::Denied(format!(
                    "no tool {tool} is granted; the owner adds [taps.{tool}] with tool = true"
                ))
            })?;
        let label = grant.results.unwrap_or(Label::Public);
        if label > self.grants.handling {
            return Err(Error::Denied(format!(
                "{tool}'s results are {label}, above what this root handles"
            )));
        }
        Ok((grant, label))
    }

    async fn put_source(&self, s: &Source) -> Result<(), Error> {
        let bytes = serde_json::to_vec_pretty(s).map_err(|e| Error::Tool(e.to_string()))?;
        self.store
            .put_object(&s.domain, Area::Sources, &[&file(&s.id)], bytes)
            .await?;
        Ok(())
    }

    async fn source(&self, domain: &str, id: &str) -> Result<Option<Source>, Error> {
        let Some(text) = self
            .store
            .get_object(domain, Area::Sources, &[&file(id)])
            .await?
        else {
            return Ok(None);
        };
        Ok(serde_json::from_str(&text).ok())
    }

    /// Every source the record keeps, the owner's view, newest first.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for anyone but the owner, or a store error.
    pub async fn sources(&self) -> Result<Vec<Source>, Error> {
        if self.author != self.grants.owner {
            return Err(Error::Denied("sources are listed by the owner".into()));
        }
        let mut out = Vec::new();
        for domain in self.domains() {
            for name in self
                .store
                .objects(&domain, Area::Sources, &[], false)
                .await?
            {
                if let Some(id) = name.strip_suffix(".json") {
                    if let Some(s) = self.source(&domain, id).await? {
                        out.push(s);
                    }
                }
            }
        }
        out.sort_by_key(|s| std::cmp::Reverse(s.kept_at));
        Ok(out)
    }

    /// One source, for the owner, held or not.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for anyone but the owner, [`Error::NotFound`], or a store
    /// error.
    pub async fn source_for_owner(&self, id: &str) -> Result<Source, Error> {
        self.sources()
            .await?
            .into_iter()
            .find(|s| s.id == id)
            .ok_or_else(|| Error::NotFound(id.to_string()))
    }

    /// Release a held answer: the owner has read it and lets it be served. From then on
    /// the same call is answered from it, with what the scan found and that the owner
    /// released it, until its clock runs out; the release is a line in
    /// `state/sources.jsonl`. Releasing what is not held changes nothing.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for anyone but the owner, [`Error::NotFound`], or a store
    /// error.
    pub async fn release_source(&self, id: &str) -> Result<Source, Error> {
        let mut s = self.source_for_owner(id).await?;
        if !s.held {
            return Ok(s);
        }
        let now = OffsetDateTime::now_utc();
        s.held = false;
        s.released = Some(Released {
            by: self.author.clone(),
            at: now,
        });
        self.put_source(&s).await?;
        let at = now
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default();
        crate::reads::append_line(
            &self.root.join(LOG),
            &json!({
                "at": at, "action": "release", "source": s.id, "tool": s.tool,
                "by": self.author, "findings": s.findings,
            })
            .to_string(),
        );
        Ok(s)
    }

    /// Sweep the sources whose clock ran out at `now`, unless a live entry cites them:
    /// a page an entry relied on stays as long as the entry. Returns what was dropped.
    ///
    /// # Errors
    ///
    /// A store error.
    pub async fn sweep_sources(&self, now: OffsetDateTime) -> Result<Vec<String>, Error> {
        let mut cited = std::collections::BTreeSet::new();
        for domain in self.domains() {
            for id in self.store.list(&domain).await? {
                let Some(e) = self.store.get(&domain, &id).await? else {
                    continue;
                };
                if !e.is_active() {
                    continue;
                }
                let mut text = e.body.clone();
                for s in &e.front.source {
                    text.push('\n');
                    text.push_str(s);
                }
                for part in text.split(CITATION).skip(1) {
                    let id: String = part
                        .chars()
                        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
                        .collect();
                    cited.insert(id);
                }
            }
        }
        let mut dropped = Vec::new();
        for domain in self.domains() {
            for name in self
                .store
                .objects(&domain, Area::Sources, &[], false)
                .await?
            {
                let Some(id) = name.strip_suffix(".json") else {
                    continue;
                };
                let Some(s) = self.source(&domain, id).await? else {
                    continue;
                };
                if s.retain_until <= now && !cited.contains(&s.id) {
                    self.store
                        .remove_object(&domain, Area::Sources, &[&name])
                        .await?;
                    dropped.push(s.id);
                }
            }
        }
        Ok(dropped)
    }
}
