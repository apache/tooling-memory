//! Tool taps: a component an agent calls mid-turn, fetch or search, through the memory
//! server, with every request it makes decided by the policy.
//!
//! Where a tap proper checks its grant's hosts in the host here, a tool asks the
//! policy's `fetch` action for every request, every redirect hop a request of its own:
//! the scheme, the method, the host against the tool's grant, whether the name
//! resolves inward, and the calling session's label, since a URL or a query is data
//! leaving. Each decision is a line in the decision log. A credential the grant names
//! goes on the request after the decision, in the header the grant names, so the tool
//! never holds it. The request then connects to the addresses the policy was shown, not
//! to whatever the name answers a moment later ([`crate::pinned`]), so a name that
//! answers outward to the check and inward to the connect gets nowhere.
//!
//! Who decides is a [`Gate`]: in the launcher, the calling session's policy
//! ([`PolicyGate`]); in a tool server, which serves the same components to launchers
//! elsewhere and has no session of its own, the hosts the caller passed, inside the
//! server's own grant ([`HostGate`]). Either way every request the gate allows is
//! recorded, so the answer can say where it was fetched from.

use crate::{engine, Error, Limits, EPOCH_TICK};
use dm_policy::decide::FetchInput;
use dm_policy::TapGrant;
pub use dm_server::sources::Answered;
use dm_server::Memory;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::Store;
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};
use wasmtime_wasi_http::{
    RequestOptions, WasiBody, WasiHttpCtx, WasiHttpCtxView, WasiHttpHooks, WasiHttpView,
};

type HttpError = wasmtime_wasi_http::Error;
type SendResult = Result<
    (
        http::Response<WasiBody>,
        Box<dyn Future<Output = Result<(), HttpError>> + Send>,
    ),
    HttpError,
>;

/// Whether an address is this machine's network turned inward: loopback, private,
/// link-local (the cloud metadata address among them), shared, unspecified.
#[must_use]
pub fn is_inward(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            let o = v.octets();
            v.is_private()
                || v.is_loopback()
                || v.is_link_local()
                || v.is_unspecified()
                || v.is_broadcast()
                || (o[0] == 100 && (o[1] & 0xc0) == 64)
        }
        IpAddr::V6(v) => {
            let first = v.segments()[0];
            v.is_loopback()
                || v.is_unspecified()
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80
                || v.to_ipv4_mapped().is_some_and(|m| is_inward(IpAddr::V4(m)))
        }
    }
}

/// What a host resolves to, once: the addresses the request will connect to, whether
/// any is inward, and whether all are loopback. A name that does not resolve has no
/// address, is neither, and its request goes nowhere.
async fn resolves(host: &str, port: u16) -> (Vec<SocketAddr>, bool, bool) {
    let addrs: Vec<SocketAddr> = match host.parse::<IpAddr>() {
        Ok(ip) => vec![SocketAddr::new(ip, port)],
        Err(_) => tokio::net::lookup_host((host, port))
            .await
            .map(Iterator::collect)
            .unwrap_or_default(),
    };
    if addrs.is_empty() {
        return (addrs, false, false);
    }
    let inward = addrs.iter().any(|a| is_inward(a.ip()));
    let loopback = addrs.iter().all(|a| a.ip().is_loopback());
    (addrs, inward, loopback)
}

/// Decides each request a tool makes: `Ok` lets it go, `Err` says why not.
pub trait Gate: Send + Sync {
    /// Decide one request.
    ///
    /// # Errors
    ///
    /// The reason the request may not go.
    fn decide(&self, ask: FetchInput) -> Result<(), String>;
}

/// The calling session's policy, its `fetch` action at the session's label.
pub struct PolicyGate(pub Arc<Memory>);

impl Gate for PolicyGate {
    fn decide(&self, ask: FetchInput) -> Result<(), String> {
        match self.0.check_fetch(ask) {
            Ok(d) if d.allow => Ok(()),
            Ok(d) => Err(d.reason),
            Err(e) => Err(format!("the policy failed: {e}")),
        }
    }
}

/// A tool server's gate: the policy's hard rules (TLS, the granted methods, nothing
/// inward), and a host only when both the server's own grant and the caller name it,
/// so a caller can narrow what the server may reach and never widen it.
#[derive(Clone, Debug, Default)]
pub struct HostGate {
    /// The hosts the server's grant names, the trusted ones included.
    pub granted: Vec<String>,
    /// The hosts the caller's session may reach at its label.
    pub caller: Vec<String>,
    /// The methods the grant names; `GET` when empty.
    pub methods: Vec<String>,
    /// Whether the grant lets the tool reach this machine's loopback, for development.
    pub loopback: bool,
}

/// Whether `host` is one a pattern names: exactly, or under it for `*.example.org`.
#[must_use]
pub fn host_matches(pattern: &str, host: &str) -> bool {
    let p = pattern.to_ascii_lowercase();
    let host = host.to_ascii_lowercase();
    match p.strip_prefix('*') {
        Some(suffix) if suffix.starts_with('.') => host.ends_with(suffix),
        _ => p == host,
    }
}

impl Gate for HostGate {
    fn decide(&self, ask: FetchInput) -> Result<(), String> {
        let loopback_ok = self.loopback && ask.loopback_address;
        if ask.scheme != "https" && !loopback_ok {
            return Err(format!(
                "{} to {} would leave in the clear; https only",
                ask.scheme, ask.host
            ));
        }
        if ask.private_address && !loopback_ok {
            return Err(format!(
                "{} resolves to a private or loopback address; a tool does not reach inward",
                ask.host
            ));
        }
        let method = ask.method.to_ascii_uppercase();
        let allowed = if self.methods.is_empty() {
            method == "GET"
        } else {
            self.methods.iter().any(|m| m.eq_ignore_ascii_case(&method))
        };
        if !allowed {
            return Err(format!("{method} is not a method the tool is granted"));
        }
        let named = |hosts: &[String]| hosts.iter().any(|p| host_matches(p, &ask.host));
        if !named(&self.granted) {
            return Err(format!(
                "{} is not a host this tool server is granted",
                ask.host
            ));
        }
        if !named(&self.caller) {
            return Err(format!(
                "{} is not a host the calling session may reach",
                ask.host
            ));
        }
        Ok(())
    }
}

/// Ask `gate` about one request a tool wants to make: the host resolved once, the
/// policy's input made of the request, and, when it may go, the addresses it connects
/// to and the URL it is recorded as reaching. A refusal says which host and why.
///
/// # Errors
///
/// The gate's reason.
pub(crate) async fn admit(
    gate: &dyn Gate,
    tool: &str,
    method: &str,
    uri: &http::Uri,
    hop: u32,
) -> Result<(Vec<SocketAddr>, String), String> {
    let scheme = uri.scheme_str().unwrap_or("https").to_ascii_lowercase();
    let host = uri
        .host()
        .unwrap_or_default()
        .trim_matches(['[', ']'])
        .to_ascii_lowercase();
    let port = uri.port_u16();
    let default_port = if scheme == "http" { 80 } else { 443 };
    let (addrs, private_address, loopback_address) =
        resolves(&host, port.unwrap_or(default_port)).await;
    let path = uri
        .path_and_query()
        .map_or_else(|| "/".to_string(), |p| p.as_str().to_string());
    let ask = FetchInput {
        tool: tool.to_string(),
        method: method.to_string(),
        scheme: scheme.clone(),
        host: host.clone(),
        port,
        path: path.clone(),
        private_address,
        loopback_address,
        label: dm_core::Label::Public,
        hop,
    };
    gate.decide(ask).map_err(|why| format!("{host}: {why}"))?;
    let port = port.map(|p| format!(":{p}")).unwrap_or_default();
    Ok((addrs, format!("{scheme}://{host}{port}{path}")))
}

/// The outbound hook for a tool: the gate decides, then the credential goes on.
struct Hooks {
    tool: String,
    gate: Arc<dyn Gate>,
    credential: Option<(http::HeaderName, http::HeaderValue)>,
    refused: Arc<Mutex<Vec<String>>>,
    reached: Arc<Mutex<Vec<String>>>,
    hop: Arc<AtomicU32>,
}

impl WasiHttpHooks for Hooks {
    fn send_request(
        &mut self,
        mut request: http::Request<WasiBody>,
        options: Option<RequestOptions>,
        _fut: Box<dyn Future<Output = Result<(), HttpError>> + Send>,
    ) -> Box<dyn Future<Output = SendResult> + Send> {
        let gate = self.gate.clone();
        let tool = self.tool.clone();
        let credential = self.credential.clone();
        let refused = self.refused.clone();
        let reached = self.reached.clone();
        let hop = self.hop.fetch_add(1, Ordering::Relaxed);
        Box::new(async move {
            let uri = request.uri().clone();
            let method = request.method().as_str().to_string();
            let addrs = match admit(gate.as_ref(), &tool, &method, &uri, hop).await {
                Ok((addrs, url)) => {
                    reached
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push(url);
                    addrs
                }
                Err(why) => {
                    refused
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push(why);
                    return Err(HttpError::HttpRequestDenied);
                }
            };
            if let Some((name, value)) = credential {
                request.headers_mut().insert(name, value);
            }
            crate::pinned::send(request, options, &addrs).await
        })
    }
}

struct State {
    wasi: WasiCtx,
    http: WasiHttpCtx,
    table: ResourceTable,
    hooks: Hooks,
    limits: wasmtime::StoreLimits,
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

/// The header a grant's credential goes in, and its value: `Authorization: Bearer`
/// unless the grant names another header, which gets the value as it is.
pub(crate) fn credential_header(
    grant: &TapGrant,
    credential: Option<&str>,
) -> Result<Option<(http::HeaderName, http::HeaderValue)>, Error> {
    let Some(c) = credential else {
        return Ok(None);
    };
    let bad = |e: &dyn std::fmt::Display| Error::Runtime(format!("credential: {e}"));
    Ok(Some(match grant.credential_header.as_deref() {
        None => (
            http::header::AUTHORIZATION,
            http::HeaderValue::from_str(&format!("Bearer {c}")).map_err(|e| bad(&e))?,
        ),
        Some(h) => (
            http::HeaderName::from_bytes(h.as_bytes()).map_err(|e| bad(&e))?,
            http::HeaderValue::from_str(c).map_err(|e| bad(&e))?,
        ),
    }))
}

/// One call of a tool: which, under what grant, with what.
#[derive(Clone, Copy, Debug)]
pub struct Call<'a> {
    /// The tool, by its name in the grants.
    pub tool: &'a str,
    /// Its grant.
    pub grant: &'a TapGrant,
    /// The value of the variable the grant names, if it was set.
    pub credential: Option<&'a str>,
    /// The owner's configuration, JSON.
    pub config: &'a str,
    /// The agent's arguments, JSON.
    pub args: &'a str,
}

/// Call a tool once for the session `memory` is: the tool's JSON, and every URL the
/// policy let it reach.
///
/// # Errors
///
/// A component that will not load or link, a trap, the wall clock, a request the
/// policy refused that the tool did not survive, or the tool's own failure.
pub async fn call(
    memory: Arc<Memory>,
    component: &Path,
    c: Call<'_>,
    limits: Limits,
) -> Result<Answered, Error> {
    call_gated(Arc::new(PolicyGate(memory)), component, c, limits).await
}

/// Call a tool once with `gate` deciding each request it makes.
///
/// # Errors
///
/// As [`call`].
pub async fn call_gated(
    gate: Arc<dyn Gate>,
    component: &Path,
    c: Call<'_>,
    limits: Limits,
) -> Result<Answered, Error> {
    let engine = engine()?;
    let component = Component::from_file(&engine, component)
        .map_err(|e| Error::Runtime(format!("{}: {e:#}", component.display())))?;
    let mut linker: Linker<State> = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
    wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker)?;
    let refused = Arc::new(Mutex::new(Vec::new()));
    let reached = Arc::new(Mutex::new(Vec::new()));
    let state = State {
        wasi: WasiCtxBuilder::new().build(),
        http: WasiHttpCtx::new(),
        table: ResourceTable::new(),
        hooks: Hooks {
            tool: c.tool.to_string(),
            gate,
            credential: credential_header(c.grant, c.credential)?,
            refused: refused.clone(),
            reached: reached.clone(),
            hop: Arc::new(AtomicU32::new(0)),
        },
        limits: wasmtime::StoreLimitsBuilder::new()
            .memory_size(limits.memory)
            .build(),
    };
    let mut store = Store::new(&engine, state);
    store.limiter(|s| &mut s.limits);
    store.set_epoch_deadline(1);
    store.epoch_deadline_async_yield_and_update(1);
    let ticker = {
        let engine = engine.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(EPOCH_TICK).await;
                engine.increment_epoch();
            }
        })
    };
    let outcome = tokio::time::timeout(limits.wall, async {
        let t =
            crate::bindings::tool::Tool::instantiate_async(&mut store, &component, &linker).await?;
        t.call_call(&mut store, c.config, c.args).await
    })
    .await;
    ticker.abort();
    let result = match outcome {
        Ok(r) => r?,
        Err(_) => {
            return Err(Error::Runtime(format!(
                "the call exceeded its wall clock of {:?}",
                limits.wall
            )))
        }
    };
    let refused = std::mem::take(&mut *refused.lock().unwrap_or_else(PoisonError::into_inner));
    let reached = std::mem::take(&mut *reached.lock().unwrap_or_else(PoisonError::into_inner));
    match result {
        Ok(answer) => Ok(Answered { answer, reached }),
        Err(e) if refused.is_empty() => Err(Error::Tap(e)),
        Err(e) => Err(Error::Refused(refused.join("; "), e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(scheme: &str, host: &str, method: &str) -> FetchInput {
        FetchInput {
            tool: "fetch".into(),
            method: method.into(),
            scheme: scheme.into(),
            host: host.into(),
            port: None,
            path: "/".into(),
            private_address: host == "127.0.0.1" || host == "10.0.0.1",
            loopback_address: host == "127.0.0.1",
            label: dm_core::Label::Public,
            hop: 0,
        }
    }

    #[test]
    fn a_host_gate_lets_through_only_what_the_grant_and_the_caller_both_name() {
        let g = HostGate {
            granted: vec!["*.apache.org".into(), "docs.python.org".into()],
            caller: vec!["*.apache.org".into()],
            methods: Vec::new(),
            loopback: false,
        };
        assert_eq!(g.decide(ask("https", "www.apache.org", "GET")), Ok(()));
        assert!(g
            .decide(ask("https", "docs.python.org", "GET"))
            .unwrap_err()
            .contains("calling session"));
        assert!(g
            .decide(ask("https", "example.com", "GET"))
            .unwrap_err()
            .contains("tool server is granted"));
        assert!(g
            .decide(ask("http", "www.apache.org", "GET"))
            .unwrap_err()
            .contains("https only"));
        assert!(g
            .decide(ask("https", "www.apache.org", "POST"))
            .unwrap_err()
            .contains("method"));
        assert!(g
            .decide(ask("https", "10.0.0.1", "GET"))
            .unwrap_err()
            .contains("inward"));
        // the caller cannot widen what the server is granted
        let wide = HostGate {
            caller: vec!["example.com".into()],
            ..g.clone()
        };
        assert!(wide.decide(ask("https", "example.com", "GET")).is_err());
        // loopback only when the grant says so, and then in the clear too
        let dev = HostGate {
            granted: vec!["127.0.0.1".into()],
            caller: vec!["127.0.0.1".into()],
            loopback: true,
            ..HostGate::default()
        };
        assert_eq!(dev.decide(ask("http", "127.0.0.1", "GET")), Ok(()));
        assert!(host_matches("*.apache.org", "WWW.Apache.org"));
        assert!(!host_matches("*.apache.org", "apache.org.evil.com"));
        assert!(!host_matches("*apache.org", "evilapache.org"));
    }

    #[test]
    fn inward_addresses_are_the_machine_s_own_networks() {
        for a in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:10.0.0.1",
        ] {
            assert!(is_inward(a.parse().unwrap()), "{a}");
        }
        for a in ["151.101.1.69", "2a04:4e42::223", "100.128.0.1"] {
            assert!(!is_inward(a.parse().unwrap()), "{a}");
        }
    }
}
