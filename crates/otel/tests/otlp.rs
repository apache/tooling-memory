//! The facade end to end: a collector on a loopback port receives what `init` installed,
//! as OTLP/HTTP protobuf.
//!
//! One test, because the endpoint is a process-global environment variable and the
//! provider a process-global tracer.

use axum::body::Bytes;
use axum::routing::post;
use axum::Router;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::any_value;
use opentelemetry_proto::tonic::common::v1::KeyValue;
use opentelemetry_proto::tonic::trace::v1::Span;
use prost::Message as _;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A collector that keeps every body posted to `/v1/traces`, on a thread of its own so
/// the facade's own runtime and the test thread's blocking calls do not share it.
fn receiver(bodies: Arc<Mutex<Vec<Bytes>>>) -> std::net::SocketAddr {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime");
        rt.block_on(async move {
            let app = Router::new().route(
                "/v1/traces",
                post(move |body: Bytes| {
                    let bodies = bodies.clone();
                    async move {
                        bodies.lock().expect("unpoisoned").push(body);
                        ([("content-type", "application/x-protobuf")], Bytes::new())
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("a loopback port");
            tx.send(listener.local_addr().expect("an address"))
                .expect("the test is waiting");
            axum::serve(listener, app).await.expect("served");
        });
    });
    rx.recv().expect("the receiver started")
}

fn attribute<'a>(span: &'a Span, key: &str) -> Option<&'a any_value::Value> {
    span.attributes
        .iter()
        .find(|kv| kv.key == key)
        .and_then(|kv| kv.value.as_ref())
        .and_then(|v| v.value.as_ref())
}

fn string_attribute(attrs: &[KeyValue], key: &str) -> Option<String> {
    attrs
        .iter()
        .find(|kv| kv.key == key)
        .and_then(|kv| kv.value.as_ref())
        .and_then(|v| v.value.as_ref())
        .and_then(|v| match v {
            any_value::Value::StringValue(s) => Some(s.clone()),
            _ => None,
        })
}

/// Every span posted so far, each with its resource's attributes, decoded; waits up to
/// five seconds for `at_least` of them. The guard's drop flushed, and the response was
/// sent before the export resolved, so the wait covers the receiver's own thread
/// handing the body over.
fn collected(bodies: &Mutex<Vec<Bytes>>, at_least: usize) -> Vec<(Vec<KeyValue>, Span)> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let decoded: Vec<(Vec<KeyValue>, Span)> = bodies
            .lock()
            .expect("unpoisoned")
            .iter()
            .map(|b| ExportTraceServiceRequest::decode(b.clone()).expect("OTLP protobuf"))
            .flat_map(|req| req.resource_spans)
            .flat_map(|rs| {
                let resource = rs.resource.map(|r| r.attributes).unwrap_or_default();
                rs.scope_spans
                    .into_iter()
                    .flat_map(|ss| ss.spans)
                    .map(move |s| (resource.clone(), s))
                    .collect::<Vec<_>>()
            })
            .collect();
        if decoded.len() >= at_least || Instant::now() > deadline {
            return decoded;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn spans_reach_a_collector_as_otlp_protobuf() {
    assert!(
        dm_otel::init("unconfigured").is_none(),
        "no endpoint, no provider"
    );
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let addr = receiver(bodies.clone());
    std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", format!("http://{addr}"));

    let guard = dm_otel::init("amem-test").expect("an endpoint, a provider");
    let mut span = dm_otel::span(
        "model.call",
        &[
            ("memory.session", "sess-1".into()),
            ("memory.hits", 3_usize.into()),
        ],
    );
    span.set(&[("memory.cached", true.into())]);
    span.end();
    dm_otel::event(
        "memory.read",
        &[("memory.tool", String::from("search").into())],
    );
    drop(guard);
    assert!(
        dm_otel::init("amem-test").is_some(),
        "init is repeatable after a drop"
    );
    std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT");

    let spans = collected(&bodies, 2);
    let names: Vec<&str> = spans.iter().map(|(_, s)| s.name.as_str()).collect();
    assert!(names.contains(&"model.call"), "names: {names:?}");
    assert!(names.contains(&"memory.read"), "names: {names:?}");

    let (resource, call) = spans
        .iter()
        .find(|(_, s)| s.name == "model.call")
        .expect("the span");
    assert_eq!(
        attribute(call, "memory.session"),
        Some(&any_value::Value::StringValue("sess-1".into()))
    );
    assert_eq!(
        attribute(call, "memory.hits"),
        Some(&any_value::Value::IntValue(3))
    );
    assert_eq!(
        attribute(call, "memory.cached"),
        Some(&any_value::Value::BoolValue(true)),
        "an attribute set after the start"
    );
    assert!(call.end_time_unix_nano >= call.start_time_unix_nano);
    assert_eq!(
        string_attribute(resource, "service.name").as_deref(),
        Some("amem-test")
    );
    assert_eq!(
        string_attribute(resource, "service.version").as_deref(),
        Some(env!("CARGO_PKG_VERSION"))
    );

    let (_, read) = spans
        .iter()
        .find(|(_, s)| s.name == "memory.read")
        .expect("the event");
    assert_eq!(
        attribute(read, "memory.tool"),
        Some(&any_value::Value::StringValue("search".into()))
    );
}
