//! The projection as a live mount: the record as files the memory server itself serves.
//!
//! The projection's first rung (PLAN M4b) renders a session's view of the record to a
//! directory the launcher mounts read-only, and a hook turns the harness's file read into
//! a `get`. The hook sees the harness's own tools and nothing else: a `cat` from a shell
//! reads the rendered body and the server never knows. This rung puts the server in the
//! path. The render stays on disk as the shape of the view and the home of the summaries
//! and the recall pages, and what the sandbox mounts is a FUSE filesystem the launcher
//! serves over it: a listing or the summaries page is read from the render, free, as the
//! hook always let them be; opening an entry file is a `get` on the memory, through the
//! session's filter, raising its label, kept in the read log and on the tape, whatever
//! tool made it; a `get` the memory refuses is an open that fails. The mount is
//! read-only: the way to write is the memory's `write` tool.
//!
//! The paper's "project, never mount" guards against a path around the filter and the
//! telemetry. A mount the server itself serves has none, and keeps the rule's reason.
//!
//! Used optimistically: the launcher tries the mount where the host has FUSE and falls
//! back to the render where it has not, and says which. Linux only: macOS keeps the
//! render and the hooks, and a WSL container would need the device passed through.

use std::path::{Path, PathBuf};
use std::sync::Arc;

#[cfg(target_os = "linux")]
mod linux;

pub use dm_server::projection::{RECALLS, SUMMARIES};
use dm_server::Memory;

/// What can go wrong making a mount.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// This host cannot serve a live mount; the projection is a render here.
    #[error("{0}")]
    Unavailable(String),
    /// The mount itself failed.
    #[error("mount at {at}: {source}")]
    Mount {
        /// The mountpoint.
        at: String,
        /// Why.
        #[source]
        source: std::io::Error,
    },
    /// A directory could not be made or removed.
    #[error("{path}: {source}")]
    Io {
        /// The path.
        path: String,
        /// Why.
        #[source]
        source: std::io::Error,
    },
}

// Its one caller is inside `mount`'s Linux arm, so off Linux this is dead code and
// `-D warnings` makes that an error rather than a warning: the macOS and Windows test
// jobs fail to compile dm-mount at all.
#[cfg(target_os = "linux")]
fn io(path: &Path) -> impl FnOnce(std::io::Error) -> Error + '_ {
    move |source| Error::Io {
        path: path.display().to_string(),
        source,
    }
}

/// Where the launcher mounts each session's projection under the root, one directory
/// per session as `<harness>/<session>/`, beside the renders at
/// `dm_server::projection::PROJECTIONS`.
pub const MOUNTS: &str = "state/mount";

/// What a path in the view is: a directory, a file read free from the render, or an
/// entry whose open is a `get`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The root, a domain, or the recall directory and this session's own under it.
    Dir,
    /// A domain's summaries page, or one of this session's recall pages: what the
    /// render holds, served as it is.
    Free,
    /// An entry file: opening it is a `get` of this id.
    Entry(String),
}

/// A name a directory of the render may hold: one path component of the record's
/// alphabet, never hidden and never a climb.
fn is_name(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('.')
        && s.len() <= 255
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// What `rel`, a path relative to the view's root, is for `session`; `None` for anything
/// the view does not show, whatever the render holds there. Mirrors what the hook lets
/// through: the summaries page and this session's recall pages free, entry files as a
/// `get`, another session's recall pages and anything else not at all.
#[must_use]
pub fn classify(rel: &Path, session: &str) -> Option<Kind> {
    let parts: Option<Vec<&str>> = rel.components().map(|c| c.as_os_str().to_str()).collect();
    let parts = parts?;
    match parts.as_slice() {
        [] => Some(Kind::Dir),
        [recall] if *recall == RECALLS => Some(Kind::Dir),
        [recall, own] if *recall == RECALLS && *own == session && !session.is_empty() => {
            Some(Kind::Dir)
        }
        [recall, own, page]
            if *recall == RECALLS
                && *own == session
                && !session.is_empty()
                && is_name(page)
                && page.ends_with(".md") =>
        {
            Some(Kind::Free)
        }
        [recall, ..] if *recall == RECALLS => None,
        [domain] if is_name(domain) => Some(Kind::Dir),
        [domain, page] if is_name(domain) && *page == SUMMARIES => Some(Kind::Free),
        [domain, file] if is_name(domain) => {
            dm_server::projection::id_of(Path::new(file)).map(|id| Kind::Entry(id.to_string()))
        }
        _ => None,
    }
}

/// Whether this host can serve a live mount: `Ok` to try, or why not. A check of what
/// is here, not of the mount itself, which can still fail and is then a render.
///
/// # Errors
///
/// Why not, in a sentence the launch line can carry.
pub fn availability() -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        linux::availability()
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err("a live mount needs Linux with FUSE; the projection is a render here".to_string())
    }
}

/// A projection served live at a mountpoint. Dropping it unmounts and removes the
/// mountpoint; the render it was served over stays, the launcher's to remove.
pub struct Mounted {
    at: PathBuf,
    #[cfg(target_os = "linux")]
    session: Option<fuser::BackgroundSession>,
    /// Dropped after the mount and its mountpoint are gone.
    #[cfg(target_os = "linux")]
    _owner: linux::Owner,
}

impl Mounted {
    /// The mountpoint.
    #[must_use]
    pub fn at(&self) -> &Path {
        &self.at
    }
}

impl Drop for Mounted {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        drop(self.session.take());
        std::fs::remove_dir(&self.at).ok();
    }
}

impl std::fmt::Debug for Mounted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mounted").field("at", &self.at).finish()
    }
}

/// Serve `memory`'s projection, rendered at `render`, as a live mount at `at`. Called from
/// inside a Tokio runtime, whose handle serves the gets; the filesystem runs on a thread
/// of its own. A dead mount a killed launcher left at `at` is cleared first.
///
/// # Errors
///
/// [`Error::Unavailable`] where the host has no FUSE, [`Error::Mount`] when the mount
/// fails, [`Error::Io`] when the mountpoint cannot be made.
#[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
pub fn mount(memory: Arc<Memory>, render: &Path, at: &Path) -> Result<Mounted, Error> {
    availability().map_err(Error::Unavailable)?;
    #[cfg(target_os = "linux")]
    {
        // before the mountpoint is made, so no sweep removes it as empty meanwhile
        let owner = linux::own(at)?;
        linux::clear_dead(at);
        std::fs::create_dir_all(at).map_err(io(at))?;
        let session = linux::serve(memory, render, at)?;
        Ok(Mounted {
            at: at.to_path_buf(),
            session: Some(session),
            _owner: owner,
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        unreachable!("availability refuses off Linux")
    }
}

/// Clear what killed launchers left under `base`, a harness's mount directory: a dead
/// mount is unmounted and its directory removed, an empty directory removed, and the
/// record of each removed. A mount whose record a live launcher holds is not touched or
/// asked anything, since a stopped launcher would not answer; a link is never followed.
/// Returns the directories cleared.
#[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
pub fn sweep(base: &Path) -> Vec<PathBuf> {
    let mut cleared = Vec::new();
    if is_link(base) {
        return cleared;
    }
    let Ok(entries) = std::fs::read_dir(base) else {
        return cleared;
    };
    for entry in entries.flatten() {
        // the kind as the directory lists it, without a stat of a mountpoint
        if entry.file_type().is_ok_and(|t| t.is_symlink()) {
            continue;
        }
        let path = entry.path();
        #[cfg(target_os = "linux")]
        if let Some(swept) = sweep_recorded(&path) {
            cleared.extend(swept);
            continue;
        }
        if std::fs::remove_dir(&path).is_ok() {
            cleared.push(path);
        }
    }
    cleared
}

/// Sweep `path` by its record: `None` for a mountpoint from before records were kept,
/// which the caller removes when it is empty, as then.
#[cfg(target_os = "linux")]
fn sweep_recorded(path: &Path) -> Option<Option<PathBuf>> {
    let name = path.file_name()?.to_str()?;
    if let Some(at) = name.strip_suffix(".lock") {
        // a record whose launcher died before it made the mountpoint
        let at = path.with_file_name(at);
        if let linux::Held::Gone(lock) = linux::held(&at) {
            let no_mountpoint = std::fs::symlink_metadata(&at)
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound);
            if no_mountpoint {
                std::fs::remove_file(path).ok();
            }
            drop(lock);
        }
        return Some(None);
    }
    match linux::held(path) {
        linux::Held::Live => Some(None),
        linux::Held::Gone(lock) => {
            linux::clear_dead(path);
            let removed = std::fs::remove_dir(path).is_ok();
            std::fs::remove_file(linux::lock_path(path)).ok();
            drop(lock);
            Some(removed.then(|| path.to_path_buf()))
        }
        linux::Held::Unrecorded => {
            linux::clear_dead(path);
            None
        }
    }
}

/// [`sweep`] every harness's mount directory under `root`'s [`MOUNTS`], so a launch of
/// one harness clears what killed launches of another left. Returns the directories
/// cleared.
pub fn sweep_root(root: &Path) -> Vec<PathBuf> {
    let mounts = root.join(MOUNTS);
    if is_link(&mounts) {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(&mounts) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .flat_map(|e| sweep(&e.path()))
        .collect()
}

/// Whether `path` is a symbolic link, which a sweep does not follow.
fn is_link(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_view_shows_what_the_hook_lets_through_and_nothing_else() {
        let entry = Some(Kind::Entry("mem-me-abc1".into()));
        let table: [(&str, Option<Kind>); 16] = [
            ("", Some(Kind::Dir)),
            ("dom-me", Some(Kind::Dir)),
            ("dom-me/README.md", Some(Kind::Free)),
            ("dom-me/mem-me-abc1.md", entry),
            ("recall", Some(Kind::Dir)),
            ("recall/sess-1", Some(Kind::Dir)),
            ("recall/sess-1/2026-10-13T10-00-00Z.md", Some(Kind::Free)),
            // another session's pages are not this one's to read
            ("recall/sess-2", None),
            ("recall/sess-2/page.md", None),
            // a stray file in a domain, a hidden name, a climb, a deeper path
            ("dom-me/notes.txt", None),
            ("dom-me/.hidden", None),
            (".git", None),
            ("../etc", None),
            ("dom-me/sub/mem-me-abc1.md", None),
            ("dom-me/README.md/x", None),
            ("recall/sess-1/../../dom-me/mem-me-abc1.md", None),
        ];
        for (path, want) in table {
            assert_eq!(classify(Path::new(path), "sess-1"), want, "{path}");
        }
        // a session with no id owns no recall pages
        assert_eq!(classify(Path::new("recall/"), ""), Some(Kind::Dir));
        assert_eq!(classify(Path::new("recall/x"), ""), None);
    }

    #[test]
    fn a_sweep_clears_only_what_is_dead_or_empty() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("claude");
        std::fs::create_dir_all(base.join("gone-1")).unwrap();
        std::fs::create_dir_all(base.join("busy")).unwrap();
        std::fs::write(base.join("busy/keep"), "x").unwrap();
        let cleared = sweep(&base);
        assert_eq!(cleared, vec![base.join("gone-1")]);
        assert!(
            base.join("busy/keep").exists(),
            "a directory in use is not touched"
        );
        assert!(
            sweep(&dir.path().join("none")).is_empty(),
            "no base, nothing to do"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_sweep_reaches_every_harness_and_follows_no_link() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let empty_outside = outside.path().join("empty");
        std::fs::create_dir(&empty_outside).unwrap();
        let session_outside = outside.path().join("h/sess-x");
        std::fs::create_dir_all(&session_outside).unwrap();
        let mounts = root.path().join(MOUNTS);
        for harness in ["claude", "codex"] {
            std::fs::create_dir_all(mounts.join(harness).join("sess-gone")).unwrap();
        }
        // links in a harness's directory, and a harness directory that is a link, to
        // directories outside the root that a followed link would clear
        std::os::unix::fs::symlink(&empty_outside, mounts.join("claude/sess-link")).unwrap();
        std::os::unix::fs::symlink(outside.path(), mounts.join("gemini")).unwrap();

        let mut cleared = sweep_root(root.path());
        cleared.sort();

        assert_eq!(
            cleared,
            vec![
                mounts.join("claude/sess-gone"),
                mounts.join("codex/sess-gone")
            ]
        );
        assert!(empty_outside.is_dir(), "a link is not followed");
        assert!(mounts.join("claude/sess-link").is_symlink());
        // a root whose mount directory is a link is not swept at all
        let linked_root = tempfile::tempdir().unwrap();
        std::fs::create_dir(linked_root.path().join("state")).unwrap();
        std::os::unix::fs::symlink(outside.path(), linked_root.path().join(MOUNTS)).unwrap();
        assert!(sweep_root(linked_root.path()).is_empty());
        assert!(empty_outside.is_dir());
        assert!(session_outside.is_dir());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_sweep_keeps_what_a_live_launcher_s_record_holds_and_clears_a_dead_one_s() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("claude");
        std::fs::create_dir_all(&base).unwrap();
        // a live launcher: it holds its record, its mountpoint may not answer
        let live = base.join("sess-live");
        let owner = linux::own(&live).unwrap();
        std::fs::create_dir(&live).unwrap();
        // one about to make its mountpoint: the record is there, the directory not yet
        let coming = linux::own(&base.join("sess-coming")).unwrap();
        // a dead launcher's record and mountpoint, and a record without a mountpoint
        let dead = base.join("sess-dead");
        std::fs::write(linux::lock_path(&dead), "").unwrap();
        std::fs::create_dir(&dead).unwrap();
        let stray = linux::lock_path(&base.join("sess-stray"));
        std::fs::write(&stray, "").unwrap();

        let cleared = sweep(&base);

        assert_eq!(cleared, vec![dead.clone()]);
        assert!(!linux::lock_path(&dead).exists());
        assert!(!stray.exists());
        assert!(live.is_dir(), "a live launcher's mountpoint is kept");
        assert!(linux::lock_path(&live).exists());
        assert!(linux::lock_path(&base.join("sess-coming")).exists());
        drop(owner);
        drop(coming);
        assert!(
            !linux::lock_path(&live).exists(),
            "the record goes with its owner"
        );
    }
}
