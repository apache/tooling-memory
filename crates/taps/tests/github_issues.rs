//! The GitHub issues tap, end to end: the real component, built from `taps/` for
//! `wasm32-wasip2`, run by the host against a fake of the GitHub API on loopback. What
//! it proves: issues become proposed tasks with their origin, a replay files nothing
//! twice, pull requests are skipped, the credential the grant names reaches the API
//! and never the tap, and a host the grant does not name is refused.

// An integration test is test code; a panic here is the failure being reported.
#![allow(clippy::unwrap_used)]

use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::get;
use axum::{Json, Router};
use dm_core::Label;
use dm_policy::{Grants, TapGrant};
use dm_server::Memory;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// The component, built once per test binary from the taps workspace.
fn component() -> &'static Path {
    static BUILT: OnceLock<PathBuf> = OnceLock::new();
    BUILT.get_or_init(|| {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let manifest = repo.join("taps/Cargo.toml");
        let out = std::process::Command::new(env!("CARGO"))
            .args([
                "build",
                "--manifest-path",
                manifest.to_str().unwrap(),
                "-p",
                "tap-github-issues",
                "--target",
                "wasm32-wasip2",
                "--release",
            ])
            .env_remove("CARGO_TARGET_DIR")
            .output()
            .expect("cargo runs");
        assert!(
            out.status.success(),
            "the tap did not build (is the wasm32-wasip2 target installed?):\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        repo.join("taps/target/wasm32-wasip2/release/tap_github_issues.wasm")
    })
}

#[derive(Clone, Default)]
struct Seen {
    authorization: Arc<Mutex<Vec<String>>>,
    paths: Arc<Mutex<Vec<String>>>,
}

async fn issues(State(seen): State<Seen>, uri: axum::http::Uri, headers: HeaderMap) -> Json<Value> {
    seen.authorization.lock().unwrap().push(
        headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string(),
    );
    seen.paths.lock().unwrap().push(uri.to_string());
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
        },
        {
            "number": 14, "title": "Document the manifest", "state": "open",
            "updated_at": "2026-10-03T09:00:00Z",
            "html_url": "https://github.com/acme/widgets/issues/14",
            "body": null
        }
    ]))
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn issues_become_proposed_tasks_once_with_the_credential_kept_from_the_tap() {
    let (authority, seen) = fake_github().await;
    let dir = tempfile::tempdir().unwrap();
    let (memory, grant) = root_with_tap(dir.path(), vec![authority.clone()]);
    let config = json!({ "repo": "acme/widgets", "api": format!("http://{authority}") });

    let report = dm_taps::run(
        memory.clone(),
        component(),
        &grant,
        Some("test-token"),
        &config.to_string(),
    )
    .await
    .unwrap();
    assert_eq!(report.filed, 2, "{report:?}");
    assert_eq!(report.deduplicated, 0);
    assert!(
        report.note.contains("1 pull requests skipped"),
        "{report:?}"
    );

    let tasks = memory
        .tasks(Some("dom-widgets"), dm_ledger::Status::Proposed)
        .await
        .unwrap();
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
        seen.authorization.lock().unwrap().as_slice(),
        ["Bearer test-token"],
        "the host put the credential on the wire"
    );
    assert!(seen.paths.lock().unwrap()[0].starts_with("/repos/acme/widgets/issues?state=open"));

    // a replay files nothing twice
    let again = dm_taps::run(
        memory.clone(),
        component(),
        &grant,
        Some("test-token"),
        &config.to_string(),
    )
    .await
    .unwrap();
    assert_eq!((again.filed, again.deduplicated), (0, 2), "{again:?}");
    assert_eq!(
        memory
            .tasks(Some("dom-widgets"), dm_ledger::Status::Proposed)
            .await
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_the_grant_does_not_name_is_refused_and_nothing_is_filed() {
    let (authority, seen) = fake_github().await;
    let dir = tempfile::tempdir().unwrap();
    let (memory, grant) = root_with_tap(dir.path(), vec!["api.github.com".into()]);
    let config = json!({ "repo": "acme/widgets", "api": format!("http://{authority}") });
    let err = dm_taps::run(
        memory.clone(),
        component(),
        &grant,
        None,
        &config.to_string(),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, dm_taps::Error::Refused(ref why, _) if why.contains("does not allow")),
        "{err}"
    );
    assert!(
        seen.paths.lock().unwrap().is_empty(),
        "nothing reached the fake"
    );
    assert!(memory
        .tasks(Some("dom-widgets"), dm_ledger::Status::Proposed)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bad_config_is_the_tap_s_error_not_a_trap() {
    let dir = tempfile::tempdir().unwrap();
    let (memory, grant) = root_with_tap(dir.path(), vec![]);
    let err = dm_taps::run(memory, component(), &grant, None, "{\"repo\": \"nope\"}")
        .await
        .unwrap_err();
    assert!(
        matches!(err, dm_taps::Error::Tap(ref m) if m.contains("owner/name")),
        "{err}"
    );
}
