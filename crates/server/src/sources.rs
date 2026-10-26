//! Sources from outside: what a tool tap, fetch or search, brings in when an agent
//! calls it mid-turn.
//!
//! A tool is a WASI component the binary hosts; this crate only asks it, through
//! [`Tools`], so the server carries no runtime. Everything around the call is here:
//! the grant, the policy's `fetch` decision on every request the tool makes (the host
//! asks [`Memory::check_fetch`]), the scan of what comes back, and keeping it. What a
//! tool returns is data, never instructions: it is scanned as a write is, and from
//! anyone but the owner an answer that carries instructions, a broadcast, tool syntax,
//! hidden text, a payload or a secret is held for the owner, never served. What a tool
//! says when it fails comes from outside too, an upstream's error body or a header it
//! echoed, and is scanned the same way: one that carries any of those is withheld from
//! the agent and kept as a line in [`LOG`] for the owner. What is
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
/// A source a tool server fetched for this session: it made the requests, held to the
/// hosts the session may reach, and reported where it went; the memory server checked
/// each of those against the policy before keeping what came back.
pub const OBSERVED_BY_TOOL_SERVER: &str = "tool-server";
/// A source the owner's browser fetched for this session, through the web launcher's
/// companion extension, with the browser's own cookies: the policy here decided the URL
/// before the browser was asked and the URL that answered before the answer was kept.
pub const OBSERVED_BY_BROWSER: &str = "browser";

/// What a tool answered, and every URL it was let reach to answer it, in order: the
/// requests the tap host allowed, or the ones a tool server reported and the policy
/// here accepted.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Answered {
    /// The tool's JSON.
    pub answer: String,
    /// `scheme://host[:port]/path?query` of each request, in order.
    pub reached: Vec<String>,
}

/// What a tool's answer future is.
pub type Answer<'a> = Pin<Box<dyn Future<Output = Result<Answered, String>> + Send + 'a>>;

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
    /// Who observed it: the tap host, or a tool server.
    pub observed_by: String,
    /// Where it was fetched from: every URL reached to answer the call, in order, as
    /// the tap host allowed them or a tool server reported them and the policy here
    /// accepted them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reached: Vec<String>,
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

/// Where releases, and tool failures withheld from the agent, are a line each, under
/// the root.
pub const LOG: &str = "state/sources.jsonl";

/// How much of a withheld failure is kept: its start, which is where an upstream says
/// what went wrong, so a caller repeating a failing call cannot grow the log without
/// bound.
const WITHHELD_BYTES: usize = 16 * 1024;

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
    v["reached"] = json!(s.reached);
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
    /// [`Error::Tool`] when the tool fails or a request it needed was refused, with
    /// what it said withheld when the scan finds anything in that.
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
        let Answered { answer, reached } = runner
            .call(
                self.clone(),
                tool.to_string(),
                grant.config.clone().unwrap_or_else(|| "{}".into()),
                args.to_string(),
            )
            .await
            .map_err(|said| self.tool_failed(tool, &id, label, said))?;
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
            observed_by: if grant.browser {
                OBSERVED_BY_BROWSER
            } else if grant.hosted.is_some() {
                OBSERVED_BY_TOOL_SERVER
            } else {
                OBSERVED_BY_TAP
            }
            .to_string(),
            reached,
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

    /// A tool's failure as the agent is told it. What an upstream says when it fails is
    /// its data as much as an answer is: the session is raised to the tool's results
    /// label to read it, or told nothing of it above its ceiling. It is told as it came,
    /// unless the scan finds something in it and the session is not the owner's, when
    /// the agent is told only what kinds were found and the start of the text is a line
    /// in [`LOG`].
    fn tool_failed(&self, tool: &str, id: &str, label: Label, said: String) -> Error {
        if !lock(&self.session).observe(label) {
            return Error::Denied(format!(
                "{tool} failed, and what it said is {label}, above this session's ceiling"
            ));
        }
        let findings = scan::kinds(&scan::scan(&said));
        if findings.is_empty() || self.principal().id == self.grants.owner {
            return Error::Tool(said);
        }
        let at = OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default();
        let mut end = said.len().min(WITHHELD_BYTES);
        while !said.is_char_boundary(end) {
            end -= 1;
        }
        let kept = crate::reads::try_append_line(
            &self.root.join(LOG),
            &json!({
                "at": at, "action": "withhold", "source": id, "tool": tool,
                "session": self.session_id, "by": self.author, "findings": findings,
                "error": &said[..end], "length": said.len(),
            })
            .to_string(),
        )
        .is_ok();
        let kept = if kept {
            format!("it is kept in {LOG} for the owner")
        } else {
            format!("writing it to {LOG} failed, so it is lost")
        };
        Error::Tool(format!(
            "{tool} failed, and what it said carries {}, which is not knowledge; {kept} and not shown",
            findings.join(", ")
        ))
    }

    /// The hosts this session may reach through `tool` at its label now: the grant's
    /// own up to its ceiling, and the trusted ones up to theirs. What a hosted tool is
    /// told it may reach; an exception that lifts reach for one task is not in it, so
    /// the list errs narrow, and every URL reached is decided by the policy again.
    #[must_use]
    pub fn fetch_hosts(&self, tool: &str) -> Vec<String> {
        let Some(grant) = self.grants.taps.get(tool) else {
            return Vec::new();
        };
        let label = self.label();
        let mut hosts = Vec::new();
        if label <= grant.ceiling {
            hosts.extend(grant.hosts.iter().cloned());
        }
        if let Some(t) = grant.trusted.as_ref().filter(|t| label <= t.ceiling) {
            hosts.extend(t.hosts.iter().cloned());
        }
        hosts
    }

    /// Decide, by the policy at this session's label, a URL a hosted tool reports it
    /// reached, as the tap host decides a request before it leaves.
    ///
    /// # Errors
    ///
    /// Why the policy would not have let it go, or a URL that cannot be read.
    pub fn check_reached(&self, tool: &str, url: &str) -> Result<(), String> {
        let (scheme, rest) = url
            .split_once("://")
            .ok_or_else(|| format!("{url:?} is not a URL"))?;
        let (authority, path) = rest.find('/').map_or((rest, "/"), |i| rest.split_at(i));
        // `[v6]:port`, `[v6]`, `host:port` or `host`
        let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
            let (h, after) = v6.split_once(']').unwrap_or((v6, ""));
            (
                h,
                after.strip_prefix(':').and_then(|p| p.parse::<u16>().ok()),
            )
        } else {
            match authority.rsplit_once(':') {
                Some((h, p)) => (h, p.parse::<u16>().ok()),
                None => (authority, None),
            }
        };
        let host = host.to_ascii_lowercase();
        let ip = host.parse::<std::net::IpAddr>().ok();
        let loopback = host == "localhost" || ip.is_some_and(|ip| ip.is_loopback());
        let private = loopback
            || ip.is_some_and(|ip| match ip {
                std::net::IpAddr::V4(v) => {
                    v.is_private() || v.is_link_local() || v.is_unspecified()
                }
                std::net::IpAddr::V6(v) => {
                    v.is_unspecified() || (v.segments()[0] & 0xfe00) == 0xfc00
                }
            });
        let decision = self
            .check_fetch(dm_policy::decide::FetchInput {
                tool: tool.to_string(),
                method: "GET".into(),
                scheme: scheme.to_ascii_lowercase(),
                host,
                port,
                path: path.to_string(),
                private_address: private,
                loopback_address: loopback,
                label: Label::Public,
                hop: 0,
            })
            .map_err(|e| format!("the policy failed: {e}"))?;
        if decision.allow {
            Ok(())
        } else {
            Err(decision.reason)
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// A tool that fails, passing on what its upstream said.
    struct Failing(String);

    impl Tools for Failing {
        fn call(
            &self,
            _memory: Arc<Memory>,
            _tool: String,
            _config: String,
            _args: String,
        ) -> Answer<'static> {
            let said = self.0.clone();
            Box::pin(async move { Err(said) })
        }
    }

    const OWNER: &str = "human:test:me";
    const ECHOED: &str =
        "upstream 401 for Authorization: token ghp_abcdefghijklmnopqrstuvwxyz0123456789";

    fn root(dir: &Path, said: &str, author: &str) -> Arc<Memory> {
        root_with(dir, said, author, None)
    }

    fn root_with(dir: &Path, said: &str, author: &str, results: Option<Label>) -> Arc<Memory> {
        Memory::init(dir, OWNER, "dom-me").unwrap();
        let path = dir.join(crate::GRANTS_FILE);
        let mut g = dm_policy::Grants::load(&path).unwrap();
        g.taps.insert(
            "lookup".into(),
            dm_policy::TapGrant {
                domain: "dom-me".into(),
                tool: true,
                ceiling: Label::Public,
                results,
                ..dm_policy::TapGrant::default()
            },
        );
        std::fs::write(&path, g.render().unwrap()).unwrap();
        let m = Arc::new(Memory::open(dir).unwrap().with_author(author));
        m.set_tools(Arc::new(Failing(said.to_string())));
        m
    }

    #[tokio::test]
    async fn a_failure_that_carries_a_secret_is_withheld_and_kept_for_the_owner() {
        let dir = tempfile::tempdir().unwrap();
        let agent = root(dir.path(), ECHOED, "agent:test");
        let err = agent
            .use_tool("lookup", json!({"q": "x"}))
            .await
            .unwrap_err();
        let Error::Tool(why) = &err else {
            panic!("{err}")
        };
        assert!(!why.contains("ghp_"), "{why}");
        assert!(
            why.contains("lookup failed") && why.contains("secret"),
            "{why}"
        );
        let log = std::fs::read_to_string(dir.path().join(LOG)).unwrap();
        assert!(log.contains("\"action\":\"withhold\""), "{log}");
        assert!(log.contains(ECHOED), "{log}");
    }

    #[tokio::test]
    async fn a_plain_failure_and_the_owners_own_are_told_as_they_came() {
        let dir = tempfile::tempdir().unwrap();
        let agent = root(dir.path(), "upstream 503: try again later", "agent:test");
        let err = agent
            .use_tool("lookup", json!({"q": "x"}))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, Error::Tool(m) if m == "upstream 503: try again later"),
            "{err}"
        );
        assert!(
            !dir.path().join(LOG).exists(),
            "a plain failure is not logged"
        );
        let dir = tempfile::tempdir().unwrap();
        let owner = root(dir.path(), ECHOED, OWNER);
        let err = owner
            .use_tool("lookup", json!({"q": "x"}))
            .await
            .unwrap_err();
        assert!(matches!(&err, Error::Tool(m) if m == ECHOED), "{err}");
        assert!(!dir.path().join(LOG).exists());
    }

    #[tokio::test]
    async fn a_failure_raises_the_session_to_the_tools_results_label() {
        // what an upstream says when it fails is as much its data as an answer is
        let dir = tempfile::tempdir().unwrap();
        let agent = root_with(
            dir.path(),
            "upstream 500: customer 4411 failed validation",
            "agent:test",
            Some(Label::Internal),
        );
        assert_eq!(agent.label(), Label::Public);
        let err = agent
            .use_tool("lookup", json!({"q": "x"}))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, Error::Tool(m) if m.contains("4411")),
            "{err}"
        );
        assert_eq!(agent.label(), Label::Internal);
    }

    #[tokio::test]
    async fn a_withheld_failure_is_kept_cut_to_a_size() {
        let dir = tempfile::tempdir().unwrap();
        let said = format!("{ECHOED} {}", "x".repeat(200 * 1024));
        let agent = root(dir.path(), &said, "agent:test");
        assert!(agent.use_tool("lookup", json!({"q": "x"})).await.is_err());
        let log = std::fs::read_to_string(dir.path().join(LOG)).unwrap();
        assert!(log.contains("ghp_"), "the start is kept");
        assert!(log.len() < 32 * 1024, "{} bytes", log.len());
        assert!(
            log.contains(&format!("\"length\":{}", said.len())),
            "the length is said"
        );
    }

    #[tokio::test]
    async fn a_failure_that_could_not_be_kept_does_not_say_it_was() {
        let dir = tempfile::tempdir().unwrap();
        let agent = root(dir.path(), ECHOED, "agent:test");
        // the log's place is a directory, so nothing can be appended there
        std::fs::create_dir_all(dir.path().join(LOG)).unwrap();
        let err = agent
            .use_tool("lookup", json!({"q": "x"}))
            .await
            .unwrap_err();
        let Error::Tool(why) = &err else {
            panic!("{err}")
        };
        assert!(!why.contains("ghp_"), "{why}");
        assert!(!why.contains("kept in"), "{why}");
    }
}
