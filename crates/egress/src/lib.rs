//! The egress: the one way out of a confined harness.
//!
//! The harness runs with no network. The only thing it can reach is this server, over a
//! unix socket bind-mounted into its sandbox and relayed to a loopback port inside. Three
//! things live behind that socket:
//!
//! - `/mcp`: the memory server, over MCP's streamable HTTP transport;
//! - `/<destination>/...`: a forwarding proxy to a model provider or a gateway, one per
//!   destination the grants file names, with the session label checked against the
//!   destination's ceiling before anything leaves, and token usage read out of every
//!   response on the way back;
//! - nothing else. A path that names no destination is a 404, unless the session's only
//!   destination says its harness addresses calls at the origin, as Muse Code does its
//!   model catalog: then the whole path goes there. At any other single destination,
//!   Muse's catalog is that destination's OpenAI model list, which Muse reads as one.
//!
//! Credentials for a destination live here, injected on the way out, unless the mode is
//! pass-through, in which case the harness holds its own (a subscription login) and the
//! egress forwards what it sends.

use axum::body::{to_bytes, Body};
use axum::extract::{Path, Request, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use bytes::Bytes;
use dm_core::Label;
use dm_server::mcp::McpServer;
use dm_server::Memory;
use futures_util::StreamExt;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::Path as FsPath;
use std::sync::Arc;

pub mod telemetry;
pub mod usage;

pub use telemetry::Telemetry;
pub use usage::{Sniffer, Usage};

/// How the egress authenticates to a destination.
#[derive(Clone, Debug)]
pub enum Auth {
    /// Forward whatever the harness sent. For a subscription login the harness holds.
    PassThrough,
    /// Strip the harness's credential headers and set this one. The harness never sees
    /// the value.
    Inject {
        /// The header to set, such as `authorization` or `x-api-key`.
        header: HeaderName,
        /// Its value.
        value: String,
    },
}

/// One place model calls may go.
#[derive(Clone, Debug)]
pub struct Destination {
    /// The upstream base, such as `https://api.anthropic.com` or `http://127.0.0.1:4000`.
    pub upstream: String,
    /// How to authenticate there.
    pub auth: Auth,
    /// Hosts a `CONNECT` tunnel may be opened to, for a harness with no base-URL
    /// override that is pointed here with `HTTPS_PROXY`. An entry starting with a dot
    /// matches that domain and its subdomains. An entry allows port 443, or the port it
    /// names, `host:port` or `[v6]:port`. A name is resolved when the tunnel opens and
    /// refused when it resolves to loopback, a private or link-local range, or another
    /// address no internet host has; an entry that is an address is dialled as named.
    /// Empty for an ordinary forward.
    /// A tunnel is pinned egress without metering: TLS runs end to end, so the egress
    /// sees no usage and checks the ceiling only when the tunnel opens.
    pub tunnel_hosts: Vec<String>,
    /// Whether the harness addresses some calls at the base URL's origin rather than
    /// under the base's path, as Muse Code does its model catalog. A path that names
    /// no destination then goes here whole, when this is the session's only
    /// destination; otherwise such a path is a 404.
    pub origin_paths: bool,
    /// Whether a Messages request's `system`-role messages are folded into its `system`
    /// field on the way out. Claude Code puts its environment block in `messages` as a
    /// `system` role after the user turn, which Anthropic's API takes and a gateway
    /// translating to another model's chat template refuses (`System message must be at
    /// the beginning`); folding it is what the template wants and loses nothing.
    pub fold_system_messages: bool,
    /// Whether a Gemini API request's `functionResponse` parts are moved out of a
    /// `model` turn into a `user` turn after it, on the way out. Antigravity CLI sends a
    /// tool's result under the model's role, which Google's API takes and LiteLLM's
    /// Gemini adapter drops, so the model answers without the result.
    pub fold_function_responses: bool,
    /// A cap on the output tokens a request asks for, on the way out. Muse Code asks
    /// for 128k whatever the catalog says, which a 131k-context model refuses once the
    /// prompt is counted; a gateway's models are better served by a cap.
    pub max_output_tokens: Option<u64>,
}

impl Destination {
    /// A forwarding destination.
    #[must_use]
    pub fn forward(upstream: impl Into<String>, auth: Auth) -> Destination {
        Destination {
            upstream: upstream.into(),
            auth,
            tunnel_hosts: Vec::new(),
            origin_paths: false,
            fold_system_messages: false,
            fold_function_responses: false,
            max_output_tokens: None,
        }
    }

    /// A tunnel-only destination to the given hosts.
    #[must_use]
    pub fn tunnel(hosts: impl IntoIterator<Item = impl Into<String>>) -> Destination {
        Destination {
            upstream: String::new(),
            auth: Auth::PassThrough,
            tunnel_hosts: hosts.into_iter().map(Into::into).collect(),
            origin_paths: false,
            fold_system_messages: false,
            fold_function_responses: false,
            max_output_tokens: None,
        }
    }

    /// Also allow tunnels to these hosts. For a forwarding destination whose login
    /// refreshes its token at a host the forward does not cover.
    #[must_use]
    pub fn with_tunnels(
        mut self,
        hosts: impl IntoIterator<Item = impl Into<String>>,
    ) -> Destination {
        self.tunnel_hosts.extend(hosts.into_iter().map(Into::into));
        self
    }

    /// The harness addresses some calls at the origin; see [`Destination::origin_paths`].
    #[must_use]
    pub fn with_origin_paths(mut self) -> Destination {
        self.origin_paths = true;
        self
    }

    /// Fold `system`-role messages into `system`; see
    /// [`Destination::fold_system_messages`].
    #[must_use]
    pub fn with_folded_system_messages(mut self) -> Destination {
        self.fold_system_messages = true;
        self
    }

    /// Move `functionResponse` parts into a `user` turn; see
    /// [`Destination::fold_function_responses`].
    #[must_use]
    pub fn with_function_responses_in_user_turns(mut self) -> Destination {
        self.fold_function_responses = true;
        self
    }

    /// Cap the output tokens a request asks for; see [`Destination::max_output_tokens`].
    #[must_use]
    pub fn with_max_output_tokens(mut self, cap: u64) -> Destination {
        self.max_output_tokens = Some(cap);
        self
    }
}

/// What can go wrong building or running the egress.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A destination has no ceiling in the grants file.
    #[error(
        "destination {0} has no ceiling in grants.toml; add [destinations.{0}] ceiling = \"...\""
    )]
    NoCeiling(String),
    /// The HTTP client could not be built.
    #[error("egress client: {0}")]
    Client(#[from] reqwest::Error),
    /// Telemetry.
    #[error(transparent)]
    Telemetry(#[from] telemetry::Error),
    /// Listening or serving.
    #[error("egress: {0}")]
    Io(#[from] std::io::Error),
    /// A direct call to a destination this egress does not have, or may not send to at
    /// the session's label.
    #[error("destination {0} is not available to this session")]
    Refused(String),
    /// The call would go over a spend limit; the reason says which.
    #[error("{0}")]
    OverBudget(String),
    /// A direct call the upstream did not answer with success.
    #[error("{0}")]
    Upstream(String),
    /// A tool server named for a tool the grants do not host.
    #[error("{0} is not a hosted tool in grants.toml; a tool server is reached only for [taps.{0}] hosted = \"...\"")]
    NotHosted(String),
}

struct Inner {
    memory: Arc<Memory>,
    destinations: HashMap<String, Destination>,
    /// Tool servers, by the tool's name in the grants: called by the launcher itself
    /// for a hosted tool ([`Egress::post`]), never forwarded for the harness, which
    /// reaches a tool only through the memory server.
    tools: HashMap<String, Destination>,
    client: reqwest::Client,
    telemetry: Telemetry,
}

/// What a tool server answered one request with.
#[derive(Clone, Debug)]
pub struct Relayed {
    /// The status.
    pub status: u16,
    /// The response headers.
    pub headers: HeaderMap,
    /// The body, as text.
    pub body: String,
}

/// The model a tool server's call is recorded under in the usage log: no tokens, a
/// status and a latency.
pub const TOOL_MODEL: &str = "tool";

/// The egress, ready to be served.
#[derive(Clone)]
pub struct Egress {
    inner: Arc<Inner>,
}

/// Install the TLS crypto provider once. Safe to call repeatedly; the launcher calls it
/// before any client of its own speaks TLS.
pub fn install_crypto() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

impl Egress {
    /// Build an egress over a memory, with the destinations the launcher allows. Every
    /// destination must have a ceiling in the grants file; that is what the label is
    /// checked against.
    ///
    /// # Errors
    ///
    /// A destination without a ceiling, a telemetry file that will not open, or an HTTP
    /// client that will not build.
    pub fn new(
        memory: Arc<Memory>,
        destinations: HashMap<String, Destination>,
        telemetry_path: &FsPath,
    ) -> Result<Egress, Error> {
        Egress::with_tool_servers(memory, destinations, HashMap::new(), telemetry_path)
    }

    /// As [`Egress::new`], with the tool servers the launcher calls for its hosted
    /// tools, by tool name: each must be a hosted tool in the grants, whose ceiling is
    /// what the label is checked against.
    ///
    /// # Errors
    ///
    /// As [`Egress::new`], or a tool server for a tool the grants do not host.
    pub fn with_tool_servers(
        memory: Arc<Memory>,
        destinations: HashMap<String, Destination>,
        tools: HashMap<String, Destination>,
        telemetry_path: &FsPath,
    ) -> Result<Egress, Error> {
        install_crypto();
        for name in destinations.keys() {
            if memory.grants().ceiling(name).is_none() {
                return Err(Error::NoCeiling(name.clone()));
            }
        }
        for name in tools.keys() {
            let hosted = memory
                .grants()
                .taps
                .get(name)
                .is_some_and(|t| t.hosted.is_some());
            if !hosted {
                return Err(Error::NotHosted(name.clone()));
            }
        }
        let client = reqwest::Client::builder()
            .use_rustls_tls()
            .connect_timeout(std::time::Duration::from_secs(20))
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .build()?;
        let telemetry = Telemetry::open(telemetry_path)?;
        // a destination that forwards the harness's own credential is its login
        telemetry.set_plan(
            destinations
                .iter()
                .filter(|(_, d)| matches!(d.auth, Auth::PassThrough))
                .map(|(n, _)| n.clone()),
        );
        Ok(Egress {
            inner: Arc::new(Inner {
                memory,
                destinations,
                tools,
                client,
                telemetry,
            }),
        })
    }

    /// One request to a tool server, from the launcher itself for a hosted tool: the
    /// session's label is checked against the tool's ceiling before anything leaves,
    /// the server's credential goes on here, and the call is recorded with its status
    /// and latency like a model call, under [`TOOL_MODEL`]. The answer comes back
    /// whatever its status; the caller reads it.
    ///
    /// # Errors
    ///
    /// A tool this egress has no server for, a session above the tool's ceiling, or a
    /// server that does not answer.
    pub async fn post(
        &self,
        tool: &str,
        path: &str,
        headers: HeaderMap,
        body: Bytes,
    ) -> Result<Relayed, Error> {
        let inner = &self.inner;
        let dest = inner
            .tools
            .get(tool)
            .ok_or_else(|| Error::Refused(tool.to_string()))?;
        let ceiling = inner
            .memory
            .grants()
            .taps
            .get(tool)
            .map_or(Label::Public, |t| t.ceiling);
        let label = inner.memory.label();
        if label > ceiling {
            return Err(Error::Refused(format!(
                "{tool}: this session has read {label} material and the tool accepts at most {ceiling}"
            )));
        }
        let rest = path.trim_start_matches('/');
        let mut req = Request::builder()
            .method(Method::POST)
            .uri(format!("/{tool}/{rest}"))
            .body(Body::empty())
            .map_err(|e| Error::Upstream(e.to_string()))?;
        *req.headers_mut() = headers;
        let started = std::time::SystemTime::now();
        let sent = upstream_request(inner, dest, rest, req, Some(body))
            .send()
            .await;
        let usage = Usage {
            model: Some(TOOL_MODEL.into()),
            ..Usage::default()
        };
        let resp = match sent {
            Ok(r) => r,
            Err(e) => {
                let _ = inner
                    .telemetry
                    .record(started, inner.memory.session_id(), tool, 0, &usage);
                return Err(Error::Upstream(format!(
                    "the tool server for {tool} did not answer: {e}"
                )));
            }
        };
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        let body = resp
            .text()
            .await
            .map_err(|e| Error::Upstream(e.to_string()))?;
        let _ = inner
            .telemetry
            .record(started, inner.memory.session_id(), tool, status, &usage);
        Ok(Relayed {
            status,
            headers,
            body,
        })
    }

    /// The usage log.
    #[must_use]
    pub fn telemetry(&self) -> &Telemetry {
        &self.inner.telemetry
    }

    /// A GET at a destination, from the launcher itself: a model list, say. The
    /// destination's credential is injected as on any call; nothing is metered, since
    /// no model answered.
    ///
    /// # Errors
    ///
    /// A destination the grants refuse, or an upstream that does not answer with JSON.
    pub async fn get(&self, destination: &str, path: &str) -> Result<serde_json::Value, Error> {
        let inner = &self.inner;
        let dest =
            admit(inner, destination).map_err(|_| Error::Refused(destination.to_string()))?;
        let req = Request::builder()
            .method(Method::GET)
            .uri(format!("/{destination}/{}", path.trim_start_matches('/')))
            .body(Body::empty())
            .map_err(|e| Error::Upstream(e.to_string()))?;
        let rest = path.trim_start_matches('/').to_string();
        let resp = upstream_request(inner, &dest, &rest, req, None)
            .send()
            .await
            .map_err(|e| Error::Upstream(e.to_string()))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| Error::Upstream(e.to_string()))?;
        if !status.is_success() {
            return Err(Error::Upstream(format!(
                "{status} from {destination}/{rest}: {}",
                text.chars().take(300).collect::<String>()
            )));
        }
        serde_json::from_str(&text).map_err(|e| Error::Upstream(format!("not JSON: {e}")))
    }

    /// Call a destination directly with a JSON body, as the launcher does for its own
    /// model calls, such as mining a transcript. The same ceiling check, credential
    /// injection and usage recording as a harness's call through the proxy.
    ///
    /// # Errors
    ///
    /// A destination this egress does not have or may not send to, an upstream that
    /// cannot be reached, or a non-success status, with the upstream's body in the message.
    pub async fn call(
        &self,
        destination: &str,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value, Error> {
        let inner = &self.inner;
        let dest =
            admit(inner, destination).map_err(|_| Error::Refused(destination.to_string()))?;
        if let Some(reason) = over_budget(inner, destination) {
            return Err(Error::OverBudget(reason));
        }
        let rest = path.trim_start_matches('/').to_string();
        let shaped = shape_body(&dest, &rest, Bytes::from(body.to_string()));
        let req = Request::builder()
            .method(Method::POST)
            .uri(format!("/{destination}/{rest}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(shaped))
            .map_err(|e| Error::Upstream(e.to_string()))?;
        let started = std::time::SystemTime::now();
        let resp = upstream_request(inner, &dest, &rest, req, None)
            .send()
            .await
            .map_err(|e| Error::Upstream(e.to_string()))?;
        let status = resp.status().as_u16();
        let text = resp
            .text()
            .await
            .map_err(|e| Error::Upstream(e.to_string()))?;
        let value: serde_json::Value =
            serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
        let usage = usage::from_value(&value).unwrap_or_else(|| Usage {
            model: Some(format!("http {status}")),
            ..Usage::default()
        });
        let _ = inner.telemetry.record(
            started,
            inner.memory.session_id(),
            destination,
            status,
            &usage,
        );
        if (200..300).contains(&status) {
            let _ = inner
                .memory
                .keep_context(
                    destination,
                    &rest,
                    body.to_string().as_bytes(),
                    text.as_bytes(),
                )
                .await;
        }
        if !(200..300).contains(&status) {
            return Err(Error::Upstream(format!(
                "{destination} answered {status}: {text}"
            )));
        }
        Ok(value)
    }

    /// The router: `/mcp` plus one forwarding prefix per destination.
    pub fn router(&self) -> Router {
        let memory = self.inner.memory.clone();
        let mcp = StreamableHttpService::new(
            move || Ok(McpServer::new(memory.clone())),
            Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig::default(),
        );
        Router::new()
            .nest_service("/mcp", mcp)
            .route("/{destination}", any(forward_root))
            .route("/{destination}/{*rest}", any(forward))
            .fallback(connect_or_not_found)
            .with_state(self.clone())
    }

    /// Serve on a unix socket until the task is cancelled. Removes a stale socket first.
    /// Unix only; Windows runs the harness unconfined over TCP.
    ///
    /// # Errors
    ///
    /// When the socket cannot be bound or serving fails.
    #[cfg(unix)]
    pub async fn serve_unix(&self, path: &FsPath) -> Result<(), Error> {
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        let listener = tokio::net::UnixListener::bind(path)?;
        axum::serve(listener, self.router()).await?;
        Ok(())
    }

    /// Serve on a TCP address, for a harness that is not confined.
    ///
    /// # Errors
    ///
    /// When the address cannot be bound or serving fails.
    pub async fn serve_tcp(&self, addr: std::net::SocketAddr) -> Result<(), Error> {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        axum::serve(listener, self.router()).await?;
        Ok(())
    }
}

async fn forward_root(
    State(egress): State<Egress>,
    Path(destination): Path<String>,
    req: Request,
) -> Response {
    proxy(egress, destination, String::new(), req).await
}

async fn forward(
    State(egress): State<Egress>,
    Path((destination, rest)): Path<(String, String)>,
    req: Request,
) -> Response {
    proxy(egress, destination, rest, req).await
}

/// Headers that belong to one hop and must not be forwarded, plus `host`, which names
/// the wrong server, and `content-length`, which the client recomputes.
const HOP_BY_HOP: &[&str] = &[
    // not hop-by-hop, but asking upstream for a compressed body would hide the usage block
    // from the sniffer; the harness is on loopback and does not need the saving
    "accept-encoding",
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "host",
    "content-length",
];

/// What the harness may have sent as its own credential, stripped when the egress
/// injects one of its own.
const CREDENTIAL_HEADERS: &[&str] = &[
    "authorization",
    "x-api-key",
    "x-goog-api-key",
    "openai-organization",
];

fn error_json(status: StatusCode, kind: &str, message: &str) -> Response {
    let body = serde_json::json!({
        "type": "error",
        "error": { "type": kind, "message": message }
    });
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

fn copy_headers(from: &HeaderMap, strip_credentials: bool) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in from {
        let n = name.as_str();
        if HOP_BY_HOP.contains(&n) {
            continue;
        }
        if strip_credentials && CREDENTIAL_HEADERS.contains(&n) {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    out
}

/// The upstream URL. When the egress injects the credential, a `key=` query parameter
/// (how Google's SDKs may send an API key) is dropped along with the credential headers.
fn upstream_url(base: &str, rest: &str, uri: &Uri, strip_credentials: bool) -> String {
    let mut url = format!("{}/{}", base.trim_end_matches('/'), rest);
    if let Some(q) = uri.query() {
        let kept: Vec<&str> = q
            .split('&')
            .filter(|kv| !(strip_credentials && (kv.starts_with("key=") || *kv == "key")))
            .collect();
        if !kept.is_empty() {
            url.push('?');
            url.push_str(&kept.join("&"));
        }
    }
    url
}

/// A path whose first segment names no destination, when the session's only destination
/// is addressed at the origin: that one, with the whole path as the remainder. Muse Code
/// asks for its model catalog at `/muse-code/models` on the base URL's origin, whatever
/// path the base carries.
fn only_destination(inner: &Inner, destination: String, rest: String) -> (String, String) {
    if inner.destinations.contains_key(&destination) || inner.destinations.len() != 1 {
        return (destination, rest);
    }
    let Some((only, d)) = inner.destinations.iter().next() else {
        return (destination, rest);
    };
    // Muse Code's catalog at any other destination: the OpenAI model list, which Muse
    // reads as its catalog, so a gateway's models are Muse's models
    if destination == "muse-code" && rest == "models" && !d.origin_paths {
        return (only.clone(), "v1/models".to_string());
    }
    if !d.origin_paths {
        return (destination, rest);
    }
    let only = only.clone();
    let whole = if rest.is_empty() {
        destination
    } else {
        format!("{destination}/{rest}")
    };
    (only, whole)
}

/// The destination for a request, or the response that refuses it: unknown name, or a
/// session label above the destination's ceiling.
/// The spend limits, before a model call leaves: what this session and the root have
/// spent at the destination, as metered here, against the policy. Over a limit the
/// call is refused with the reason, as a permission error, which a harness does not
/// retry.
fn over_budget(inner: &Inner, destination: &str) -> Option<String> {
    let day_ago = time::OffsetDateTime::now_utc() - time::Duration::days(1);
    let session = inner
        .telemetry
        .spent(destination, Some(inner.memory.session_id()), None)
        .unwrap_or(0);
    let day = inner
        .telemetry
        .spent(destination, None, Some(day_ago))
        .unwrap_or(0);
    match inner.memory.check_spend(destination, session, day) {
        Ok(d) if !d.allow => Some(d.reason),
        _ => None,
    }
}

fn admit(inner: &Inner, destination: &str) -> Result<Destination, Box<Response>> {
    let Some(dest) = inner.destinations.get(destination) else {
        return Err(Box::new(error_json(
            StatusCode::NOT_FOUND,
            "not_found_error",
            &format!("no destination {destination}"),
        )));
    };
    let ceiling = inner
        .memory
        .grants()
        .ceiling(destination)
        .unwrap_or(Label::Public);
    let label = inner.memory.label();
    if label > ceiling {
        return Err(Box::new(error_json(
            StatusCode::FORBIDDEN,
            "permission_error",
            &format!(
                "this session has read {label} material and {destination} accepts at most {ceiling}; start a fresh session for this call"
            ),
        )));
    }
    if dest.upstream.is_empty() {
        return Err(Box::new(error_json(
            StatusCode::NOT_FOUND,
            "not_found_error",
            &format!("{destination} is a tunnel destination; use CONNECT"),
        )));
    }
    Ok(dest.clone())
}

/// Whether an allowlist entry admits a host: exact, or, for an entry starting with a
/// dot, the domain itself and any subdomain.
fn host_allowed(entry: &str, host: &str) -> bool {
    match entry.strip_prefix('.') {
        Some(domain) => host == domain || host.ends_with(&format!(".{domain}")),
        None => entry == host,
    }
}

/// The port a tunnel entry allows when it names none.
const TUNNEL_PORT: u16 = 443;

/// A host and its port, from `host`, `host:port`, `[v6]` or `[v6]:port`, with the
/// brackets taken off; a bare IPv6 address is a host with no port. `None` for a port
/// that is not a number or brackets that do not close.
fn host_and_port(s: &str) -> Option<(&str, Option<u16>)> {
    if let Some(rest) = s.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        return match after {
            "" => Some((host, None)),
            _ => Some((host, Some(after.strip_prefix(':')?.parse().ok()?))),
        };
    }
    match s.split_once(':') {
        Some((host, port)) if !port.contains(':') => Some((host, Some(port.parse().ok()?))),
        _ => Some((s, None)),
    }
}

/// Whether a tunnel entry admits `host` on `port`: the host as [`host_allowed`] has it,
/// and the port the entry names, or [`TUNNEL_PORT`] when it names none.
fn entry_allows(entry: &str, host: &str, port: u16) -> bool {
    host_and_port(entry)
        .is_some_and(|(h, p)| p.unwrap_or(TUNNEL_PORT) == port && host_allowed(h, host))
}

/// Whether a tunnel may not reach an address a name resolved to: an address that is
/// this machine, a network it sits on, its cloud's metadata service, or no host on the
/// internet. The same for an IPv4 address carried in an IPv6 one.
fn denied(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => denied_v4(v),
        IpAddr::V6(v) => denied_v6(v),
    }
}

fn denied_v4(v: Ipv4Addr) -> bool {
    let [a, b, ..] = v.octets();
    // link-local holds the metadata address, 169.254.169.254; broadcast is in 240/4
    v.is_unspecified()
        || v.is_loopback()
        || v.is_private()
        || v.is_link_local()
        || v.is_multicast()
        || v.is_documentation()
        || a == 0
        || a >= 240
        || (a == 100 && (b & 0xc0) == 64)
        || (a == 198 && (b & 0xfe) == 18)
}

fn denied_v6(v: Ipv6Addr) -> bool {
    let s = v.segments();
    // unique local, fc00::/7, holds the metadata address fd00:ec2::254
    v.is_unspecified()
        || v.is_loopback()
        || v.is_multicast()
        || v.is_unique_local()
        || v.is_unicast_link_local()
        || (s[0] & 0xffc0) == 0xfec0
        || (s[0] == 0x2001 && s[1] == 0x0db8)
        || (s[0] == 0x3fff && (s[1] & 0xf000) == 0)
        || embedded_v4(v).is_some_and(denied_v4)
}

/// The IPv4 address an IPv6 one carries: mapped, `::ffff:a.b.c.d`; compatible,
/// `::a.b.c.d`; or behind NAT64's well-known prefix, `64:ff9b::a.b.c.d`.
fn embedded_v4(v: Ipv6Addr) -> Option<Ipv4Addr> {
    let s = v.segments();
    if s[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
        let [.., a, b, c, d] = v.octets();
        return Some(Ipv4Addr::new(a, b, c, d));
    }
    v.to_ipv4()
}

/// The addresses a tunnel to `host` may dial: an entry that is an address is the
/// launcher's own choice and is dialled as named; a name is resolved here, once, and
/// refused when any address it resolves to is [`denied`]. The dial goes to these
/// addresses, so a second answer for the name cannot change where the tunnel goes.
async fn tunnel_addresses(
    entry_host: &str,
    host: &str,
    port: u16,
) -> Result<Vec<SocketAddr>, Box<Response>> {
    if let Ok(ip) = entry_host.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    let addrs: Vec<SocketAddr> = match tokio::net::lookup_host((host, port)).await {
        Ok(found) => found.collect(),
        Err(e) => {
            return Err(Box::new(error_json(
                StatusCode::BAD_GATEWAY,
                "api_error",
                &format!("{host} does not resolve: {e}"),
            )))
        }
    };
    if addrs.is_empty() {
        return Err(Box::new(error_json(
            StatusCode::BAD_GATEWAY,
            "api_error",
            &format!("{host} resolves to no address"),
        )));
    }
    if let Some(bad) = addrs.iter().find(|a| denied(a.ip())) {
        return Err(Box::new(error_json(
            StatusCode::FORBIDDEN,
            "permission_error",
            &format!(
                "{host} resolves to {}, which a tunnel may not reach",
                bad.ip()
            ),
        )));
    }
    Ok(addrs)
}

/// What a `CONNECT` to `authority` tunnels under: the destination whose entry allows
/// the host on that port, within the session's ceiling, and the checked addresses to
/// dial. The response that refuses it otherwise.
async fn tunnel_for(
    inner: &Inner,
    authority: &str,
) -> Result<(String, Vec<SocketAddr>), Box<Response>> {
    let refuse = |message: &str| {
        Box::new(error_json(
            StatusCode::FORBIDDEN,
            "permission_error",
            message,
        ))
    };
    let Some((host, port)) = host_and_port(authority) else {
        return Err(Box::new(error_json(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            &format!("{authority} is not a host and port"),
        )));
    };
    let port = port.unwrap_or(TUNNEL_PORT);
    let found = inner.destinations.iter().find_map(|(name, d)| {
        d.tunnel_hosts
            .iter()
            .find(|e| entry_allows(e, host, port))
            .map(|e| (name, e))
    });
    let Some((name, entry)) = found else {
        return Err(refuse(&format!(
            "no destination allows a tunnel to {host}:{port}"
        )));
    };
    let ceiling = inner.memory.grants().ceiling(name).unwrap_or(Label::Public);
    let label = inner.memory.label();
    if label > ceiling {
        return Err(refuse(&format!(
            "this session has read {label} material and {name} accepts at most {ceiling}"
        )));
    }
    let entry_host = host_and_port(entry).map_or(entry.as_str(), |(h, _)| h);
    let addrs = tunnel_addresses(entry_host, host, port).await?;
    Ok((name.clone(), addrs))
}

/// `CONNECT host:port`: open a TCP tunnel when some destination allows the host on that
/// port, the session's label is within that destination's ceiling, and the host's
/// addresses are ones a tunnel may reach. Anything else here is a 404.
async fn connect_or_not_found(State(egress): State<Egress>, mut req: Request) -> Response {
    if req.method() != Method::CONNECT {
        return error_json(StatusCode::NOT_FOUND, "not_found_error", "no such path");
    }
    let inner = egress.inner.clone();
    let authority = req
        .uri()
        .authority()
        .map(|a| a.to_string())
        .or_else(|| {
            req.headers()
                .get(header::HOST)
                .and_then(|h| h.to_str().ok())
                .map(str::to_string)
        })
        .unwrap_or_default();
    let (name, addrs) = match tunnel_for(&inner, &authority).await {
        Ok(found) => found,
        Err(refusal) => return *refusal,
    };
    let on_upgrade = hyper::upgrade::on(&mut req);
    let session = inner.memory.session_id().to_string();
    let started = std::time::SystemTime::now();
    tokio::spawn(async move {
        let Ok(upgraded) = on_upgrade.await else {
            return;
        };
        let Ok(mut outbound) = tokio::net::TcpStream::connect(&addrs[..]).await else {
            return;
        };
        let mut inbound = hyper_util::rt::TokioIo::new(upgraded);
        let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
        let _ = inner.telemetry.record(
            started,
            &session,
            &name,
            200,
            &Usage {
                model: Some("tunnel".into()),
                ..Usage::default()
            },
        );
    });
    Response::new(Body::empty())
}

/// The upstream request: the harness's request with hop-by-hop headers dropped, the
/// credential injected where the destination says so, and the body streamed through.
fn upstream_request(
    inner: &Inner,
    dest: &Destination,
    rest: &str,
    req: Request,
    replaced: Option<Bytes>,
) -> reqwest::RequestBuilder {
    let (parts, body) = req.into_parts();
    let inject = matches!(dest.auth, Auth::Inject { .. });
    let url = upstream_url(&dest.upstream, rest, &parts.uri, inject);
    let method = reqwest::Method::from_bytes(parts.method.as_str().as_bytes())
        .unwrap_or(reqwest::Method::GET);
    let mut headers = copy_headers(&parts.headers, inject);
    if let Auth::Inject { header, value } = &dest.auth {
        if let Ok(v) = HeaderValue::from_str(value) {
            headers.insert(header.clone(), v);
        }
    }
    let mut builder = inner.client.request(method.clone(), &url).headers(headers);
    if let Some(bytes) = replaced {
        builder = builder.body(bytes);
    } else if method != Method::GET && method != Method::HEAD {
        let stream = body
            .into_data_stream()
            .map(|r| r.map_err(|e| std::io::Error::other(e.to_string())));
        builder = builder.body(reqwest::Body::wrap_stream(stream));
    }
    builder
}

/// A Messages body with its `system`-role messages folded into `system`, when there are
/// any; `None` when there is nothing to fold or the body is not a JSON object.
#[must_use]
pub fn fold_system_messages(body: &[u8]) -> Option<Vec<u8>> {
    let mut v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let messages = v.get_mut("messages")?.as_array_mut()?;
    let stray: Vec<serde_json::Value> = messages
        .iter()
        .filter(|m| m.get("role").and_then(serde_json::Value::as_str) == Some("system"))
        .cloned()
        .collect();
    if stray.is_empty() {
        return None;
    }
    messages.retain(|m| m.get("role").and_then(serde_json::Value::as_str) != Some("system"));
    let mut system: Vec<serde_json::Value> = match v.get("system") {
        Some(serde_json::Value::String(t)) => {
            vec![serde_json::json!({ "type": "text", "text": t })]
        }
        Some(serde_json::Value::Array(a)) => a.clone(),
        _ => Vec::new(),
    };
    for m in stray {
        match m.get("content") {
            Some(serde_json::Value::String(t)) => {
                system.push(serde_json::json!({ "type": "text", "text": t }));
            }
            Some(serde_json::Value::Array(blocks)) => {
                system.extend(
                    blocks
                        .iter()
                        .filter(|b| {
                            b.get("type").and_then(serde_json::Value::as_str) == Some("text")
                        })
                        .cloned(),
                );
            }
            _ => {}
        }
    }
    v["system"] = serde_json::Value::Array(system);
    serde_json::to_vec(&v).ok()
}

/// A Gemini API body with each `model` turn's `functionResponse` parts moved into a
/// `user` turn right after it, the turn left out when nothing else was in it; `None`
/// when there are none or the body is not a JSON object.
///
/// TODO(fix upstream): LiteLLM's GoogleGenAI adapter
/// (`litellm/google_genai/adapters/transformation.py`, `_transform_contents_to_messages`)
/// translates a `functionResponse` only in a `user` turn and drops it from a `model`
/// one, as of 1.102.0 and main. Remove this fold once LiteLLM takes it in any role.
#[must_use]
pub fn fold_function_responses(body: &[u8]) -> Option<Vec<u8>> {
    let mut v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let contents = v.get_mut("contents")?.as_array_mut()?;
    let mut out = Vec::with_capacity(contents.len());
    let mut moved = false;
    for turn in contents.drain(..) {
        let from_model = turn.get("role").and_then(serde_json::Value::as_str) == Some("model");
        let parts = turn.get("parts").and_then(serde_json::Value::as_array);
        let Some(parts) = parts.filter(|p| from_model && p.iter().any(is_function_response)) else {
            out.push(turn);
            continue;
        };
        moved = true;
        // in order: each run of results becomes a user turn where it stood, and what the
        // model said around it stays in model turns on either side
        let mut runs: Vec<(bool, Vec<serde_json::Value>)> = Vec::new();
        for part in parts {
            let result = is_function_response(part);
            match runs.last_mut() {
                Some((was, run)) if *was == result => run.push(part.clone()),
                _ => runs.push((result, vec![part.clone()])),
            }
        }
        for (result, run) in runs {
            if result {
                out.push(serde_json::json!({ "role": "user", "parts": run }));
            } else {
                let mut kept = turn.clone();
                kept["parts"] = serde_json::Value::Array(run);
                out.push(kept);
            }
        }
    }
    *contents = out;
    moved.then(|| serde_json::to_vec(&v).ok()).flatten()
}

fn is_function_response(part: &serde_json::Value) -> bool {
    part.get("functionResponse").is_some()
}

/// A request the destination wants shaped is read whole, up to 64 MiB, and rewritten: a
/// Messages request with its system role folded, a Gemini request with a model turn's
/// tool results moved to a user turn, a request asking for more output tokens than the
/// cap. Everything else streams through untouched.
/// A request body as `dest` needs it: a late system message folded in, a Gemini tool
/// result moved to the user's role, the output asked for capped. The proxy and a direct
/// call both go through it, so a replay leaves as the session's own calls did.
fn shape_body(dest: &Destination, rest: &str, bytes: Bytes) -> Bytes {
    let path = rest.trim_end_matches('/');
    let mut out = bytes;
    if dest.fold_system_messages && path.ends_with("messages") {
        if let Some(folded) = fold_system_messages(&out) {
            out = Bytes::from(folded);
        }
    }
    if dest.fold_function_responses
        && (path.ends_with(":generateContent") || path.ends_with(":streamGenerateContent"))
    {
        if let Some(folded) = fold_function_responses(&out) {
            out = Bytes::from(folded);
        }
    }
    let capped_path =
        path.ends_with("messages") || path.ends_with("responses") || path.ends_with("completions");
    if let Some(cap) = dest.max_output_tokens.filter(|_| capped_path) {
        if let Some(capped) = cap_output_tokens(&out, cap) {
            out = Bytes::from(capped);
        }
    }
    out
}

async fn shaped(
    dest: &Destination,
    rest: &str,
    req: Request,
) -> Result<(Request, Option<Bytes>), Box<Response>> {
    // every POST is buffered: its body is the decision context the egress keeps
    if req.method() != Method::POST {
        return Ok((req, None));
    }
    let (parts, body) = req.into_parts();
    let bytes = to_bytes(body, 64 << 20).await.map_err(|e| {
        Box::new(error_json(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            &format!("request body: {e}"),
        ))
    })?;
    let out = shape_body(dest, rest, bytes.clone());
    if std::env::var_os("MEMORY_EGRESS_DEBUG").is_some() {
        // the request as it leaves, for an adapter whose harness says one thing
        // on a laptop and another in the sandbox
        // the parameters, not the prompt: the big fields are dropped from the print
        let shown = serde_json::from_slice::<serde_json::Value>(&out)
            .ok()
            .and_then(|mut v| {
                let o = v.as_object_mut()?;
                for k in [
                    "input",
                    "messages",
                    "tools",
                    "instructions",
                    "system",
                    "contents",
                    "systemInstruction",
                    "system_instruction",
                ] {
                    o.remove(k);
                }
                Some(v.to_string())
            })
            .unwrap_or_else(|| String::from_utf8_lossy(&out).chars().take(300).collect());
        eprintln!("egress debug: {rest} as shaped, parameters: {shown}");
    }
    Ok((Request::from_parts(parts, Body::empty()), Some(out)))
}

/// The body with its output-token request capped at `cap`, under whichever name the
/// API spells it; `None` when nothing is over the cap or the body is not a JSON object.
#[must_use]
pub fn cap_output_tokens(body: &[u8], cap: u64) -> Option<Vec<u8>> {
    let mut v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let obj = v.as_object_mut()?;
    let mut changed = false;
    for key in ["max_output_tokens", "max_tokens", "max_completion_tokens"] {
        if let Some(n) = obj.get(key).and_then(serde_json::Value::as_u64) {
            if n > cap {
                obj.insert(key.to_string(), serde_json::json!(cap));
                changed = true;
            }
        }
    }
    changed.then(|| serde_json::to_vec(&v).ok()).flatten()
}

async fn proxy(egress: Egress, destination: String, rest: String, req: Request) -> Response {
    let inner = &egress.inner;
    let (destination, rest) = only_destination(inner, destination, rest);
    let dest = match admit(inner, &destination) {
        Ok(d) => d,
        Err(refusal) => return *refusal,
    };
    // a model call spends; a listing does not
    if req.method() == Method::POST {
        if let Some(reason) = over_budget(inner, &destination) {
            return error_json(StatusCode::FORBIDDEN, "permission_error", &reason);
        }
    }
    let (req, replaced) = match shaped(&dest, &rest, req).await {
        Ok(pair) => pair,
        Err(refusal) => return *refusal,
    };
    let sent = replaced.clone();
    let builder = upstream_request(inner, &dest, &rest, req, replaced);
    let started = std::time::SystemTime::now();
    let upstream = match builder.send().await {
        Ok(r) => r,
        Err(e) => {
            return error_json(
                StatusCode::BAD_GATEWAY,
                "api_error",
                &format!("upstream {destination}: {e}"),
            )
        }
    };

    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    if !status.is_success() && std::env::var_os("MEMORY_EGRESS_DEBUG").is_some() {
        // a diagnostic: who refused, and how. The body is read here and not relayed, so
        // this is for finding out, not for running.
        let url = upstream.url().to_string();
        let server = upstream
            .headers()
            .get("server")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("-")
            .to_string();
        let cf = upstream.headers().contains_key("cf-ray");
        let body = upstream.text().await.unwrap_or_default();
        eprintln!(
            "egress debug: {status} from {url} server={server} cloudflare={cf} body={:?}",
            body.chars().take(300).collect::<String>()
        );
        return error_json(
            status,
            "upstream_error",
            &body.chars().take(300).collect::<String>(),
        );
    }
    let resp_headers = copy_headers(upstream.headers(), false);
    let content_type = upstream
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    let body_stream = upstream
        .bytes_stream()
        .map(|chunk| chunk.map_err(|e| std::io::Error::other(e.to_string())));
    let (body_stream, tapped) = tap(body_stream);
    let (tx, rx) = tokio::sync::oneshot::channel::<Option<Usage>>();
    let stream = usage_tail(body_stream, content_type.as_deref(), tx);
    let context = Sent {
        path: rest,
        request: sent,
        response: tapped,
    };
    record_when_done(egress.clone(), started, destination, status, rx, context);

    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    *response.headers_mut() = resp_headers;
    response
}

/// Record the call once its usage is known, when the body has streamed through. A call
/// that reported usage is recorded as such; a call the upstream refused is recorded by
/// its status, so a refused key or an expired login shows up in `memory usage` rather
/// than vanishing; anything else, a listing say, is not a model call.
/// A copy of a response as it streams through, up to a cap, for its decision context.
fn tap<S>(
    stream: S,
) -> (
    impl futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
    Arc<std::sync::Mutex<Vec<u8>>>,
)
where
    S: futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
{
    let tapped = Arc::new(std::sync::Mutex::new(Vec::new()));
    let copy = tapped.clone();
    let stream = stream.map(move |chunk| {
        if let Ok(bytes) = &chunk {
            let mut t = copy
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if t.len() < dm_server::contexts::MAX_BYTES {
                t.extend_from_slice(bytes);
            }
        }
        chunk
    });
    (stream, tapped)
}

/// What a proxied model call sent and got back, for its decision context.
struct Sent {
    path: String,
    request: Option<Bytes>,
    response: Arc<std::sync::Mutex<Vec<u8>>>,
}

fn record_when_done(
    egress: Egress,
    started: std::time::SystemTime,
    destination: String,
    status: StatusCode,
    rx: tokio::sync::oneshot::Receiver<Option<Usage>>,
    sent: Sent,
) {
    let status_code = status.as_u16();
    tokio::spawn(async move {
        let usage = match rx.await {
            Ok(Some(u)) => u,
            Ok(None) if !status.is_success() => Usage {
                model: Some(format!("http {status_code}")),
                ..Usage::default()
            },
            _ => return,
        };
        let inner = &egress.inner;
        if let Err(e) = inner.telemetry.record(
            started,
            inner.memory.session_id(),
            &destination,
            status_code,
            &usage,
        ) {
            eprintln!("egress: telemetry: {e}");
        }
        // a model call that answered: its context is kept, as the egress saw it
        if let (true, Some(request)) = (status.is_success(), &sent.request) {
            let response = sent
                .response
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let _ = inner
                .memory
                .keep_context(&destination, &sent.path, request, &response)
                .await;
        }
    });
}

/// Wrap a byte stream so that the chunks are also fed to a sniffer, and when the stream
/// ends, or is dropped early, the sniffer's verdict is sent on `tx` exactly once.
fn usage_tail<S>(
    stream: S,
    content_type: Option<&str>,
    tx: tokio::sync::oneshot::Sender<Option<Usage>>,
) -> impl futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send
where
    S: futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
{
    struct Tail<S> {
        inner: S,
        sniffer: Option<Sniffer>,
        tx: Option<tokio::sync::oneshot::Sender<Option<Usage>>>,
    }
    impl<S> futures_util::Stream for Tail<S>
    where
        S: futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Unpin,
    {
        type Item = Result<Bytes, std::io::Error>;
        fn poll_next(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            let this = &mut *self;
            match std::pin::Pin::new(&mut this.inner).poll_next(cx) {
                std::task::Poll::Ready(Some(Ok(bytes))) => {
                    if let Some(s) = this.sniffer.as_mut() {
                        s.feed(&bytes);
                    }
                    std::task::Poll::Ready(Some(Ok(bytes)))
                }
                std::task::Poll::Ready(Some(Err(e))) => {
                    this.sniffer = None;
                    std::task::Poll::Ready(Some(Err(e)))
                }
                std::task::Poll::Ready(None) => {
                    if let Some(tx) = this.tx.take() {
                        let _ = tx.send(this.sniffer.take().and_then(Sniffer::finish));
                    }
                    std::task::Poll::Ready(None)
                }
                std::task::Poll::Pending => std::task::Poll::Pending,
            }
        }
    }
    impl<S> Drop for Tail<S> {
        fn drop(&mut self) {
            if let Some(tx) = self.tx.take() {
                let _ = tx.send(self.sniffer.take().and_then(Sniffer::finish));
            }
        }
    }
    Tail {
        inner: Box::pin(stream),
        sniffer: Some(Sniffer::new(content_type)),
        tx: Some(tx),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt;

    async fn fixture(dest: Option<(&str, Destination)>) -> (tempfile::TempDir, Egress) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("m");
        let m = Memory::init(&root, "human:test:me", "dom-me").unwrap();
        // the starter grants carry anthropic/openai/llmao ceilings at internal
        let m = Arc::new(m);
        let mut d = HashMap::new();
        if let Some((n, v)) = dest {
            d.insert(n.to_string(), v);
        }
        let e = Egress::new(m, d, &dir.path().join("t.db")).unwrap();
        (dir, e)
    }

    #[tokio::test]
    async fn unknown_destination_is_404_and_mcp_is_mounted() {
        let (_d, e) = fixture(None).await;
        let app = e.router();
        let r = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/nowhere/v1/messages")
                    .method("POST")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        let r = app
            .oneshot(
                Request::builder()
                    .uri("/mcp")
                    .method("POST")
                    .header("host", "127.0.0.1:4100")
                    .header("content-type", "application/json")
                    .header("accept", "application/json, text/event-stream")
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            r.status().is_success(),
            "mcp initialize over http: {}",
            r.status()
        );
        let body = to_bytes(r.into_body(), 1 << 20).await.unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("\"serverInfo\""), "{text}");
    }

    #[tokio::test]
    async fn a_destination_without_a_ceiling_is_refused_at_build_time() {
        let dir = tempfile::tempdir().unwrap();
        let m = Arc::new(Memory::init(&dir.path().join("m"), "human:test:me", "dom-me").unwrap());
        let mut d = HashMap::new();
        d.insert(
            "nowhere".to_string(),
            Destination::forward("http://127.0.0.1:1", Auth::PassThrough),
        );
        assert!(matches!(
            Egress::new(m, d, &dir.path().join("t.db")),
            Err(Error::NoCeiling(_))
        ));
    }

    #[tokio::test]
    async fn a_direct_call_goes_through_the_same_destination_and_is_recorded() {
        let fake = Router::new().route(
            "/v1/chat/completions",
            any(|req: Request| async move {
                let (_, body) = req.into_parts();
                let b = to_bytes(body, 1 << 20).await.unwrap();
                assert!(String::from_utf8_lossy(&b).contains("extract"));
                (
                    [(header::CONTENT_TYPE, "application/json")],
                    r#"{"model":"gpt-x","choices":[{"message":{"content":"[]"}}],"usage":{"prompt_tokens":40,"completion_tokens":2}}"#,
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, fake).await.unwrap() });
        let (_d, e) = fixture(Some((
            "openai",
            Destination::forward(
                format!("http://{addr}"),
                Auth::Inject {
                    header: header::AUTHORIZATION,
                    value: "Bearer sk-real".into(),
                },
            ),
        )))
        .await;
        let v = e
            .call(
                "openai",
                "v1/chat/completions",
                &serde_json::json!({ "messages": [{ "role": "user", "content": "extract" }] }),
            )
            .await
            .unwrap();
        assert_eq!(v["choices"][0]["message"]["content"], "[]");
        let rows = e.telemetry().summary().unwrap();
        let row = rows.iter().find(|r| r.destination == "openai").unwrap();
        assert_eq!(row.model, "gpt-x");
        assert_eq!(row.input_tokens, 40);
        assert!(matches!(
            e.call("nowhere", "v1/x", &serde_json::json!({})).await,
            Err(Error::Refused(_))
        ));
    }

    #[tokio::test]
    async fn a_model_call_over_the_spend_limit_does_not_leave() {
        let fake = Router::new().route(
            "/v1/chat/completions",
            any(|| async {
                (
                    [(header::CONTENT_TYPE, "application/json")],
                    r#"{"model":"m","choices":[{"message":{"content":"ok"}}],"usage":{"prompt_tokens":40,"completion_tokens":2}}"#,
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, fake).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("m");
        Memory::init(&root, "human:test:me", "dom-me").unwrap();
        let grants = root.join("grants.toml");
        let text = std::fs::read_to_string(&grants).unwrap();
        std::fs::write(
            &grants,
            format!("{text}\n[spend.openai]\nsession_tokens = 50\n"),
        )
        .unwrap();
        let m = Arc::new(Memory::open(&root).unwrap().with_author("agent:a"));
        let mut d = HashMap::new();
        d.insert(
            "openai".to_string(),
            Destination::forward(format!("http://{addr}"), Auth::PassThrough),
        );
        let e = Egress::new(m, d, &dir.path().join("t.db")).unwrap();
        let ask = serde_json::json!({ "messages": [{ "role": "user", "content": "hi" }] });
        e.call("openai", "v1/chat/completions", &ask).await.unwrap();
        e.call("openai", "v1/chat/completions", &ask).await.unwrap();
        let refused = e
            .call("openai", "v1/chat/completions", &ask)
            .await
            .unwrap_err();
        assert!(refused.to_string().contains("spend limit"), "{refused}");
        let r = e
            .router()
            .oneshot(
                Request::builder()
                    .uri("/openai/v1/chat/completions")
                    .method("POST")
                    .header("content-type", "application/json")
                    .body(Body::from(ask.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        let body = to_bytes(r.into_body(), 1 << 20).await.unwrap();
        assert!(
            String::from_utf8_lossy(&body).contains("84 tokens"),
            "{}",
            String::from_utf8_lossy(&body)
        );
        let log = std::fs::read_to_string(root.join("state/decisions.jsonl")).unwrap();
        assert!(
            log.contains("\"action\":\"spend\"") && log.contains("\"allow\":false"),
            "{log}"
        );
    }

    #[tokio::test]
    async fn every_model_call_leaves_its_decision_context() {
        let fake = Router::new().route(
            "/v1/chat/completions",
            any(|| async {
                (
                    [(header::CONTENT_TYPE, "application/json")],
                    r#"{"model":"m","choices":[{"message":{"tool_calls":[{"function":{"name":"search","arguments":"{}"}}]}}],"usage":{"prompt_tokens":40,"completion_tokens":2}}"#,
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, fake).await.unwrap() });
        let (_d, e) = fixture(Some((
            "openai",
            Destination::forward(format!("http://{addr}"), Auth::PassThrough),
        )))
        .await;
        let ask = serde_json::json!({ "model": "m", "messages": [{ "role": "user", "content": "find the retry policy" }] });
        e.call("openai", "v1/chat/completions", &ask).await.unwrap();
        let r = e
            .router()
            .oneshot(
                Request::builder()
                    .uri("/openai/v1/chat/completions")
                    .method("POST")
                    .header("content-type", "application/json")
                    .body(Body::from(ask.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(r.status().is_success());
        let _ = to_bytes(r.into_body(), 1 << 20).await.unwrap();
        // the proxied call's context is kept when its stream has ended
        let m = e.inner.memory.clone();
        let mut kept = Vec::new();
        for _ in 0..50 {
            kept = m.contexts(m.session_id()).await.unwrap_or_default();
            if kept.len() == 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(kept.len(), 2, "both calls kept");
        for c in &kept {
            assert_eq!(c.request, ask, "the request exactly as it left");
            assert_eq!(
                c.tools,
                vec!["search".to_string()],
                "the tool the model asked for"
            );
            assert_eq!(c.destination, "openai");
        }
    }

    #[test]
    fn antigravity_s_tool_results_move_from_the_model_s_turn_to_the_user_s() {
        // as Antigravity CLI sends them: the call and its result, both under the model
        let body = serde_json::json!({
            "contents": [
                {"role": "user", "parts": [{"text": "read it"}]},
                {"role": "model", "parts": [{"functionCall": {"id": "c1", "name": "view_file", "args": {}}}]},
                {"role": "model", "parts": [{"functionResponse": {"id": "c1", "name": "view_file", "response": {"output": "zebra"}}}]},
                {"role": "model", "parts": [
                    {"text": "and one more"},
                    {"functionCall": {"id": "c2", "name": "view_file", "args": {}}},
                    {"functionResponse": {"id": "c2", "name": "view_file", "response": {"output": "okapi"}}}
                ]}
            ],
            "tools": []
        });
        let out = fold_function_responses(&serde_json::to_vec(&body).unwrap()).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let roles: Vec<&str> = v["contents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["role"].as_str().unwrap())
            .collect();
        assert_eq!(roles, ["user", "model", "user", "model", "user"]);
        assert_eq!(
            v["contents"][2]["parts"][0]["functionResponse"]["response"]["output"],
            "zebra"
        );
        assert_eq!(
            v["contents"][3]["parts"].as_array().unwrap().len(),
            2,
            "text and call stay"
        );
        assert_eq!(
            v["contents"][4]["parts"][0]["functionResponse"]["response"]["output"],
            "okapi"
        );
        assert_eq!(
            v["tools"],
            serde_json::json!([]),
            "the rest of the body is kept"
        );
        // a user's result, and a body with none, are left as they are
        let plain = serde_json::json!({ "contents": [
            {"role": "model", "parts": [{"functionCall": {"name": "f", "args": {}}}]},
            {"role": "user", "parts": [{"functionResponse": {"name": "f", "response": {}}}]}
        ]});
        assert!(fold_function_responses(&serde_json::to_vec(&plain).unwrap()).is_none());
        assert!(fold_function_responses(b"not json").is_none());
    }

    #[test]
    fn a_turn_that_goes_on_after_a_result_keeps_its_order() {
        // a turn that goes on after a result keeps its order: the model's remark on the
        // first result stays after it, and two results together stay together, in order
        let interleaved = serde_json::json!({ "contents": [{"role": "model", "parts": [
            {"functionCall": {"id": "c1", "name": "f", "args": {}}},
            {"functionResponse": {"id": "c1", "name": "f", "response": {"output": "zebra"}}},
            {"text": "c1 said zebra, so now c2 and c3"},
            {"functionCall": {"id": "c2", "name": "f", "args": {}}},
            {"functionCall": {"id": "c3", "name": "f", "args": {}}},
            {"functionResponse": {"id": "c2", "name": "f", "response": {"output": "okapi"}}},
            {"functionResponse": {"id": "c3", "name": "f", "response": {"output": "tapir"}}}
        ]}]});
        let v: serde_json::Value = serde_json::from_slice(
            &fold_function_responses(&serde_json::to_vec(&interleaved).unwrap()).unwrap(),
        )
        .unwrap();
        let shape: Vec<(String, Vec<String>)> = v["contents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| {
                let parts = t["parts"].as_array().unwrap().iter().map(|p| {
                    p.get("functionCall")
                        .map(|c| format!("call {}", c["id"].as_str().unwrap()))
                        .or_else(|| {
                            p.get("functionResponse").map(|r| {
                                format!("result {}", r["response"]["output"].as_str().unwrap())
                            })
                        })
                        .unwrap_or_else(|| "text".into())
                });
                (t["role"].as_str().unwrap().to_string(), parts.collect())
            })
            .collect();
        let expect = |role: &str, parts: &[&str]| {
            (
                role.to_string(),
                parts.iter().map(|p| (*p).to_string()).collect::<Vec<_>>(),
            )
        };
        assert_eq!(
            shape,
            vec![
                expect("model", &["call c1"]),
                expect("user", &["result zebra"]),
                expect("model", &["text", "call c2", "call c3"]),
                expect("user", &["result okapi", "result tapir"]),
            ]
        );
    }

    #[test]
    fn claude_code_s_trailing_system_message_is_folded_into_system() {
        let body = serde_json::json!({
            "model": "m",
            "system": [{"type": "text", "text": "a"}],
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "hi"}]},
                {"role": "system", "content": [{"type": "text", "text": "# Environment"}]}
            ]
        });
        let out = fold_system_messages(&serde_json::to_vec(&body).unwrap()).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["messages"].as_array().unwrap().len(), 1);
        assert_eq!(v["system"][1]["text"], "# Environment");
        // a string system and a string content both become blocks
        let body = serde_json::json!({
            "system": "s",
            "messages": [{"role": "system", "content": "e"}, {"role": "user", "content": "u"}]
        });
        let v: serde_json::Value = serde_json::from_slice(
            &fold_system_messages(&serde_json::to_vec(&body).unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(v["system"][0]["text"], "s");
        assert_eq!(v["system"][1]["text"], "e");
        // nothing to fold, nothing changed
        let plain = serde_json::json!({ "messages": [{"role": "user", "content": "u"}] });
        assert!(fold_system_messages(&serde_json::to_vec(&plain).unwrap()).is_none());
        assert!(fold_system_messages(b"not json").is_none());
    }

    #[test]
    fn an_output_token_request_over_the_cap_is_capped() {
        let body = serde_json::json!({ "model": "m", "max_output_tokens": 128_000, "input": "hi" });
        let out = cap_output_tokens(&serde_json::to_vec(&body).unwrap(), 32_768).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["max_output_tokens"], 32_768);
        let under = serde_json::json!({ "max_tokens": 100 });
        assert!(cap_output_tokens(&serde_json::to_vec(&under).unwrap(), 32_768).is_none());
        assert!(cap_output_tokens(b"[]", 1).is_none());
    }

    #[tokio::test]
    async fn a_get_from_the_launcher_carries_the_credential_and_returns_json() {
        let seen = Arc::new(tokio::sync::Mutex::new(None::<HeaderMap>));
        let seen2 = seen.clone();
        let fake = Router::new().route(
            "/v1/models",
            any(move |req: Request| {
                let seen = seen2.clone();
                async move {
                    *seen.lock().await = Some(req.headers().clone());
                    (
                        [(header::CONTENT_TYPE, "application/json")],
                        r#"{"object":"list","data":[{"id":"m1"}]}"#,
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, fake).await.unwrap() });
        let (_d, e) = fixture(Some((
            "openai",
            Destination::forward(
                format!("http://{addr}"),
                Auth::Inject {
                    header: HeaderName::from_static("authorization"),
                    value: "Bearer real".into(),
                },
            ),
        )))
        .await;
        let v = e.get("openai", "v1/models").await.unwrap();
        assert_eq!(v["data"][0]["id"], "m1");
        let h = seen.lock().await.clone().unwrap();
        assert_eq!(h.get("authorization").unwrap(), "Bearer real");
        assert!(matches!(
            e.get("nowhere", "v1/models").await,
            Err(Error::Refused(_))
        ));
        assert!(
            e.telemetry().summary().unwrap().is_empty(),
            "a listing is not a model call"
        );
    }

    #[tokio::test]
    async fn muse_s_catalog_at_another_destination_is_its_model_list() {
        let seen = Arc::new(tokio::sync::Mutex::new(None::<String>));
        let seen2 = seen.clone();
        let fake = Router::new().route(
            "/v1/models",
            any(move |req: Request| {
                let seen = seen2.clone();
                async move {
                    *seen.lock().await = Some(req.uri().to_string());
                    (
                        [(header::CONTENT_TYPE, "application/json")],
                        r#"{"object":"list","data":[{"id":"qwen3.8-27b"}]}"#,
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, fake).await.unwrap() });
        let (_d, e) = fixture(Some((
            "llmao",
            Destination::forward(format!("http://{addr}"), Auth::PassThrough),
        )))
        .await;
        let r = e
            .router()
            .oneshot(
                Request::builder()
                    .uri("/muse-code/models")
                    .method("GET")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(seen.lock().await.clone().unwrap(), "/v1/models");
    }

    #[tokio::test]
    async fn a_path_naming_no_destination_goes_to_the_only_one_whole() {
        // Muse Code asks for its catalog at the origin, not under the base's path
        let seen = Arc::new(tokio::sync::Mutex::new(None::<String>));
        let seen2 = seen.clone();
        let fake = Router::new().route(
            "/muse-code/models",
            any(move |req: Request| {
                let seen = seen2.clone();
                async move {
                    *seen.lock().await = Some(req.uri().to_string());
                    (
                        [(header::CONTENT_TYPE, "application/json")],
                        r#"{"models":[{"id":"muse-spark-1.3"}]}"#,
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, fake).await.unwrap() });
        let (_d, e) = fixture(Some((
            "anthropic",
            Destination::forward(format!("http://{addr}"), Auth::PassThrough).with_origin_paths(),
        )))
        .await;
        let r = e
            .router()
            .oneshot(
                Request::builder()
                    .uri("/muse-code/models")
                    .method("GET")
                    .header("authorization", "Bearer k")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(seen.lock().await.clone().unwrap(), "/muse-code/models");
    }

    #[tokio::test]
    async fn the_fold_applies_on_a_gemini_path_of_a_destination_that_asks_for_it_only() {
        // a fake upstream that keeps every body it is sent
        let seen = Arc::new(tokio::sync::Mutex::new(Vec::<(String, String)>::new()));
        let seen2 = seen.clone();
        let fake = Router::new().fallback(any(move |req: Request| {
            let seen = seen2.clone();
            async move {
                let (parts, body) = req.into_parts();
                let b = to_bytes(body, 1 << 20).await.unwrap();
                seen.lock().await.push((
                    parts.uri.path().to_string(),
                    String::from_utf8_lossy(&b).into_owned(),
                ));
                ([(header::CONTENT_TYPE, "application/json")], "{}")
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, fake).await.unwrap() });
        let agy = serde_json::json!({ "contents": [
            {"role": "model", "parts": [{"functionCall": {"name": "f", "args": {}}}]},
            {"role": "model", "parts": [{"functionResponse": {"name": "f", "response": {"output": "zebra"}}}]}
        ]})
        .to_string();
        let roles = |body: &str| -> Vec<String> {
            let v: serde_json::Value = serde_json::from_str(body).unwrap();
            v["contents"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t["role"].as_str().unwrap().to_string())
                .collect()
        };
        for (folds, path) in [
            (true, "v1beta/models/m:streamGenerateContent"),
            (true, "v1beta/models/m:generateContent"),
            (false, "v1/chat/completions"),
        ] {
            for asks in [true, false] {
                let dest = Destination::forward(format!("http://{addr}"), Auth::PassThrough);
                let dest = if asks {
                    dest.with_function_responses_in_user_turns()
                } else {
                    dest
                };
                let (_d, e) = fixture(Some(("anthropic", dest))).await;
                let r = e
                    .router()
                    .oneshot(
                        Request::builder()
                            .uri(format!("/anthropic/{path}"))
                            .method("POST")
                            .header("content-type", "application/json")
                            .body(Body::from(agy.clone()))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(r.status(), StatusCode::OK, "{path}");
                let (_, body) = seen.lock().await.pop().unwrap();
                let want = if folds && asks {
                    ["model", "user"]
                } else {
                    ["model", "model"]
                };
                assert_eq!(roles(&body), want, "{path}, asked: {asks}");
            }
        }
    }

    #[tokio::test]
    async fn forwards_injects_credentials_and_records_usage() {
        // a fake provider: records the request it saw, answers with a usage block
        let seen = Arc::new(tokio::sync::Mutex::new(None::<(String, HeaderMap, String)>));
        let seen2 = seen.clone();
        let fake = Router::new().route(
            "/v1/messages",
            any(move |req: Request| {
                let seen = seen2.clone();
                async move {
                    let (parts, body) = req.into_parts();
                    let b = to_bytes(body, 1 << 20).await.unwrap();
                    *seen.lock().await = Some((
                        parts.uri.to_string(),
                        parts.headers,
                        String::from_utf8_lossy(&b).into_owned(),
                    ));
                    (
                        [(header::CONTENT_TYPE, "application/json")],
                        r#"{"id":"m","model":"claude-opus-5-5","usage":{"input_tokens":11,"output_tokens":7}}"#,
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, fake).await.unwrap() });

        let (_d, e) = fixture(Some((
            "anthropic",
            Destination::forward(
                format!("http://{addr}"),
                Auth::Inject {
                    header: HeaderName::from_static("x-api-key"),
                    value: "real-key".into(),
                },
            ),
        )))
        .await;
        let r = e
            .router()
            .oneshot(
                Request::builder()
                    .uri("/anthropic/v1/messages?beta=true")
                    .method("POST")
                    .header("content-type", "application/json")
                    .header("x-api-key", "dummy-from-inside")
                    .header("authorization", "Bearer dummy")
                    .header("anthropic-version", "2023-06-01")
                    .body(Body::from(r#"{"model":"claude-opus-5-5","messages":[]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let body = to_bytes(r.into_body(), 1 << 20).await.unwrap();
        assert!(String::from_utf8_lossy(&body).contains("\"usage\""));

        let (uri, headers, body) = seen.lock().await.clone().unwrap();
        assert_eq!(uri, "/v1/messages?beta=true");
        assert_eq!(headers.get("x-api-key").unwrap(), "real-key", "injected");
        assert!(
            headers.get("authorization").is_none(),
            "the inside credential is stripped"
        );
        assert_eq!(
            headers.get("anthropic-version").unwrap(),
            "2023-06-01",
            "provider headers pass"
        );
        assert!(body.contains("claude-opus-5-5"));

        // telemetry is written after the body is drained; give the task a moment
        for _ in 0..50 {
            if !e.telemetry().summary().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let s = e.telemetry().summary().unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].destination, "anthropic");
        assert_eq!(s[0].model, "claude-opus-5-5");
        assert_eq!((s[0].input_tokens, s[0].output_tokens), (11, 7));
    }

    #[tokio::test]
    async fn a_streaming_upstream_is_relayed_and_its_usage_recorded() {
        let fake = Router::new().route(
            "/v1/messages",
            any(|req: Request| async move {
                assert!(
                    req.headers().get("accept-encoding").is_none(),
                    "the egress asks for an uncompressed body"
                );
                let body = concat!(
                    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-sonnet-5-5\",\"usage\":{\"input_tokens\":30,\"output_tokens\":1}}}\n\n",
                    "event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":12}}\n\n",
                    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
                );
                ([(header::CONTENT_TYPE, "text/event-stream")], body)
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, fake).await.unwrap() });
        let (_d, e) = fixture(Some((
            "anthropic",
            Destination::forward(format!("http://{addr}"), Auth::PassThrough),
        )))
        .await;
        let r = e
            .router()
            .oneshot(
                Request::builder()
                    .uri("/anthropic/v1/messages")
                    .method("POST")
                    .header("accept-encoding", "gzip, br")
                    .header("authorization", "Bearer the-harness-own-token")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let body = to_bytes(r.into_body(), 1 << 20).await.unwrap();
        assert!(
            String::from_utf8_lossy(&body).contains("message_stop"),
            "relayed verbatim"
        );
        for _ in 0..50 {
            if !e.telemetry().summary().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let s = e.telemetry().summary().unwrap();
        assert_eq!(s.len(), 1, "usage recorded from the event stream");
        assert_eq!(s[0].model, "claude-sonnet-5-5");
        assert_eq!((s[0].input_tokens, s[0].output_tokens), (30, 12));
    }

    #[test]
    fn allowlist_entries_match_exactly_or_by_suffix() {
        assert!(host_allowed("api.github.com", "api.github.com"));
        assert!(!host_allowed("api.github.com", "evil.api.github.com"));
        assert!(host_allowed(
            ".githubcopilot.com",
            "api.individual.githubcopilot.com"
        ));
        assert!(host_allowed(".githubcopilot.com", "githubcopilot.com"));
        assert!(!host_allowed(".githubcopilot.com", "notgithubcopilot.com"));
    }

    #[test]
    fn an_entry_allows_443_or_the_port_it_names() {
        // (entry, host, port, allowed)
        let cases = [
            ("api.github.com", "api.github.com", 443, true),
            ("api.github.com", "api.github.com", 22, false),
            ("api.github.com", "api.github.com", 80, false),
            (".grok.com", "code.grok.com", 443, true),
            (".grok.com", "code.grok.com", 8443, false),
            ("relay.example:8443", "relay.example", 8443, true),
            ("relay.example:8443", "relay.example", 443, false),
            ("[::1]:4000", "::1", 4000, true),
            ("[::1]:4000", "::1", 443, false),
            ("[::1]", "::1", 443, true),
            // a bare IPv6 address names no port
            ("::1", "::1", 443, true),
            // an entry whose port does not parse allows nothing
            ("relay.example:http", "relay.example", 443, false),
            ("relay.example:", "relay.example", 443, false),
            ("[::1", "::1", 443, false),
        ];
        for (entry, host, port, allowed) in cases {
            assert_eq!(
                entry_allows(entry, host, port),
                allowed,
                "{entry} for {host}:{port}"
            );
        }
        assert_eq!(host_and_port("[::1]:443"), Some(("::1", Some(443))));
        assert_eq!(
            host_and_port("api.github.com"),
            Some(("api.github.com", None))
        );
    }

    #[test]
    fn a_tunnel_may_not_reach_this_machine_its_networks_or_metadata() {
        let denied_here = [
            // loopback and unspecified, this network
            "127.0.0.1",
            "127.255.255.254",
            "0.0.0.0",
            "0.1.2.3",
            "::1",
            "::",
            // private and shared (CGNAT)
            "10.0.0.1",
            "10.255.255.255",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "100.64.0.1",
            "100.127.255.255",
            "fc00::1",
            "fdff:ffff::1",
            // link-local and the metadata services in it
            "169.254.0.1",
            "169.254.169.254",
            "fe80::1",
            "febf::1",
            "fd00:ec2::254",
            "fec0::1",
            // multicast, broadcast, reserved
            "224.0.0.1",
            "239.255.255.250",
            "255.255.255.255",
            "240.0.0.1",
            "ff02::1",
            // documentation and benchmarking
            "192.0.2.1",
            "198.51.100.1",
            "203.0.113.1",
            "198.18.0.1",
            "198.19.255.255",
            "2001:db8::1",
            "3fff::1",
            "3fff:fff::1",
            // IPv4 carried in IPv6
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "::ffff:10.0.0.1",
            "::ffff:100.64.0.1",
            "::ffff:224.0.0.1",
            "::ffff:255.255.255.255",
            "::127.0.0.1",
            "64:ff9b::a9fe:a9fe",
            "64:ff9b::7f00:1",
        ];
        for a in denied_here {
            assert!(denied(a.parse().unwrap()), "{a} is denied");
        }
        let reachable = [
            "1.1.1.1",
            "8.8.8.8",
            "140.82.112.3",
            "9.255.255.255",
            "11.0.0.0",
            "172.15.255.255",
            "172.32.0.0",
            "192.167.255.255",
            "192.169.0.0",
            "100.63.255.255",
            "100.128.0.0",
            "169.253.255.255",
            "169.255.0.0",
            "198.17.255.255",
            "198.20.0.0",
            "223.255.255.255",
            "2606:4700:4700::1111",
            "2001:db9::1",
            "3fff:1000::1",
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
        ];
        for a in reachable {
            assert!(!denied(a.parse().unwrap()), "{a} is reachable");
        }
    }

    #[tokio::test]
    async fn a_name_is_resolved_once_and_its_checked_addresses_are_what_is_dialled() {
        // an address in the request is checked as a name's answer is, without DNS
        let public = tunnel_addresses(".example", "1.1.1.1", 443).await.unwrap();
        assert_eq!(public, vec!["1.1.1.1:443".parse().unwrap()]);
        let refused = tunnel_addresses(".example", "169.254.169.254", 443)
            .await
            .unwrap_err();
        assert_eq!(refused.status(), StatusCode::FORBIDDEN);
        // an entry that is an address is dialled as named, loopback included
        let named = tunnel_addresses("127.0.0.1", "127.0.0.1", 4000)
            .await
            .unwrap();
        assert_eq!(named, vec!["127.0.0.1:4000".parse().unwrap()]);
    }

    #[tokio::test]
    async fn an_upstream_base_with_a_path_keeps_it() {
        // DashScope's compatible endpoint lives under a path; the harness's /v1/...
        // goes after it, not in place of it
        assert_eq!(
            upstream_url(
                "https://dashscope-intl.aliyuncs.com/compatible-mode/",
                "v1/chat/completions",
                &"/dashscope/v1/chat/completions".parse().unwrap(),
                true
            ),
            "https://dashscope-intl.aliyuncs.com/compatible-mode/v1/chat/completions"
        );
    }

    #[tokio::test]
    async fn a_key_in_the_query_is_dropped_when_the_egress_injects() {
        assert_eq!(
            upstream_url(
                "https://g",
                "v1beta/models",
                &"/x?alt=sse&key=inside".parse().unwrap(),
                true
            ),
            "https://g/v1beta/models?alt=sse"
        );
        assert_eq!(
            upstream_url(
                "https://g",
                "v1beta/models",
                &"/x?key=inside".parse().unwrap(),
                true
            ),
            "https://g/v1beta/models"
        );
        assert_eq!(
            upstream_url(
                "https://g",
                "v1beta/models",
                &"/x?key=own".parse().unwrap(),
                false
            ),
            "https://g/v1beta/models?key=own",
            "pass-through keeps what the harness sent"
        );
    }

    type Peers = Arc<std::sync::Mutex<Vec<std::net::SocketAddr>>>;

    /// A plain TCP echo server on loopback standing in for a TLS host, which keeps the
    /// peer address of every connection it accepts.
    async fn echo_upstream() -> (std::net::SocketAddr, Peers) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let peers = Peers::default();
        let seen = peers.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, peer)) = listener.accept().await else {
                    break;
                };
                seen.lock().unwrap().push(peer);
                tokio::spawn(async move {
                    let mut buf = [0u8; 64];
                    if let Ok(n) = tokio::io::AsyncReadExt::read(&mut s, &mut buf).await {
                        let _ = tokio::io::AsyncWriteExt::write_all(&mut s, &buf[..n]).await;
                    }
                });
            }
        });
        (addr, peers)
    }

    /// The connections `upstream` accepted other than one the test opens now. Accepts
    /// are in order, so a dial the egress made before this call is in the list.
    async fn dialled(upstream: std::net::SocketAddr, peers: &Peers) -> Vec<std::net::SocketAddr> {
        let own = tokio::net::TcpStream::connect(upstream).await.unwrap();
        let mine = own.local_addr().unwrap();
        for _ in 0..500 {
            if peers.lock().unwrap().contains(&mine) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let all = peers.lock().unwrap().clone();
        assert!(all.contains(&mine), "the upstream accepts connections");
        all.into_iter().filter(|p| *p != mine).collect()
    }

    /// The egress served on a loopback port.
    async fn serve(e: &Egress) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = e.router();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        addr
    }

    /// `CONNECT authority` by hand, as a client behind `HTTPS_PROXY` sends it: the
    /// response head and body, and the stream, which is the tunnel when it opened.
    async fn connect(
        egress: std::net::SocketAddr,
        authority: &str,
    ) -> (String, tokio::net::TcpStream) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut c = tokio::net::TcpStream::connect(egress).await.unwrap();
        c.write_all(
            format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n").as_bytes(),
        )
        .await
        .unwrap();
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            c.read_exact(&mut byte).await.unwrap();
            head.push(byte[0]);
        }
        let mut answer = String::from_utf8_lossy(&head).into_owned();
        let length = answer
            .lines()
            .find_map(|l| {
                l.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|n| n.trim().parse::<usize>().unwrap())
            })
            .unwrap_or(0);
        let mut body = vec![0u8; length];
        c.read_exact(&mut body).await.unwrap();
        answer.push_str(&String::from_utf8_lossy(&body));
        (answer, c)
    }

    #[tokio::test]
    async fn a_connect_tunnel_opens_only_to_allowed_hosts_within_the_ceiling() {
        let (addr, _) = echo_upstream().await;
        // an entry that is an address and names its port: the launcher's own choice
        let entry = format!("127.0.0.1:{}", addr.port());
        let (_d, e) = fixture(Some(("github", Destination::tunnel([entry])))).await;
        let egress = serve(&e).await;

        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (head, mut c) = connect(egress, &addr.to_string()).await;
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        c.write_all(b"ping").await.unwrap();
        let mut echo = [0u8; 4];
        c.read_exact(&mut echo).await.unwrap();
        assert_eq!(&echo, b"ping", "bytes flow through the tunnel untouched");

        // a host no destination allows
        let (head, _) = connect(egress, "evil.example:443").await;
        assert!(head.starts_with("HTTP/1.1 403"), "{head}");
    }

    #[tokio::test]
    async fn a_tunnel_goes_only_to_the_port_its_entry_names_or_443() {
        let (upstream, peers) = echo_upstream().await;
        // an entry with no port allows 443 and no other
        let (_d, e) = fixture(Some(("github", Destination::tunnel(["127.0.0.1"])))).await;
        let egress = serve(&e).await;
        let (head, _) = connect(egress, &upstream.to_string()).await;
        assert!(head.starts_with("HTTP/1.1 403"), "{head}");
        assert!(
            head.contains(&format!("127.0.0.1:{}", upstream.port())),
            "{head}"
        );
        // an entry that names a port allows that port and no other
        let other = format!("127.0.0.1:{}", upstream.port() ^ 1);
        let (_d, e) = fixture(Some(("github", Destination::tunnel([other])))).await;
        let egress = serve(&e).await;
        let (head, _) = connect(egress, &upstream.to_string()).await;
        assert!(head.starts_with("HTTP/1.1 403"), "{head}");
        assert_eq!(
            dialled(upstream, &peers).await,
            Vec::new(),
            "nothing was dialled"
        );
    }

    #[tokio::test]
    async fn a_tunnel_to_a_name_that_resolves_to_a_denied_address_is_refused_before_dialling() {
        // localhost resolves to loopback on every host, from the hosts file and not DNS
        let (_d, e) = fixture(Some(("github", Destination::tunnel(["localhost"])))).await;
        let egress = serve(&e).await;
        let (head, _) = connect(egress, "localhost:443").await;
        assert!(head.starts_with("HTTP/1.1 403"), "{head}");
        assert!(head.contains("a tunnel may not reach"), "{head}");
        // a name whose entry names the port is resolved and checked all the same
        let (upstream, peers) = echo_upstream().await;
        let entry = format!("localhost:{}", upstream.port());
        let (_d, e) = fixture(Some(("github", Destination::tunnel([entry.clone()])))).await;
        let egress = serve(&e).await;
        let (head, _) = connect(egress, &entry).await;
        assert!(head.starts_with("HTTP/1.1 403"), "{head}");
        assert!(head.contains("a tunnel may not reach"), "{head}");
        assert_eq!(
            dialled(upstream, &peers).await,
            Vec::new(),
            "nothing was dialled"
        );
    }

    #[tokio::test]
    async fn a_label_above_the_ceiling_is_refused_before_anything_leaves() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("m");
        let m = Memory::init(&root, "human:test:me", "dom-me").unwrap();
        m.write(dm_server::WriteRequest {
            classification: Some(Label::Restricted),
            body: "the restricted thing".into(),
            ..dm_server::WriteRequest::default()
        })
        .await
        .unwrap();
        // reading it raises the label to restricted; anthropic's starter ceiling is internal
        m.search("restricted", None, 10).await.unwrap();
        assert_eq!(m.label(), Label::Restricted);
        let mut d = HashMap::new();
        d.insert(
            "anthropic".to_string(),
            Destination::forward("http://127.0.0.1:1", Auth::PassThrough),
        );
        let e = Egress::new(Arc::new(m), d, &dir.path().join("t.db")).unwrap();
        let r = e
            .router()
            .oneshot(
                Request::builder()
                    .uri("/anthropic/v1/messages")
                    .method("POST")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        let body = to_bytes(r.into_body(), 1 << 20).await.unwrap();
        assert!(String::from_utf8_lossy(&body).contains("fresh session"));
    }

    /// A root whose `fetch` is hosted at `url`, with the tool's ceiling at `ceiling`.
    fn with_hosted_fetch(root: &FsPath, url: &str, ceiling: Label) -> Memory {
        Memory::init(root, "human:test:me", "dom-me").unwrap();
        let path = root.join(dm_server::GRANTS_FILE);
        let mut g = dm_policy::Grants::load(&path).unwrap();
        g.taps.insert(
            "fetch".into(),
            dm_policy::TapGrant {
                domain: "dom-me".into(),
                tool: true,
                ceiling,
                hosts: vec!["127.0.0.1".into()],
                hosted: Some(url.into()),
                ..dm_policy::TapGrant::default()
            },
        );
        std::fs::write(&path, g.render().unwrap()).unwrap();
        Memory::open(root).unwrap()
    }

    /// A tool server on loopback that answers with what it was sent, the bearer it saw
    /// and the session header, and names a session of its own.
    async fn echoing_tool_server() -> std::net::SocketAddr {
        // a tool server that answers with what it was sent and the bearer it saw
        let fake = Router::new().route(
            "/mcp",
            any(|req: Request| async move {
                let (parts, body) = req.into_parts();
                let bearer = parts
                    .headers
                    .get(header::AUTHORIZATION)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("-")
                    .to_string();
                let session = parts
                    .headers
                    .get("mcp-session-id")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("-")
                    .to_string();
                let b = to_bytes(body, 1 << 20).await.unwrap();
                (
                    [
                        (header::CONTENT_TYPE, "application/json".to_string()),
                        (HeaderName::from_static("mcp-session-id"), "s-1".to_string()),
                    ],
                    format!(
                        "{{\"saw\":{:?},\"bearer\":{bearer:?},\"session\":{session:?}}}",
                        String::from_utf8_lossy(&b)
                    ),
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, fake).await.unwrap() });
        addr
    }

    #[tokio::test]
    async fn a_tool_server_is_called_by_the_launcher_with_its_credential_and_recorded() {
        let addr = echoing_tool_server().await;
        let dir = tempfile::tempdir().unwrap();
        let url = format!("http://{addr}/mcp");
        let m = Arc::new(with_hosted_fetch(
            &dir.path().join("m"),
            &url,
            Label::Public,
        ));
        let mut tools = HashMap::new();
        tools.insert(
            "fetch".to_string(),
            Destination::forward(
                format!("http://{addr}"),
                Auth::Inject {
                    header: header::AUTHORIZATION,
                    value: "Bearer ts-secret".into(),
                },
            ),
        );
        let e =
            Egress::with_tool_servers(m.clone(), HashMap::new(), tools, &dir.path().join("t.db"))
                .unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("mcp-session-id", HeaderValue::from_static("s-0"));
        // a credential the caller sends is stripped; the server's own goes on
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer mine"),
        );
        let r = e
            .post("fetch", "/mcp", headers, Bytes::from_static(b"{\"id\":1}"))
            .await
            .unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.headers["mcp-session-id"], "s-1");
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(v["saw"], "{\"id\":1}");
        assert_eq!(v["bearer"], "Bearer ts-secret");
        assert_eq!(v["session"], "s-0");
        // recorded as a call to the tool, with no tokens and a latency
        let rows = e.telemetry().summary().unwrap();
        let row = rows.iter().find(|t| t.destination == "fetch").unwrap();
        assert_eq!(row.model, TOOL_MODEL);
        assert_eq!((row.calls, row.input_tokens), (1, 0));
        // a tool this egress has no server for, and one the harness asks for by path
        assert!(matches!(
            e.post("search", "/mcp", HeaderMap::new(), Bytes::new())
                .await,
            Err(Error::Refused(_))
        ));
        let r = e
            .router()
            .oneshot(
                Request::builder()
                    .uri("/fetch/mcp")
                    .method("POST")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_tool_server_is_held_to_the_tool_s_ceiling_and_must_be_a_hosted_tool() {
        let dir = tempfile::tempdir().unwrap();
        let m = with_hosted_fetch(
            &dir.path().join("m"),
            "http://127.0.0.1:1/mcp",
            Label::Public,
        );
        m.write(dm_server::WriteRequest {
            classification: Some(Label::Internal),
            body: "the internal thing".into(),
            ..dm_server::WriteRequest::default()
        })
        .await
        .unwrap();
        m.search("internal", None, 10).await.unwrap();
        assert_eq!(m.label(), Label::Internal);
        let server = || {
            let mut tools = HashMap::new();
            tools.insert(
                "fetch".to_string(),
                Destination::forward("http://127.0.0.1:1", Auth::PassThrough),
            );
            tools
        };
        let e = Egress::with_tool_servers(
            Arc::new(m),
            HashMap::new(),
            server(),
            &dir.path().join("t.db"),
        )
        .unwrap();
        let err = e
            .post("fetch", "/mcp", HeaderMap::new(), Bytes::new())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("accepts at most public"), "{err}");
        // and nothing was recorded, since nothing left
        assert!(e.telemetry().summary().unwrap().is_empty());
        // a tool server for a tool the grants do not host is refused at build time
        let plain =
            Arc::new(Memory::init(&dir.path().join("p"), "human:test:me", "dom-me").unwrap());
        assert!(matches!(
            Egress::with_tool_servers(plain, HashMap::new(), server(), &dir.path().join("t2.db")),
            Err(Error::NotHosted(_))
        ));
    }
}
