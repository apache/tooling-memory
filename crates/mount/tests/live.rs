//! The mount, live: a memory's render served over FUSE, read with ordinary file calls.
//! Skipped where the host has no FUSE, saying so.

// An integration test is test code; a panic here is the failure being reported.
#![allow(clippy::unwrap_used)]
#![cfg(target_os = "linux")]

use dm_server::{Memory, WriteRequest};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A root with one domain and one entry, rendered for the session, with a stray file and
/// a link in the render that are the render's and not the view's.
async fn rendered(root: &Path) -> (Arc<Memory>, String, PathBuf) {
    let m = Memory::init(root, "human:test:me", "dom-me").unwrap();
    let w = m
        .write(WriteRequest {
            title: Some("The key".into()),
            body: "The signing key lives in the vault.\n".into(),
            ..WriteRequest::default()
        })
        .await
        .unwrap();
    let render = root.join("state/projection/claude").join(m.session_id());
    let m = Arc::new(m.with_projection(render.clone()));
    m.project_into(&render).await.unwrap();
    std::fs::write(render.join("dom-me/notes.txt"), "stray").unwrap();
    std::os::unix::fs::symlink("/etc/hostname", render.join("dom-me/mem-link-1.md")).unwrap();
    (m, w.id, render)
}

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_open_in_the_mount_is_a_get_the_memory_serves_logs_and_tapes() {
    if let Err(why) = dm_mount::availability() {
        eprintln!("skipping: {why}");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let (m, id, render) = rendered(&root).await;
    let at = root.join("state/mount/claude").join(m.session_id());
    let mounted = dm_mount::mount(m.clone(), &render, &at).unwrap();
    assert_eq!(mounted.at(), at);

    let (at2, id2) = (at.clone(), id.clone());
    let seen = tokio::task::spawn_blocking(move || {
        let top = names(&at2);
        let domain = names(&at2.join("dom-me"));
        let readme = std::fs::read_to_string(at2.join("dom-me/README.md")).unwrap();
        let entry = std::fs::read_to_string(at2.join("dom-me").join(format!("{id2}.md"))).unwrap();
        let stray = std::fs::read(at2.join("dom-me/notes.txt")).map_err(|e| e.kind());
        let write = std::fs::write(at2.join("dom-me/new.md"), "x").map_err(|e| e.kind());
        let edit = std::fs::OpenOptions::new()
            .write(true)
            .open(at2.join("dom-me").join(format!("{id2}.md")))
            .map(|_| ())
            .map_err(|e| e.kind());
        (top, domain, readme, entry, stray, write, edit)
    })
    .await
    .unwrap();
    // the recall directory appears when the first recall lands, as in the render
    assert_eq!(seen.0, vec!["dom-me".to_string()]);
    assert_eq!(
        seen.1,
        vec!["README.md".to_string(), format!("{id}.md")],
        "the stray file and the link are not shown"
    );
    assert!(seen.2.contains("[The key]"), "{}", seen.2);
    assert!(
        seen.3.starts_with("---\n") && seen.3.contains("The signing key lives in the vault."),
        "the entry as the record holds it: {}",
        seen.3
    );
    assert_eq!(seen.4, Err(ErrorKind::NotFound));
    assert!(
        matches!(
            seen.5,
            Err(ErrorKind::ReadOnlyFilesystem | ErrorKind::PermissionDenied)
        ),
        "{:?}",
        seen.5
    );
    assert!(
        matches!(
            seen.6,
            Err(ErrorKind::ReadOnlyFilesystem | ErrorKind::PermissionDenied)
        ),
        "{:?}",
        seen.6
    );

    // the memory saw the one read that was a read: the summaries page is free
    let reads = dm_server::reads::all(&root).unwrap();
    let gets: Vec<_> = reads.iter().filter(|r| r.tool == "get").collect();
    assert_eq!(gets.len(), 1, "{reads:?}");
    assert_eq!(gets[0].ids, vec![id.clone()]);
    assert_eq!(gets[0].session, m.session_id());
    // and it is on the tape as the get it was
    let tape = m.tape(m.session_id()).await.unwrap();
    let taped: Vec<_> = tape.calls.iter().filter(|c| c.tool == "get").collect();
    assert_eq!(taped.len(), 1, "{:?}", tape.calls);
    assert_eq!(taped[0].arguments["id"], id);
    assert_eq!(taped[0].result["via"], "file");

    drop(mounted);
    assert!(!at.exists(), "unmounted, and the mountpoint removed");
    assert!(
        render.join("dom-me").join(format!("{id}.md")).exists(),
        "the render is the launcher's to remove"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_record_s_word_decides_an_open_not_the_render_s() {
    if let Err(why) = dm_mount::availability() {
        eprintln!("skipping: {why}");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let (m, id, render) = rendered(&root).await;
    // another process replaces the entry after the render: the render still lists the
    // old file, and its open is refused because the record has moved on
    let other = Memory::open(&root)
        .unwrap()
        .with_author("agent:other")
        .without_read_log();
    other
        .write(WriteRequest {
            title: Some("The key, moved".into()),
            body: "The signing key moved to the other vault.\n".into(),
            supersedes: Some(id.clone()),
            ..WriteRequest::default()
        })
        .await
        .unwrap();
    assert!(render.join("dom-me").join(format!("{id}.md")).exists());
    let at = root.join("state/mount/claude").join(m.session_id());
    let mounted = dm_mount::mount(m.clone(), &render, &at).unwrap();
    let (at2, id2) = (at.clone(), id.clone());
    let read = tokio::task::spawn_blocking(move || {
        std::fs::read_to_string(at2.join("dom-me").join(format!("{id2}.md"))).map_err(|e| e.kind())
    })
    .await
    .unwrap();
    assert_eq!(
        read,
        Err(ErrorKind::NotFound),
        "a replaced entry is not served"
    );
    let reads = dm_server::reads::all(&root).unwrap();
    assert!(
        reads.iter().all(|r| r.tool != "get"),
        "nothing was served, so nothing was read: {reads:?}"
    );
    let tape = m.tape(m.session_id()).await.unwrap();
    let refused: Vec<_> = tape.calls.iter().filter(|c| c.tool == "get").collect();
    assert_eq!(
        refused.len(),
        1,
        "the refusal is on the tape: {:?}",
        tape.calls
    );
    assert!(refused[0].result["error"].is_string());
    drop(mounted);
}
