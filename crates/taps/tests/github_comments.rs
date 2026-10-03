//! The GitHub comments tap, end to end: the real component, built from `taps/` for
//! `wasm32-wasip2`, run by the host against a fake of the GitHub API on loopback. What
//! it proves: the handoff of a completed task that came from an issue is posted on that
//! issue once, with the credential the grant names and never in the tap; a handoff above
//! the tap's ceiling is never handed out and never leaves; a task done for another
//! repository is left for its own tap; a post that fails costs only itself and is tried
//! again next run; a replay posts nothing twice.

// An integration test is test code; a panic here is the failure being reported.
#![allow(clippy::unwrap_used)]

mod common;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use dm_core::{EntryType, Label};
use dm_ledger::TaskSpec;
use dm_policy::{Grants, TapGrant};
use dm_server::{Memory, WriteRequest};
use dm_taps::Limits;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

fn component() -> &'static Path {
    static BUILT: OnceLock<PathBuf> = OnceLock::new();
    BUILT.get_or_init(|| common::build("tap-github-comments"))
}

/// What the fake API was sent: the path, the bearer token and the comment body.
#[derive(Clone, Default)]
struct Seen(Arc<Mutex<Vec<(String, String, String)>>>);

impl Seen {
    fn posts(&self) -> Vec<(String, String, String)> {
        self.0.lock().unwrap().clone()
    }
}

async fn comments(
    State(seen): State<Seen>,
    axum::extract::Path((owner, repo, number)): axum::extract::Path<(String, String, u64)>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let text = body["body"].as_str().unwrap_or("").to_string();
    let mut seen = seen.0.lock().unwrap();
    seen.push((
        format!("/repos/{owner}/{repo}/issues/{number}/comments"),
        auth,
        text,
    ));
    if number == 99 {
        return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "down").into_response();
    }
    (
        axum::http::StatusCode::CREATED,
        Json(json!({
            "html_url": format!("https://github.com/{owner}/{repo}/issues/{number}#issuecomment-{}", seen.len())
        })),
    )
        .into_response()
}

async fn fake_github() -> (String, Seen) {
    let seen = Seen::default();
    let app = Router::new()
        .route(
            "/repos/{owner}/{repo}/issues/{number}/comments",
            post(comments),
        )
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let authority = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (authority, seen)
}

/// A root whose grants name the tap at an internal ceiling; the owner's memory and the
/// tap's.
fn root_with_tap(dir: &Path, hosts: Vec<String>) -> (Memory, Arc<Memory>, TapGrant) {
    Memory::init(dir, "human:test:me", "dom-widgets").unwrap();
    let grants_path = dir.join(dm_server::GRANTS_FILE);
    let mut grants = Grants::load(&grants_path).unwrap();
    let grant = TapGrant {
        domain: "dom-widgets".into(),
        hosts,
        ceiling: Label::Internal,
        credential: Some("GH_TOKEN".into()),
    };
    grants.taps.insert("github-comments".into(), grant.clone());
    std::fs::write(&grants_path, grants.render().unwrap()).unwrap();
    let owner = Memory::open(dir).unwrap().without_read_log();
    let tap = Memory::open(dir)
        .unwrap()
        .with_author("tap:github-comments")
        .without_read_log();
    (owner, Arc::new(tap), grant)
}

/// A task from `origin`, claimed and completed by the owner in a session of its own
/// with a handoff saying `text`, both at `label`. Returns the task's id. A session per
/// task, as in life: a claim raises the session's label to the task's, and the handoff
/// a later claim writes would carry it.
async fn done(root: &Path, origin: Option<&str>, label: Label, text: &str) -> String {
    let owner = Memory::open(root).unwrap().without_read_log();
    let task = owner
        .create_task(TaskSpec {
            domain: "dom-widgets".into(),
            title: format!("From {}", origin.unwrap_or("here")),
            body: "Do it.\n".into(),
            requirements: vec![],
            depends_on: vec![],
            origin: origin.map(str::to_string),
            upstream_event: origin.map(|_| "open:2026-10-01T09:00:00Z".to_string()),
            created_by: String::new(),
            classification: label,
            priority: 0,
        })
        .await
        .unwrap()
        .task;
    owner.admit(&task.id).await.unwrap();
    owner
        .claim(&task.id, time::Duration::minutes(5))
        .await
        .unwrap();
    let handoff = owner
        .write(WriteRequest {
            domain: Some("dom-widgets".into()),
            entry_type: EntryType::Handoff,
            title: Some(format!("Handoff for {}", task.id)),
            body: text.into(),
            classification: Some(label),
            ..WriteRequest::default()
        })
        .await
        .unwrap();
    owner.complete(&task.id, &handoff.id).await.unwrap();
    task.id
}

/// Five completed tasks: one from an issue of the repository, one restricted, one from
/// another repository, one filed by hand, and one whose issue the API will refuse.
struct Seeded {
    twelve: String,
    restricted: String,
    elsewhere: String,
    local: String,
    failing: String,
}

async fn seed(root: &Path) -> Seeded {
    Seeded {
        twelve: done(
            root,
            Some("github:acme/widgets#12"),
            Label::Internal,
            "Fixed the import: the fourth step now retries.\n",
        )
        .await,
        restricted: done(
            root,
            Some("github:acme/widgets#13"),
            Label::Restricted,
            "The signing key is kept in the vault, which is restricted.\n",
        )
        .await,
        elsewhere: done(
            root,
            Some("github:other/repo#7"),
            Label::Internal,
            "Done over there.\n",
        )
        .await,
        local: done(root, None, Label::Internal, "Done here.\n").await,
        failing: done(
            root,
            Some("github:acme/widgets#99"),
            Label::Internal,
            "The API is down for this one.\n",
        )
        .await,
    }
}

async fn run(tap: &Arc<Memory>, grant: &TapGrant, config: &str) -> dm_taps::Outcome {
    dm_taps::run(
        tap.clone(),
        component(),
        grant,
        Some("secret-token"),
        config,
        Limits::default(),
    )
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_completed_task_s_handoff_is_posted_on_its_issue_once_under_the_ceiling() {
    let (authority, seen) = fake_github().await;
    let dir = tempfile::tempdir().unwrap();
    let (owner, tap, grant) = root_with_tap(dir.path(), vec![authority.clone()]);
    let s = seed(dir.path()).await;
    let config =
        json!({ "repo": "acme/widgets", "api": format!("http://{authority}") }).to_string();

    let outcome = run(&tap, &grant, &config).await;
    assert_eq!(outcome.posted, 1, "{outcome:?}");
    assert!(outcome.refused.is_empty(), "{outcome:?}");
    assert!(
        outcome.note.contains("1 handoffs posted")
            && outcome.note.contains("1 done elsewhere")
            && outcome.note.contains("1 failed"),
        "{}",
        outcome.note
    );
    let posts = seen.posts();
    assert_eq!(
        posts.len(),
        2,
        "one comment landed, one was tried: {posts:?}"
    );
    let (path, auth, text) = &posts[0];
    assert_eq!(path, "/repos/acme/widgets/issues/12/comments");
    assert_eq!(auth, "Bearer secret-token");
    assert!(
        text.contains("fourth step now retries") && text.contains(&format!("task `{}`", s.twelve)),
        "{text}"
    );
    assert_eq!(posts[1].0, "/repos/acme/widgets/issues/99/comments");
    assert!(
        posts.iter().all(|(_, _, t)| !t.contains("signing key")),
        "a restricted handoff left through the tap: {posts:?}"
    );
    assert_eq!(
        owner
            .get_task(&s.twelve)
            .await
            .unwrap()
            .posted_to
            .as_deref(),
        Some("https://github.com/acme/widgets/issues/12#issuecomment-1")
    );
    for id in [&s.restricted, &s.elsewhere, &s.local, &s.failing] {
        assert_eq!(owner.get_task(id).await.unwrap().posted_to, None, "{id}");
    }

    // a replay: nothing posted twice, the failed one tried again
    let again = run(&tap, &grant, &config).await;
    assert_eq!(again.posted, 0, "{again:?}");
    let posts = seen.posts();
    let to_twelve = posts
        .iter()
        .filter(|(p, _, _)| p.ends_with("/12/comments"))
        .count();
    assert_eq!((to_twelve, posts.len()), (1, 3), "{posts:?}");
}
