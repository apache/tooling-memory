//! The GitHub issues tap, end to end: the real component, built from `taps/` for
//! `wasm32-wasip2`, run by the host against a fake of the GitHub API on loopback. What
//! it proves: issues become proposed tasks with their origin, a replay files nothing
//! twice, pull requests are skipped, a second page is followed, a malformed item costs
//! only itself, the credential the grant names reaches the API and never the tap, never
//! in the clear and never another port, a label above the ceiling stops the tap, a host
//! the grant does not name is refused, and a run ends when its wall clock does.

// An integration test is test code; a panic here is the failure being reported.
#![allow(clippy::unwrap_used)]

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use dm_core::Label;
use dm_policy::{Grants, TapGrant};
use dm_server::Memory;
use dm_taps::Limits;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

mod common;

/// The GitHub issues component, built once per test binary.
fn component() -> &'static Path {
    static BUILT: OnceLock<PathBuf> = OnceLock::new();
    BUILT.get_or_init(|| common::build("tap-github-issues"))
}

#[derive(Clone, Default)]
struct Seen {
    authorization: Arc<Mutex<Vec<String>>>,
    paths: Arc<Mutex<Vec<String>>>,
}

impl Seen {
    fn authorization(&self) -> Vec<String> {
        self.authorization.lock().unwrap().clone()
    }

    fn paths(&self) -> Vec<String> {
        self.paths.lock().unwrap().clone()
    }
}

/// Two pages of issues under a `Link` header, a pull request on the first, a malformed
/// item on the second; the `slow` owner answers after twenty seconds.
async fn issues(
    State(seen): State<Seen>,
    axum::extract::Path((owner, repo)): axum::extract::Path<(String, String)>,
    uri: axum::http::Uri,
    headers: HeaderMap,
) -> Response {
    seen.authorization.lock().unwrap().push(
        headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string(),
    );
    seen.paths.lock().unwrap().push(uri.to_string());
    if owner == "slow" {
        tokio::time::sleep(Duration::from_secs(20)).await;
        return Json(json!([])).into_response();
    }
    if uri.query().is_some_and(|q| q.contains("page=2")) {
        return Json(json!([
            {
                "number": 14, "title": "Document the manifest", "state": "open",
                "updated_at": "2026-10-03T09:00:00Z",
                "html_url": "https://github.com/acme/widgets/issues/14",
                "body": null
            },
            {
                "number": 15, "title": "Malformed: no updated_at", "state": "open",
                "updated_at": null, "html_url": "https://github.com/acme/widgets/issues/15"
            }
        ]))
        .into_response();
    }
    let host = headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("127.0.0.1")
        .to_string();
    let page2 = format!("http://{host}/repos/{owner}/{repo}/issues?state=open&per_page=100&page=2");
    let link = format!("<{page2}>; rel=\"next\", <{page2}>; rel=\"last\"");
    (
        [("link", link)],
        Json(json!([
            {
                "number": 12, "title": "Import fails on step four", "state": "open",
                "updated_at": "2026-10-01T09:00:00Z",
                "html_url": "https://github.com/acme/widgets/issues/12",
                "body": "The fourth step times out on large bundles."
            },
            {
                "number": 13, "title": "Add a retry to the import", "state": "open",
                "updated_at": "2026-10-02T09:00:00Z",
                "html_url": "https://github.com/acme/widgets/pull/13",
                "body": "Retries with backoff.",
                "pull_request": { "url": "https://api.github.com/repos/acme/widgets/pulls/13" }
            }
        ])),
    )
        .into_response()
}

/// A fake GitHub on loopback; returns its authority and what it saw.
async fn fake_github() -> (String, Seen) {
    let seen = Seen::default();
    let app = Router::new()
        .route("/repos/{owner}/{repo}/issues", get(issues))
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let authority = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (authority, seen)
}

/// A root whose grants name the tap, and a memory opened under the tap's author.
fn root_with_tap(dir: &Path, hosts: Vec<String>) -> (Arc<Memory>, TapGrant) {
    Memory::init(dir, "human:test:me", "dom-widgets").unwrap();
    let grants_path = dir.join(dm_server::GRANTS_FILE);
    let mut grants = Grants::load(&grants_path).unwrap();
    let grant = TapGrant {
        domain: "dom-widgets".into(),
        hosts,
        ceiling: Label::Public,
        credential: Some("GH_TOKEN".into()),
    };
    grants.taps.insert("github-issues".into(), grant.clone());
    std::fs::write(&grants_path, grants.render().unwrap()).unwrap();
    let memory = Memory::open(dir)
        .unwrap()
        .with_author("tap:github-issues")
        .without_read_log();
    (Arc::new(memory), grant)
}

fn config_for(authority: &str, repo: &str) -> String {
    json!({ "repo": repo, "api": format!("http://{authority}") }).to_string()
}

async fn run(
    memory: &Arc<Memory>,
    grant: &TapGrant,
    credential: Option<&str>,
    config: &str,
    limits: Limits,
) -> Result<dm_taps::Outcome, dm_taps::Error> {
    dm_taps::run(
        memory.clone(),
        component(),
        grant,
        credential,
        config,
        limits,
    )
    .await
}

async fn proposed(memory: &Memory) -> Vec<dm_ledger::Task> {
    memory
        .tasks(Some("dom-widgets"), dm_ledger::Status::Proposed)
        .await
        .unwrap()
}

/// The two issues as tasks: origin, event, author, body, and the domain's label.
fn check_filed(tasks: &[dm_ledger::Task]) {
    let mut origins: Vec<String> = tasks.iter().filter_map(|t| t.spec.origin.clone()).collect();
    origins.sort();
    assert_eq!(
        origins,
        ["github:acme/widgets#12", "github:acme/widgets#14"]
    );
    let twelve = tasks
        .iter()
        .find(|t| t.spec.origin.as_deref() == Some("github:acme/widgets#12"))
        .unwrap();
    assert_eq!(twelve.spec.title, "#12: Import fails on step four");
    assert!(twelve.spec.body.contains("issues/12"));
    assert!(twelve.spec.body.contains("times out"));
    assert_eq!(
        twelve.spec.upstream_event.as_deref(),
        Some("open:2026-10-01T09:00:00Z")
    );
    assert_eq!(twelve.spec.created_by, "tap:github-issues");
    assert_eq!(
        twelve.spec.classification,
        Label::Internal,
        "a task from upstream takes the domain's default, not public"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn issues_become_proposed_tasks_once_with_the_credential_kept_from_the_tap() {
    let (authority, seen) = fake_github().await;
    let dir = tempfile::tempdir().unwrap();
    let (memory, grant) = root_with_tap(dir.path(), vec![authority.clone()]);
    let config = config_for(&authority, "acme/widgets");

    let report = run(
        &memory,
        &grant,
        Some("test-token"),
        &config,
        Limits::default(),
    )
    .await
    .unwrap();
    assert_eq!(report.filed, 2, "{report:?}");
    assert_eq!(report.deduplicated, 0);
    assert!(
        report.note.contains("2 pages"),
        "the Link header was followed: {report:?}"
    );
    assert!(
        report.note.contains("1 pull requests skipped"),
        "{report:?}"
    );
    assert!(
        report.note.contains("1 items unreadable"),
        "a malformed item costs itself only: {report:?}"
    );
    assert!(report.refused.is_empty());
    check_filed(&proposed(&memory).await);

    assert_eq!(
        seen.authorization(),
        ["Bearer test-token", "Bearer test-token"],
        "the host put the credential on the wire, on both pages"
    );
    let paths = seen.paths();
    assert!(paths[0].starts_with("/repos/acme/widgets/issues?state=open"));
    assert!(paths[1].contains("page=2"));

    // a replay files nothing twice
    let again = run(
        &memory,
        &grant,
        Some("test-token"),
        &config,
        Limits::default(),
    )
    .await
    .unwrap();
    assert_eq!((again.filed, again.deduplicated), (0, 2), "{again:?}");
    assert_eq!(proposed(&memory).await.len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_the_grant_does_not_name_is_refused_and_nothing_is_filed() {
    let (authority, seen) = fake_github().await;
    let dir = tempfile::tempdir().unwrap();
    let (memory, grant) = root_with_tap(dir.path(), vec!["api.github.com".into()]);
    let config = config_for(&authority, "acme/widgets");
    let err = run(&memory, &grant, None, &config, Limits::default())
        .await
        .unwrap_err();
    assert!(
        matches!(err, dm_taps::Error::Refused(ref why, _) if why.contains("does not allow")),
        "{err}"
    );
    assert!(seen.paths().is_empty(), "nothing reached the fake");
    assert!(proposed(&memory).await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_credential_never_goes_in_the_clear_or_to_another_port() {
    let (authority, seen) = fake_github().await;
    // plaintext to a host that is not loopback: refused before anything is sent
    let dir = tempfile::tempdir().unwrap();
    let (memory, grant) = root_with_tap(dir.path(), vec!["api.github.com".into()]);
    let config = json!({ "repo": "acme/widgets", "api": "http://api.github.com" }).to_string();
    let err = run(&memory, &grant, Some("t"), &config, Limits::default())
        .await
        .unwrap_err();
    assert!(
        matches!(err, dm_taps::Error::Refused(ref why, _) if why.contains("https")),
        "{err}"
    );
    // a grant that names a port is that port only
    let dir2 = tempfile::tempdir().unwrap();
    let (memory, grant) = root_with_tap(dir2.path(), vec!["127.0.0.1:1".into()]);
    let config = config_for(&authority, "acme/widgets");
    let err = run(&memory, &grant, Some("t"), &config, Limits::default())
        .await
        .unwrap_err();
    assert!(
        matches!(err, dm_taps::Error::Refused(ref why, _) if why.contains("does not allow")),
        "{err}"
    );
    assert!(seen.paths().is_empty(), "nothing reached the fake");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_label_above_the_ceiling_stops_the_tap_s_outbound_calls() {
    let (authority, seen) = fake_github().await;
    let dir = tempfile::tempdir().unwrap();
    let (memory, grant) = root_with_tap(dir.path(), vec![authority.clone()]);
    // the session reads something internal; the grant's ceiling is public
    memory
        .write(dm_server::WriteRequest {
            body: "An internal note the tap's session has now read.\n".into(),
            ..dm_server::WriteRequest::default()
        })
        .await
        .unwrap();
    memory.search("internal note", None, 5).await.unwrap();
    assert_eq!(memory.label(), Label::Internal);
    let config = config_for(&authority, "acme/widgets");
    let err = run(&memory, &grant, None, &config, Limits::default())
        .await
        .unwrap_err();
    assert!(
        matches!(err, dm_taps::Error::Refused(ref why, _) if why.contains("ceiling")),
        "{err}"
    );
    assert!(seen.paths().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_is_bounded_by_its_wall_clock() {
    let (authority, _seen) = fake_github().await;
    let dir = tempfile::tempdir().unwrap();
    let (memory, grant) = root_with_tap(dir.path(), vec![authority.clone()]);
    let config = config_for(&authority, "slow/repo");
    let started = Instant::now();
    let err = run(
        &memory,
        &grant,
        None,
        &config,
        Limits {
            wall: Duration::from_millis(500),
            ..Limits::default()
        },
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, dm_taps::Error::Runtime(ref m) if m.contains("wall clock")),
        "{err}"
    );
    // compiling the component is inside `run` and takes seconds in a debug build; the
    // fake would hold the tap for twenty, so well under that is the bound holding
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bad_config_is_the_tap_s_error_not_a_trap() {
    let dir = tempfile::tempdir().unwrap();
    let (memory, grant) = root_with_tap(dir.path(), vec![]);
    let err = run(
        &memory,
        &grant,
        None,
        "{\"repo\": \"nope\"}",
        Limits::default(),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, dm_taps::Error::Tap(ref m) if m.contains("owner/name")),
        "{err}"
    );
}
