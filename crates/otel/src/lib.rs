//! OpenTelemetry traces from the binary: a thin facade over the OTLP exporter.
//!
//! The SQLite telemetry tables stay what they are; this is the same events as spans, for
//! a collector. Nothing is installed unless `OTEL_EXPORTER_OTLP_ENDPOINT` or
//! `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` is set, and until [`init`] has installed a
//! provider every [`span`] and [`event`] is one atomic load and nothing else.

use opentelemetry::global::{self, BoxedSpan};
use opentelemetry::trace::{Span as _, Tracer as _};
use opentelemetry::KeyValue;
use opentelemetry_otlp::{Protocol, WithExportConfig as _, WithHttpConfig as _};
use opentelemetry_sdk::trace::span_processor_with_async_runtime::BatchSpanProcessor;
use opentelemetry_sdk::trace::SdkTracerProvider;
use opentelemetry_sdk::Resource;
use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

/// Whether a provider is installed; the fast path checks nothing else.
static ENABLED: AtomicBool = AtomicBool::new(false);

/// The instrumentation scope every span is emitted under.
const SCOPE: &str = "dm-otel";

/// The variables that configure an exporter, in the order the exporter reads them.
const ENDPOINT_VARS: [&str; 2] = [
    "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
    "OTEL_EXPORTER_OTLP_ENDPOINT",
];

/// How long one export may take: a collector that is down must not hold exit.
const EXPORT_TIMEOUT: Duration = Duration::from_secs(10);

/// An attribute's value: text, an integer, or a flag.
#[derive(Clone, Debug, PartialEq)]
pub enum Value<'a> {
    /// Text, borrowed from the caller when it can be.
    Str(Cow<'a, str>),
    /// An integer.
    I64(i64),
    /// A flag.
    Bool(bool),
}

impl<'a> From<&'a str> for Value<'a> {
    fn from(s: &'a str) -> Self {
        Value::Str(Cow::Borrowed(s))
    }
}

impl From<String> for Value<'_> {
    fn from(s: String) -> Self {
        Value::Str(Cow::Owned(s))
    }
}

impl From<i64> for Value<'_> {
    fn from(n: i64) -> Self {
        Value::I64(n)
    }
}

/// Saturates at `i64::MAX`; a count that large is not a count.
impl From<u64> for Value<'_> {
    fn from(n: u64) -> Self {
        Value::I64(i64::try_from(n).unwrap_or(i64::MAX))
    }
}

/// Saturates at `i64::MAX`.
impl From<usize> for Value<'_> {
    fn from(n: usize) -> Self {
        Value::I64(i64::try_from(n).unwrap_or(i64::MAX))
    }
}

impl From<bool> for Value<'_> {
    fn from(b: bool) -> Self {
        Value::Bool(b)
    }
}

impl From<&Value<'_>> for opentelemetry::Value {
    fn from(v: &Value<'_>) -> Self {
        match v {
            Value::Str(s) => opentelemetry::Value::String(s.to_string().into()),
            Value::I64(n) => opentelemetry::Value::I64(*n),
            Value::Bool(b) => opentelemetry::Value::Bool(*b),
        }
    }
}

fn key_values(attrs: &[(&'static str, Value<'_>)]) -> Vec<KeyValue> {
    attrs
        .iter()
        .map(|(k, v)| KeyValue::new(*k, opentelemetry::Value::from(v)))
        .collect()
}

/// A started span. It ends when dropped, or at [`Span::end`].
#[derive(Debug)]
pub struct Span(Option<BoxedSpan>);

impl Span {
    /// Add attributes to a span that is still open.
    pub fn set(&mut self, attrs: &[(&'static str, Value<'_>)]) {
        if let Some(s) = self.0.as_mut() {
            s.set_attributes(key_values(attrs));
        }
    }

    /// End the span now.
    pub fn end(mut self) {
        if let Some(s) = self.0.as_mut() {
            s.end();
        }
    }
}

/// Whether a provider is installed: a caller with attributes that cost something to
/// build asks this first.
#[must_use]
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// A span started now. Nothing is recorded, and `attrs` is not read, until a provider
/// is installed.
#[must_use]
pub fn span(name: &str, attrs: &[(&'static str, Value<'_>)]) -> Span {
    span_since(SystemTime::now(), name, attrs)
}

/// A span that started at `started`: for an operation whose outcome is only known at
/// the end, when it is also known whether it deserves a span at all.
#[must_use]
pub fn span_since(started: SystemTime, name: &str, attrs: &[(&'static str, Value<'_>)]) -> Span {
    if !enabled() {
        return Span(None);
    }
    let tracer = global::tracer(SCOPE);
    let span = tracer
        .span_builder(name.to_string())
        .with_start_time(started)
        .with_attributes(key_values(attrs))
        .start(&tracer);
    Span(Some(span))
}

/// An instantaneous span: started and ended now.
pub fn event(name: &str, attrs: &[(&'static str, Value<'_>)]) {
    span(name, attrs).end();
}

/// The installed provider; dropping it flushes what is queued and shuts the exporter
/// down.
#[derive(Debug)]
pub struct Guard {
    provider: SdkTracerProvider,
    runtime: Option<tokio::runtime::Runtime>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        ENABLED.store(false, Ordering::Relaxed);
        let _ = self.provider.force_flush();
        let _ = self.provider.shutdown();
        if let Some(rt) = self.runtime.take() {
            rt.shutdown_timeout(Duration::from_secs(1));
        }
    }
}

/// Install `provider` as the global one, and turn the facade on. [`init`] does this
/// with an OTLP exporter; a test does it with an in-memory one.
pub fn install(provider: SdkTracerProvider) -> Guard {
    global::set_tracer_provider(provider.clone());
    ENABLED.store(true, Ordering::Relaxed);
    Guard {
        provider,
        runtime: None,
    }
}

/// Whether the environment names a collector.
fn configured() -> bool {
    ENDPOINT_VARS
        .iter()
        .any(|v| std::env::var_os(v).is_some_and(|s| !s.is_empty()))
}

/// Install an OTLP/HTTP exporter when `OTEL_EXPORTER_OTLP_ENDPOINT` or
/// `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` names a collector, with `service.name` set to
/// `service_name` and `service.version` to this build's version. The batch processor
/// runs on a Tokio runtime of its own, one worker thread, so a caller's runtime may come
/// and go; hold the guard until exit, and its drop flushes. `None`, and nothing
/// installed, when neither variable is set, or when the exporter cannot be built, which
/// is said once on stderr.
#[must_use]
pub fn init(service_name: &str) -> Option<Guard> {
    if !configured() {
        return None;
    }
    match build(service_name) {
        Ok(guard) => Some(guard),
        Err(e) => {
            eprintln!("otel: not exporting traces: {e}");
            None
        }
    }
}

fn build(service_name: &str) -> Result<Guard, String> {
    // an https collector wants a TLS stack; the egress installs the same one
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::Client::builder()
        .timeout(EXPORT_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())?;
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary)
        .with_http_client(client)
        .build()
        .map_err(|e| e.to_string())?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_name("otel")
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let resource = Resource::builder()
        .with_service_name(service_name.to_string())
        .with_attribute(KeyValue::new("service.version", env!("CARGO_PKG_VERSION")))
        .build();
    let processor = {
        // the processor's task is spawned where it is built
        let _enter = runtime.enter();
        BatchSpanProcessor::builder(exporter, opentelemetry_sdk::runtime::Tokio).build()
    };
    let provider = SdkTracerProvider::builder()
        .with_span_processor(processor)
        .with_resource(resource)
        .build();
    let mut guard = install(provider);
    guard.runtime = Some(runtime);
    Ok(guard)
}
