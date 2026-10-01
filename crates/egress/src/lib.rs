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
//! - nothing else. A path that names no destination is a 404.
//!
//! Credentials for a destination live here, injected on the way out, unless the mode is
//! pass-through, in which case the harness holds its own (a subscription login) and the
//! egress forwards what it sends.

use axum::body::Body;
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
}

struct Inner {
    memory: Arc<Memory>,
    destinations: HashMap<String, Destination>,
    client: reqwest::Client,
    telemetry: Telemetry,
}

/// The egress, ready to be served.
#[derive(Clone)]
pub struct Egress {
    inner: Arc<Inner>,
}

/// Install the TLS crypto provider once. Safe to call repeatedly.
fn install_crypto() {
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
        install_crypto();
        for name in destinations.keys() {
            if memory.grants().ceiling(name).is_none() {
                return Err(Error::NoCeiling(name.clone()));
            }
        }
        let client = reqwest::Client::builder()
            .use_rustls_tls()
            .connect_timeout(std::time::Duration::from_secs(20))
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .build()?;
        Ok(Egress {
            inner: Arc::new(Inner {
                memory,
                destinations,
                client,
                telemetry: Telemetry::open(telemetry_path)?,
            }),
        })
    }

    /// The usage log.
    #[must_use]
    pub fn telemetry(&self) -> &Telemetry {
        &self.inner.telemetry
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
            .with_state(self.clone())
    }

    /// Serve on a unix socket until the task is cancelled. Removes a stale socket first.
    ///
    /// # Errors
    ///
    /// When the socket cannot be bound or serving fails.
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
const CREDENTIAL_HEADERS: &[&str] = &["authorization", "x-api-key", "openai-organization"];

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

fn upstream_url(base: &str, rest: &str, uri: &Uri) -> String {
    let mut url = format!("{}/{}", base.trim_end_matches('/'), rest);
    if let Some(q) = uri.query() {
        url.push('?');
        url.push_str(q);
    }
    url
}

/// The destination for a request, or the response that refuses it: unknown name, or a
/// session label above the destination's ceiling.
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
    Ok(dest.clone())
}

/// The upstream request: the harness's request with hop-by-hop headers dropped, the
/// credential injected where the destination says so, and the body streamed through.
fn upstream_request(
    inner: &Inner,
    dest: &Destination,
    rest: &str,
    req: Request,
) -> reqwest::RequestBuilder {
    let (parts, body) = req.into_parts();
    let url = upstream_url(&dest.upstream, rest, &parts.uri);
    let method = reqwest::Method::from_bytes(parts.method.as_str().as_bytes())
        .unwrap_or(reqwest::Method::GET);
    let mut headers = copy_headers(&parts.headers, matches!(dest.auth, Auth::Inject { .. }));
    if let Auth::Inject { header, value } = &dest.auth {
        if let Ok(v) = HeaderValue::from_str(value) {
            headers.insert(header.clone(), v);
        }
    }
    let mut builder = inner.client.request(method.clone(), &url).headers(headers);
    if method != Method::GET && method != Method::HEAD {
        let stream = body
            .into_data_stream()
            .map(|r| r.map_err(|e| std::io::Error::other(e.to_string())));
        builder = builder.body(reqwest::Body::wrap_stream(stream));
    }
    builder
}

async fn proxy(egress: Egress, destination: String, rest: String, req: Request) -> Response {
    let inner = &egress.inner;
    let dest = match admit(inner, &destination) {
        Ok(d) => d,
        Err(refusal) => return *refusal,
    };
    let builder = upstream_request(inner, &dest, &rest, req);
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
    let resp_headers = copy_headers(upstream.headers(), false);
    let content_type = upstream
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    let egress2 = egress.clone();
    let session = inner.memory.session_id().to_string();
    let status_code = status.as_u16();
    let body_stream = upstream
        .bytes_stream()
        .map(|chunk| chunk.map_err(|e| std::io::Error::other(e.to_string())));
    let (tx, rx) = tokio::sync::oneshot::channel::<Option<Usage>>();
    let stream = usage_tail(body_stream, content_type.as_deref(), tx);
    tokio::spawn(async move {
        if let Ok(Some(u)) = rx.await {
            if let Err(e) = egress2
                .inner
                .telemetry
                .record(&session, &destination, status_code, &u)
            {
                eprintln!("egress: telemetry: {e}");
            }
        }
    });

    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    *response.headers_mut() = resp_headers;
    response
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
    use axum::body::to_bytes;
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
            Destination {
                upstream: "http://127.0.0.1:1".into(),
                auth: Auth::PassThrough,
            },
        );
        assert!(matches!(
            Egress::new(m, d, &dir.path().join("t.db")),
            Err(Error::NoCeiling(_))
        ));
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
            Destination {
                upstream: format!("http://{addr}"),
                auth: Auth::Inject {
                    header: HeaderName::from_static("x-api-key"),
                    value: "real-key".into(),
                },
            },
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
            Destination {
                upstream: format!("http://{addr}"),
                auth: Auth::PassThrough,
            },
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
            Destination {
                upstream: "http://127.0.0.1:1".into(),
                auth: Auth::PassThrough,
            },
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
}
