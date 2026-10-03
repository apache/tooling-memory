//! Taps as WASI components: a connector runs inside the wrapper with exactly the access
//! its grant allows, enforced by the runtime rather than by trust.
//!
//! The grants file names each tap's domain, the hosts it may reach, its ceiling, and the
//! environment variable that holds the tracker's credential. The host here builds a
//! component store with no filesystem, no sockets and no environment, links the tap
//! world of `wit/tap.wit` to the memory service, and routes the tap's outbound HTTP
//! through one hook: a host outside the grant is refused, a label above the ceiling is
//! refused, and the credential goes on as a bearer token so the tap never holds it.
//! Everything the tap writes is a proposal in the owner's domain, under the tap's own
//! author: a task born proposed, or an entry at raw tier.

use dm_core::{Confidence, EntryType};
use dm_policy::TapGrant;
use dm_server::{Memory, WriteRequest};
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use wasmtime::component::{Component, HasSelf, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};
use wasmtime_wasi_http::{
    RequestOptions, WasiBody, WasiHttpCtx, WasiHttpCtxView, WasiHttpHooks, WasiHttpView,
};

#[allow(missing_docs)] // generated from wit/tap.wit, which carries the documentation
mod bindings {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "tap",
        imports: { default: async | trappable },
        exports: { default: async },
    });
}

use bindings::durable_memory::tap::memory::{Filed, Host, Proposal, Task};
use bindings::Tap;

/// What can go wrong running a tap.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The component will not load, link or run.
    #[error("tap: {0}")]
    Runtime(String),
    /// The tap itself reported failure.
    #[error("tap reported: {0}")]
    Tap(String),
    /// The tap gave up after the host refused what its grant does not allow.
    #[error("refused: {0}; the tap then reported: {1}")]
    Refused(String, String),
    /// The memory service.
    #[error(transparent)]
    Memory(#[from] dm_server::Error),
}

impl From<wasmtime::Error> for Error {
    fn from(e: wasmtime::Error) -> Self {
        Error::Runtime(format!("{e:#}"))
    }
}

/// The outbound hook: the grant's hosts, the ceiling, and the credential. What it
/// refused is kept, so a run the tap gives up on can say why.
struct Hooks {
    hosts: Vec<String>,
    credential: Option<http::HeaderValue>,
    memory: Arc<Memory>,
    ceiling: dm_core::Label,
    refused: Vec<String>,
}

/// Whether an authority (`host` or `host:port`) is one the grant names.
#[must_use]
pub fn host_allowed(hosts: &[String], authority: &str) -> bool {
    let a = authority.to_ascii_lowercase();
    hosts.iter().any(|h| {
        let h = h.trim().to_ascii_lowercase();
        !h.is_empty() && (a == h || a.rsplit_once(':').is_some_and(|(host, _)| host == h))
    })
}

type HttpError = wasmtime_wasi_http::Error;
type SendResult = Result<
    (
        http::Response<WasiBody>,
        Box<dyn Future<Output = Result<(), HttpError>> + Send>,
    ),
    HttpError,
>;

impl WasiHttpHooks for Hooks {
    fn send_request(
        &mut self,
        mut request: http::Request<WasiBody>,
        options: Option<RequestOptions>,
        _fut: Box<dyn Future<Output = Result<(), HttpError>> + Send>,
    ) -> Box<dyn Future<Output = SendResult> + Send> {
        let authority = request
            .uri()
            .authority()
            .map(|a| a.as_str().to_string())
            .unwrap_or_default();
        if !host_allowed(&self.hosts, &authority) {
            self.refused
                .push(format!("the grant does not allow {authority}"));
            return Box::new(async { Err(HttpError::HttpRequestDenied) });
        }
        let label = self.memory.label();
        if label > self.ceiling {
            self.refused.push(format!(
                "the tap's label {label} is above its ceiling {}",
                self.ceiling
            ));
            return Box::new(async { Err(HttpError::HttpRequestDenied) });
        }
        if let Some(c) = &self.credential {
            request
                .headers_mut()
                .insert(http::header::AUTHORIZATION, c.clone());
        }
        Box::new(async move {
            use http_body_util::BodyExt;
            let (res, io) = wasmtime_wasi_http::default_send_request(request, options).await?;
            Ok((
                res.map(BodyExt::boxed_unsync),
                Box::new(io) as Box<dyn Future<Output = _> + Send>,
            ))
        })
    }
}

/// The store's state: WASI with nothing granted, HTTP through the hook, and the memory.
struct State {
    wasi: WasiCtx,
    http: WasiHttpCtx,
    table: ResourceTable,
    hooks: Hooks,
    memory: Arc<Memory>,
    domain: String,
}

impl WasiView for State {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl WasiHttpView for State {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            hooks: &mut self.hooks,
            table: &mut self.table,
            ctx: &mut self.http,
        }
    }
}

impl Host for State {
    async fn file_task(&mut self, t: Task) -> wasmtime::Result<Result<Filed, String>> {
        let spec = dm_ledger::TaskSpec {
            domain: self.domain.clone(),
            title: t.title,
            body: t.body,
            requirements: t.requirements,
            depends_on: Vec::new(),
            origin: Some(t.origin),
            upstream_event: Some(t.upstream_event),
            created_by: String::new(),
            classification: dm_core::Label::Public,
            priority: t.priority,
        };
        Ok(match self.memory.create_task(spec).await {
            Ok(c) => Ok(Filed {
                id: c.task.id,
                deduplicated: c.deduplicated,
            }),
            Err(e) => Err(e.to_string()),
        })
    }

    async fn propose(&mut self, p: Proposal) -> wasmtime::Result<Result<String, String>> {
        let entry_type = match p.kind.parse::<EntryType>() {
            Ok(k) => k,
            Err(e) => return Ok(Err(format!("kind: {e}"))),
        };
        let req = WriteRequest {
            domain: Some(self.domain.clone()),
            entry_type,
            title: Some(p.title),
            body: p.body,
            source: p.source,
            confidence: Some(Confidence::Low),
            ..WriteRequest::default()
        };
        Ok(match self.memory.write(req).await {
            Ok(w) => Ok(w.id),
            Err(e) => Err(e.to_string()),
        })
    }
}

/// What one run did, as the tap reported it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Outcome {
    /// Tasks filed anew.
    pub filed: u32,
    /// Tasks already filed for their origin and event.
    pub deduplicated: u32,
    /// Entries proposed.
    pub proposed: u32,
    /// The tap's one-line note to the owner.
    pub note: String,
}

fn engine() -> wasmtime::Result<Engine> {
    let mut config = Config::new();
    config.wasm_component_model(true);
    Engine::new(&config)
}

/// Run a tap once. `memory` is opened by the caller under the tap's author;
/// `credential` is the value of the variable the grant names, if it was set.
///
/// # Errors
///
/// A component that will not load or link, a trap, a refused outbound call the tap did
/// not survive, or the tap's own reported failure.
pub async fn run(
    memory: Arc<Memory>,
    component: &Path,
    grant: &TapGrant,
    credential: Option<&str>,
    config: &str,
) -> Result<Outcome, Error> {
    let engine = engine()?;
    let component = Component::from_file(&engine, component)
        .map_err(|e| Error::Runtime(format!("{}: {e:#}", component.display())))?;
    let mut linker: Linker<State> = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
    wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker)?;
    Tap::add_to_linker::<_, HasSelf<_>>(&mut linker, |s| s)?;

    let credential = credential
        .map(|c| http::HeaderValue::from_str(&format!("Bearer {c}")))
        .transpose()
        .map_err(|e| Error::Runtime(format!("credential: {e}")))?;
    let state = State {
        wasi: WasiCtxBuilder::new().build(),
        http: WasiHttpCtx::new(),
        table: ResourceTable::new(),
        hooks: Hooks {
            hosts: grant.hosts.clone(),
            credential,
            memory: memory.clone(),
            ceiling: grant.ceiling,
            refused: Vec::new(),
        },
        domain: grant.domain.clone(),
        memory,
    };
    let mut store = Store::new(&engine, state);
    let tap = Tap::instantiate_async(&mut store, &component, &linker).await?;
    let result = tap.call_run(&mut store, config).await?;
    let refused = store.data().hooks.refused.join("; ");
    match result {
        Ok(r) => Ok(Outcome {
            filed: r.filed,
            deduplicated: r.deduplicated,
            proposed: r.proposed,
            note: r.note,
        }),
        Err(e) if refused.is_empty() => Err(Error::Tap(e)),
        Err(e) => Err(Error::Refused(refused, e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_match_with_or_without_a_port_and_never_by_suffix() {
        let hosts = vec!["api.github.com".to_string(), "127.0.0.1".to_string()];
        assert!(host_allowed(&hosts, "api.github.com"));
        assert!(host_allowed(&hosts, "API.GITHUB.COM:443"));
        assert!(host_allowed(&hosts, "127.0.0.1:4321"));
        assert!(!host_allowed(&hosts, "evil-api.github.com"));
        assert!(!host_allowed(&hosts, "api.github.com.evil.example"));
        assert!(!host_allowed(&hosts, "github.com"));
        assert!(!host_allowed(&[], "api.github.com"));
        assert!(!host_allowed(&[String::new()], ""));
    }
}
