//! The projection: the record as a read-only file view beside the working tree, so a
//! harness can pull an entry with the cheapest tool it has, a file read. One directory
//! per granted domain: a summaries page, which is the index as a file, and one file per
//! active entry under the session's filter, which is the body as a file. It is a static
//! render the wrapper refreshes on events; the launcher mounts it read-only and installs
//! the hook that turns an open into a `get`, so the filter, the label and the funnel
//! apply without the agent reporting anything. Rendering here is not a read: the hook's
//! `get` is.

use crate::{Error, Memory};
use dm_core::{digest, Entry};
use dm_index::Hit;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// The summaries page in each domain's directory.
pub const SUMMARIES: &str = "README.md";
/// The directory, beside the domains, where a session's recalls land as files.
pub const RECALLS: &str = "recall";

/// The file an entry is projected as.
#[must_use]
pub fn entry_file(id: &str) -> String {
    format!("{id}.md")
}

/// The entry id a projected path names, if its file name is `<id>.md` with an id of the
/// record's shape; the summaries page and anything else are `None`.
#[must_use]
pub fn id_of(path: &Path) -> Option<&str> {
    let name = path.file_name()?.to_str()?;
    let id = name.strip_suffix(".md")?;
    let shaped = id.starts_with("mem-")
        && id.len() > 4
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    shaped.then_some(id)
}

/// The summaries page: what the index serves before anyone opens a body, one line per
/// entry, newest first, with the file to open beside it.
#[must_use]
pub fn render_summaries(domain: &str, hits: &[Hit]) -> String {
    let mut out = format!(
        "# {domain}: {} entries\n\nThe memory's summaries for this domain, newest first. Each \
         line names the file holding the entry; opening one is a read of the memory and \
         raises this session's label to the entry's. Search is cheaper than reading: use \
         the memory's `search` tool before opening files here.\n\n",
        hits.len()
    );
    for h in hits {
        out.push_str(&format!(
            "- [{title}]({file}) · {kind} · {tier} · {label}: {summary}\n",
            title = h.title.replace(['[', ']'], " "),
            file = entry_file(&h.id),
            kind = h.entry_type,
            tier = h.trust_tier,
            label = h.classification,
            summary = h.summary.lines().next().unwrap_or_default()
        ));
    }
    out
}

/// What one render did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Projected {
    /// Entry files written because they were new or changed.
    pub written: usize,
    /// Entry files removed because the filter no longer lists them.
    pub removed: usize,
    /// Entries listed in all, across the domains.
    pub listed: usize,
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> Error + '_ {
    move |source| Error::Io {
        path: path.display().to_string(),
        source,
    }
}

/// A directory of the projection, made if missing and never a symlink: the projection
/// is written by the launcher, and a link planted where it writes would carry its files
/// elsewhere.
fn own_dir(path: &Path) -> Result<(), Error> {
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Err(Error::Denied(format!(
                "{} is not a directory of the projection's own",
                path.display()
            )));
        }
        return Ok(());
    }
    std::fs::create_dir_all(path).map_err(io(path))
}

impl Memory {
    /// Render every granted domain under `dir`: `<dir>/<domain>/README.md` and one file
    /// per active entry the session's filter lists, as the record holds it now: an entry
    /// the index lists but the record has since demoted or raised above the session's
    /// handling is left out, as `get` would refuse it. A file whose text is already
    /// current is left alone, so a refresh touches only what changed; a file for an
    /// entry no longer listed, erased, demoted or above the handling, is removed. A
    /// symlink anywhere in the way is refused rather than followed.
    ///
    /// # Errors
    ///
    /// An index, store or I/O error, or a path in the projection that is not its own.
    pub async fn project_into(&self, dir: &Path) -> Result<Projected, Error> {
        self.own_below(dir)?;
        let mut done = Projected::default();
        for domain in self.domains() {
            let f = self.filter_for(Some(&domain))?;
            let hits = self.index.lock().await.list(&f, None, usize::MAX / 2)?;
            let dom_dir = dir.join(&domain);
            own_dir(&dom_dir)?;
            let mut keep: BTreeSet<PathBuf> = BTreeSet::new();
            let mut listed = Vec::with_capacity(hits.len());
            for h in &hits {
                let Some(text) = self.store.get_text(&domain, &h.id).await? else {
                    continue;
                };
                let served = Entry::parse(&text).is_ok_and(|e| {
                    e.is_active()
                        && e.front.classification <= f.handling
                        && e.front.id == h.id
                        && e.front.domain == domain
                });
                if !served {
                    continue;
                }
                let path = dom_dir.join(entry_file(&h.id));
                keep.insert(path.clone());
                let current = read_own(&path);
                if current.as_deref().map(digest) != Some(digest(&text)) {
                    write_atomically(&path, &text)?;
                    done.written += 1;
                }
                listed.push(h.clone());
            }
            let summaries = dom_dir.join(SUMMARIES);
            write_atomically(&summaries, &render_summaries(&domain, &listed))?;
            keep.insert(summaries);
            for stale in std::fs::read_dir(&dom_dir).map_err(io(&dom_dir))?.flatten() {
                let path = stale.path();
                let kind = stale.file_type().map_err(io(&path))?;
                if (kind.is_file() || kind.is_symlink()) && !keep.contains(&path) {
                    std::fs::remove_file(&path).map_err(io(&path))?;
                    done.removed += 1;
                }
            }
            done.listed += listed.len();
        }
        Ok(done)
    }
}

impl Memory {
    /// Every component of `dir` below the root, or below the directory's parent when it
    /// is elsewhere, checked and made in turn, so a link higher up is not followed.
    fn own_below(&self, dir: &Path) -> Result<(), Error> {
        let mut step = if dir.starts_with(&self.root) {
            self.root.clone()
        } else {
            dir.parent().map(Path::to_path_buf).unwrap_or_default()
        };
        let below = dir.strip_prefix(&step).unwrap_or(dir).to_path_buf();
        for component in below.components() {
            step.push(component);
            own_dir(&step)?;
        }
        Ok(())
    }

    /// Land a recall as a file under the projection's `recall/` directory, when the
    /// session has a projection, and name it on the recall relative to the projection.
    /// The pack was served to this session already, so the file is read free. A render
    /// that fails is said on standard error and does not fail the recall.
    pub(crate) async fn project_recall(&self, r: &mut crate::recall::Recall) {
        let Some(dir) = &self.projection else {
            return;
        };
        let now = time::OffsetDateTime::now_utc();
        let name = format!(
            "{}-{:09}.md",
            now.format(time::macros::format_description!(
                "[year][month][day]T[hour][minute][second]"
            ))
            .unwrap_or_default(),
            now.nanosecond()
        );
        let landed = self
            .own_below(&dir.join(RECALLS))
            .and_then(|()| write_atomically(&dir.join(RECALLS).join(&name), &render_recall(r)));
        match landed {
            Ok(()) => r.projected = Some(format!("{RECALLS}/{name}")),
            Err(e) => eprintln!("memory: projection at {}: {e}", dir.display()),
        }
    }
}

/// A recall as a page: the question, the brief or the note that synthesis is owed, the
/// citations, and the pack's pieces as they were served.
#[must_use]
pub fn render_recall(r: &crate::recall::Recall) -> String {
    use crate::recall::Synthesis;
    let mut out = format!("# {}\n\n", r.question.trim());
    match &r.synthesis {
        Synthesis::Server { model } => {
            out.push_str(&format!("Brief by {model}.\n\n"));
            if let Some(brief) = &r.brief {
                out.push_str(brief.trim());
                out.push_str("\n\n");
            }
        }
        Synthesis::Owed => out.push_str(
            "Synthesis owed: no model wrote a brief; the pieces below are the pack, at the \
             depth asked for, for the reader to answer from.\n\n",
        ),
    }
    out.push_str(&format!(
        "Cites: {}. Pack: {} pieces at {} depth, {} tokens, {} hop(s).\n\n",
        r.citations.join(", "),
        r.pack.pieces.len(),
        r.pack.depth,
        r.pack.tokens,
        r.hops
    ));
    for p in &r.pack.pieces {
        out.push_str(&format!(
            "## {} ({}, {}, {})\n\n{}\n\n",
            p.title, p.id, p.entry_type, p.classification, p.summary
        ));
        if let Some(body) = &p.body {
            out.push_str(body.trim_end());
            out.push_str("\n\n");
        }
    }
    out
}

/// The text of a projected file, if it is a plain file of the projection's own; a
/// symlink in its place is not read and is replaced.
fn read_own(path: &Path) -> Option<String> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    meta.is_file().then(|| std::fs::read_to_string(path).ok())?
}

/// Write through a sibling and rename, so a reader never sees half a file. The sibling
/// is made fresh: whatever stood under its name, a link included, is removed first,
/// and the rename replaces the target's name rather than writing through it.
fn write_atomically(path: &Path, text: &str) -> Result<(), Error> {
    use std::io::Write;
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    let tmp = path.with_file_name(name);
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .map_err(io(&tmp))?;
    f.write_all(text.as_bytes()).map_err(io(&tmp))?;
    drop(f);
    std::fs::rename(&tmp, path).map_err(io(path))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WriteRequest;
    use dm_core::erasure::Reason;
    use dm_core::Label;

    #[test]
    fn a_projected_path_names_its_entry_and_nothing_else_does() {
        assert_eq!(
            id_of(Path::new("/memory/dom-me/mem-me-abc123.md")),
            Some("mem-me-abc123")
        );
        assert_eq!(id_of(Path::new("/memory/dom-me/README.md")), None);
        assert_eq!(
            id_of(Path::new("/memory/dom-me/mem-me-abc123.md.tmp")),
            None
        );
        assert_eq!(id_of(Path::new("/memory/dom-me/notes.md")), None);
        assert_eq!(id_of(Path::new("/memory/dom-me/mem-.md")), None);
        assert_eq!(id_of(Path::new("mem-me-a/../b.md")), None);
    }

    #[tokio::test]
    async fn the_projection_lists_what_the_filter_lists_and_follows_the_record() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let view = dir.path().join("view");
        let m = Memory::init(&root, "human:test:me", "dom-me")
            .unwrap()
            .without_read_log();
        let a = m
            .write(WriteRequest {
                title: Some("Retry policy".into()),
                body: "Operation R retries with backoff.\n".into(),
                ..WriteRequest::default()
            })
            .await
            .unwrap();
        let secret = m
            .write(WriteRequest {
                title: Some("The key".into()),
                body: "The signing key lives in the vault.\n".into(),
                classification: Some(Label::Restricted),
                ..WriteRequest::default()
            })
            .await
            .unwrap();
        let first = m.project_into(&view).await.unwrap();
        assert_eq!((first.written, first.removed, first.listed), (2, 0, 2));
        let dom = view.join("dom-me");
        let page = std::fs::read_to_string(dom.join(SUMMARIES)).unwrap();
        assert!(
            page.contains(&format!("[Retry policy]({}.md)", a.id)),
            "{page}"
        );
        assert!(page.contains("restricted: The signing key"), "{page}");
        let body = std::fs::read_to_string(dom.join(entry_file(&a.id))).unwrap();
        assert!(body.starts_with("---\n") && body.contains("backoff"));
        // nothing changed: nothing rewritten
        let again = m.project_into(&view).await.unwrap();
        assert_eq!((again.written, again.removed), (0, 0));

        // a session capped at internal does not get the restricted body as a file
        let capped = Memory::open(&root)
            .unwrap()
            .without_read_log()
            .with_handling_cap(Label::Internal);
        let p = capped.project_into(&view).await.unwrap();
        assert_eq!((p.removed, p.listed), (1, 1), "{p:?}");
        assert!(!dom.join(entry_file(&secret.id)).exists());
        assert!(!std::fs::read_to_string(dom.join(SUMMARIES))
            .unwrap()
            .contains("signing key"));

        // erasure removes the file on the next render, and no sibling is left behind
        m.erase(&a.id, Reason::Mistaken).await.unwrap();
        let after = m.project_into(&view).await.unwrap();
        assert_eq!(after.removed, 1, "{after:?}");
        assert!(!dom.join(entry_file(&a.id)).exists());
        let names: Vec<String> = std::fs::read_dir(&dom)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().all(|n| !n.ends_with(".tmp")), "{names:?}");
    }

    #[tokio::test]
    async fn a_session_with_a_projection_keeps_it_current() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let view = dir.path().join("view");
        let m = Memory::init(&root, "human:test:me", "dom-me")
            .unwrap()
            .without_read_log()
            .with_projection(view.clone());
        let w = m
            .write(WriteRequest {
                title: Some("Fresh".into()),
                body: "Written a moment ago.\n".into(),
                ..WriteRequest::default()
            })
            .await
            .unwrap();
        let file = view.join("dom-me").join(entry_file(&w.id));
        assert!(file.is_file(), "a write renders");
        assert!(std::fs::read_to_string(view.join("dom-me").join(SUMMARIES))
            .unwrap()
            .contains("[Fresh]"));
        m.erase(&w.id, Reason::Mistaken).await.unwrap();
        assert!(!file.exists(), "an erasure removes the file at once");
        let out = dir.path().join("out");
        let other = Memory::init(&dir.path().join("other"), "human:test:me", "dom-me")
            .unwrap()
            .without_read_log();
        other
            .write(WriteRequest {
                title: Some("From elsewhere".into()),
                body: "Imported.\n".into(),
                ..WriteRequest::default()
            })
            .await
            .unwrap();
        other
            .export(Some("dom-me"), Label::Internal, &out)
            .await
            .unwrap();
        let im = m.import(&out, Some("dom-me")).await.unwrap();
        assert!(
            view.join("dom-me")
                .join(entry_file(&im.written[0]))
                .is_file(),
            "an import renders"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlink_in_the_projection_is_refused_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let view = dir.path().join("view");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("victim.txt"), "keep me").unwrap();
        let m = Memory::init(&root, "human:test:me", "dom-me")
            .unwrap()
            .without_read_log();
        m.write(WriteRequest {
            body: "An entry.\n".into(),
            ..WriteRequest::default()
        })
        .await
        .unwrap();
        // the domain's directory planted as a link to somewhere else
        std::fs::create_dir_all(&view).unwrap();
        std::os::unix::fs::symlink(&outside, view.join("dom-me")).unwrap();
        assert!(
            matches!(m.project_into(&view).await, Err(Error::Denied(_))),
            "a linked directory is refused"
        );
        assert_eq!(
            std::fs::read_to_string(outside.join("victim.txt")).unwrap(),
            "keep me"
        );
        assert!(
            !outside.join(SUMMARIES).exists(),
            "nothing was written outside"
        );
        // the view itself as a link
        let view2 = dir.path().join("view2");
        std::os::unix::fs::symlink(&outside, &view2).unwrap();
        assert!(matches!(
            m.project_into(&view2).await,
            Err(Error::Denied(_))
        ));
        // a link above the leaf, under the root: state/projection itself
        std::os::unix::fs::symlink(&outside, root.join("state/projection")).unwrap();
        assert!(
            matches!(
                m.project_into(&root.join("state/projection/claude")).await,
                Err(Error::Denied(_))
            ),
            "a linked parent is refused, not walked"
        );
        assert!(
            !outside.join("claude").exists(),
            "nothing was made in the link's target"
        );
        // a sibling planted where the atomic write lands: replaced, never written through
        std::fs::remove_file(view.join("dom-me")).unwrap();
        std::fs::create_dir_all(view.join("dom-me")).unwrap();
        let planted = outside.join("planted.txt");
        std::fs::write(&planted, "mine").unwrap();
        std::os::unix::fs::symlink(&planted, view.join("dom-me").join("README.md.tmp")).unwrap();
        std::os::unix::fs::symlink(&planted, view.join("dom-me").join("stray.md")).unwrap();
        let p = m.project_into(&view).await.unwrap();
        assert_eq!(std::fs::read_to_string(&planted).unwrap(), "mine");
        assert_eq!(
            p.removed, 1,
            "the stray link is removed, the sibling replaced: {p:?}"
        );
        assert!(!view.join("dom-me").join("stray.md").exists());
        assert!(
            std::fs::symlink_metadata(view.join("dom-me").join(SUMMARIES))
                .unwrap()
                .is_file()
        );
    }

    #[tokio::test]
    async fn the_record_s_word_decides_what_is_rendered_not_the_index_s() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let view = dir.path().join("view");
        let m = Memory::init(&root, "human:test:me", "dom-me")
            .unwrap()
            .without_read_log();
        let demoted = m
            .write(WriteRequest {
                body: "Demoted behind the index's back.\n".into(),
                ..WriteRequest::default()
            })
            .await
            .unwrap();
        let raised = m
            .write(WriteRequest {
                title: Some("Raised later".into()),
                body: "secret body B\n".into(),
                ..WriteRequest::default()
            })
            .await
            .unwrap();
        // another process moved the record on and its index step was lost
        m.store.demote("dom-me", &demoted.id).await.unwrap();
        let mut e = m.store.get("dom-me", &raised.id).await.unwrap().unwrap();
        e.front.classification = Label::Restricted;
        m.store.put(&e).await.unwrap();
        let capped = Memory::open(&root)
            .unwrap()
            .without_read_log()
            .with_handling_cap(Label::Internal);
        let p = capped.project_into(&view).await.unwrap();
        assert_eq!((p.written, p.listed), (0, 0), "{p:?}");
        let dom = view.join("dom-me");
        assert!(!dom.join(entry_file(&demoted.id)).exists());
        assert!(!dom.join(entry_file(&raised.id)).exists());
        assert!(!std::fs::read_to_string(dom.join(SUMMARIES))
            .unwrap()
            .contains("Raised later"));
    }

    #[tokio::test]
    async fn a_recall_lands_as_a_file_the_session_can_read_free() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let view = dir.path().join("view");
        let m = Memory::init(&root, "human:test:me", "dom-me")
            .unwrap()
            .without_read_log()
            .with_projection(view.clone());
        m.write(WriteRequest {
            title: Some("Retry policy".into()),
            body: "Operation R retries with exponential backoff.\n".into(),
            ..WriteRequest::default()
        })
        .await
        .unwrap();
        let r = m
            .recall(
                "how does the retry work?",
                None,
                None,
                crate::recall::Budget::default(),
            )
            .await
            .unwrap();
        let rel = r.projected.clone().expect("the recall landed");
        assert!(rel.starts_with("recall/") && rel.ends_with(".md"), "{rel}");
        let text = std::fs::read_to_string(view.join(&rel)).unwrap();
        assert!(text.starts_with("# how does the retry work?"), "{text}");
        assert!(
            text.contains("Synthesis owed") && text.contains("Retry policy"),
            "{text}"
        );
        assert!(
            text.contains("exponential backoff"),
            "the pack's body is in the file"
        );
        let json = crate::recall::to_json(&r);
        assert_eq!(json["projected"], rel);
        // without a projection, nothing lands and nothing is named
        let plain = Memory::open(&root).unwrap().without_read_log();
        let r = plain
            .recall("retry", None, None, crate::recall::Budget::default())
            .await
            .unwrap();
        assert_eq!(r.projected, None);
    }
}
