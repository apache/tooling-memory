//! The launcher's side of transcripts as sources (the paper, Appendix B).
//!
//! At the end of a session, when the launcher reads the harness's transcript to file
//! the hooks' notes or to mine it, it hands the transcript to the record, which keeps it
//! in the session's domain beside the entries: never indexed, never in T0, reached only
//! by a citation by turn from an entry drawn from it. The commands here show one by
//! citation, sweep the expired, and erase one early.

use crate::{hooks, json, mine, Fallible, ReasonArg};
use dm_server::transcripts::{Keep, Kept, Swept};
use dm_server::Memory;
use std::path::Path;

/// `amem transcript`.
#[derive(clap::Subcommand)]
pub enum TranscriptOp {
    /// The transcripts the record keeps, turns left out: whose, how many turns, when
    /// each runs out, and whether it is live, swept or erased.
    List,
    /// One transcript's header and the turns entries cite; with --turn, that turn, served
    /// only when an entry this session may read cites it, raising the label as a get
    /// does.
    Show {
        /// The id a citation names: `transcript:<session>#<turn>`.
        session: String,
        /// The turn, from one.
        #[arg(long)]
        turn: Option<usize>,
    },
    /// Sweep the transcripts whose domain's clock has run out: turns live entries cite
    /// are kept as excerpts of those entries, the rest is dropped, and the drop is a
    /// line in state/transcripts.jsonl.
    Sweep,
    /// Erase a transcript before its clock: its turns and their excerpts leave the
    /// record, a stub and a receipt remain, and the entries citing it are flagged for
    /// review. Irreversible; the owner only.
    Erase {
        session: String,
        /// Why, as a class: the receipt keeps it.
        #[arg(long, value_enum)]
        reason: ReasonArg,
    },
}

/// Run `amem transcript`.
///
/// # Errors
///
/// What the memory refuses, or a store error.
pub async fn cmd(m: &Memory, op: TranscriptOp) -> Fallible {
    match op {
        TranscriptOp::List => json(&m.transcripts().await?),
        TranscriptOp::Show {
            session,
            turn: Some(n),
        } => json(&m.transcript_turn(&session, n).await?),
        TranscriptOp::Show {
            session,
            turn: None,
        } => json(&m.transcript(&session).await?),
        TranscriptOp::Sweep => json(&sweep(m).await?),
        TranscriptOp::Erase { session, reason } => {
            json(&m.erase_transcript(&session, reason.into()).await?)
        }
    }
}

/// Sweep what has expired, now, and say what went on standard error.
///
/// # Errors
///
/// A store error.
pub async fn sweep(m: &Memory) -> Result<Vec<Swept>, Box<dyn std::error::Error>> {
    let now = time::OffsetDateTime::now_utc();
    let swept = m.sweep_transcripts(now).await?;
    // decision contexts run on the same clock, and go whole: no entry cites them
    for session in m.sweep_contexts(now).await? {
        eprintln!("memory: decision contexts of {session} expired and were dropped");
    }
    // and so do the sources tools brought in, except what a live entry cites
    for id in m.sweep_sources(now).await? {
        eprintln!("memory: source {id} expired, uncited, and was dropped");
    }
    for s in &swept {
        eprintln!(
            "memory: transcript {} expired: {} turns kept as {} excerpts, {} dropped",
            s.session,
            s.kept.len(),
            s.excerpts,
            s.dropped.len()
        );
    }
    Ok(swept)
}

/// Keep the transcript at `path` in the record, as the harness `harness` left it under
/// its private home `home`, in `domain` or the session's only domain.
///
/// # Errors
///
/// When the file cannot be read or the record refuses it.
pub async fn keep_file(
    memory: &Memory,
    harness: &str,
    domain: Option<&str>,
    home: Option<&Path>,
    path: &Path,
) -> Result<Kept, Box<dyn std::error::Error>> {
    let raw = std::fs::read_to_string(hooks::transcript_file(path))?;
    let origin = home
        .and_then(|h| path.strip_prefix(h).ok())
        .unwrap_or(path)
        .display()
        .to_string();
    Ok(memory
        .keep_transcript(Keep {
            domain: domain.map(str::to_string),
            harness: harness.to_string(),
            origin,
            turns: hooks::transcript_turns(&raw),
            raw,
        })
        .await?)
}

/// Keep the newest transcript the harness left under its private home, if it left
/// one. A failure is said and does not stop the session's end: its notes are filed
/// all the same, citing nothing.
pub async fn keep_newest(
    memory: &Memory,
    harness: &str,
    domain: Option<&str>,
    home: &Path,
) -> Option<Kept> {
    let path = mine::newest_transcript(home)?;
    match keep_file(memory, harness, domain, Some(home), &path).await {
        Ok(k) => {
            eprintln!(
                "memory: transcript kept as source {} in {}: {} turns, {}, until {}",
                k.session,
                k.domain,
                k.turns,
                k.classification,
                k.retain_until.date()
            );
            Some(k)
        }
        Err(e) => {
            eprintln!(
                "memory: the transcript at {} was not kept: {e}",
                path.display()
            );
            None
        }
    }
}

/// Take out of the private home `home` each transcript this session's record now holds:
/// the record is where a transcript lives from here, on its clock and under erasure, and
/// a copy left in the home would outlive both. Only a regular file inside the home goes,
/// named by an origin the record keeps; anything else is left where it is. Returns what
/// was removed.
pub async fn remove_kept(memory: &Memory, home: &Path) -> Vec<std::path::PathBuf> {
    let origins = match memory.kept_origins().await {
        Ok(o) => o,
        Err(e) => {
            eprintln!("memory: the harness's transcripts were left in its home: {e}");
            return Vec::new();
        }
    };
    let Ok(home) = home.canonicalize() else {
        return Vec::new();
    };
    let mut removed = Vec::new();
    for origin in origins {
        let Some(path) = inside(&home, &origin) else {
            continue;
        };
        if std::fs::remove_file(&path).is_ok() {
            eprintln!("memory: the harness's copy of {origin} removed; the record keeps it");
            removed.push(path);
        }
    }
    removed
}

/// `origin` as a regular file under `home`, which is canonical: a relative path of plain
/// names, no link on the way or at the end.
fn inside(home: &Path, origin: &str) -> Option<std::path::PathBuf> {
    let rel = Path::new(origin);
    let plain = rel
        .components()
        .all(|c| matches!(c, std::path::Component::Normal(_)));
    if !plain || rel.as_os_str().is_empty() {
        return None;
    }
    let path = home.join(rel);
    let meta = std::fs::symlink_metadata(&path).ok()?;
    let real = path.canonicalize().ok()?;
    (meta.file_type().is_file() && real == path && real.starts_with(home)).then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_newest_transcript_is_kept_by_its_place_under_the_home() {
        let dir = tempfile::tempdir().unwrap();
        let m = Memory::init(&dir.path().join("root"), "human:test:me", "dom-me").unwrap();
        let home = dir.path().join("home");
        assert!(keep_newest(&m, "claude", None, &home).await.is_none());
        let p = home.join(".claude/projects/p");
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(
            p.join("s.jsonl"),
            "{\"type\":\"user\",\"message\":{\"content\":\"Fix the build.\"}}\n\
             {\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"Fixed.\"}]}}\n",
        )
        .unwrap();
        let k = keep_newest(&m, "claude", None, &home).await.unwrap();
        assert_eq!(k.turns, 2);
        let t = &m.transcripts().await.unwrap()[0];
        assert_eq!(t.origin, ".claude/projects/p/s.jsonl");
        assert_eq!(t.harness, "claude");
        let again = keep_newest(&m, "claude", None, &home).await.unwrap();
        assert_eq!(
            again.session, k.session,
            "the same place, the same transcript"
        );
    }

    #[tokio::test]
    async fn only_a_kept_transcript_leaves_the_home() {
        let dir = tempfile::tempdir().unwrap();
        let m = Memory::init(&dir.path().join("root"), "human:test:me", "dom-me").unwrap();
        let home = dir.path().join("home");
        let p = home.join(".claude/projects/p");
        std::fs::create_dir_all(&p).unwrap();
        let line = "{\"type\":\"user\",\"message\":{\"content\":\"Fix the build.\"}}\n";
        std::fs::write(p.join("other.jsonl"), line).unwrap();
        std::fs::write(p.join("s.jsonl"), line).unwrap();
        keep_file(&m, "claude", None, Some(&home), &p.join("s.jsonl"))
            .await
            .unwrap();
        let removed = remove_kept(&m, &home).await;
        assert_eq!(removed.len(), 1, "{removed:?}");
        assert!(!p.join("s.jsonl").exists(), "the kept copy goes");
        assert!(
            p.join("other.jsonl").exists(),
            "a transcript not kept stays"
        );
        assert!(
            m.transcript(&m.transcripts().await.unwrap()[0].session)
                .await
                .is_ok(),
            "the record still holds it"
        );
    }

    #[test]
    fn an_origin_outside_the_home_names_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home").canonicalize().ok();
        assert!(home.is_none(), "not created yet");
        std::fs::create_dir_all(dir.path().join("home/d")).unwrap();
        std::fs::write(dir.path().join("outside.jsonl"), "x").unwrap();
        std::fs::write(dir.path().join("home/d/t.jsonl"), "x").unwrap();
        let home = dir.path().join("home").canonicalize().unwrap();
        assert!(inside(&home, "d/t.jsonl").is_some());
        assert!(inside(&home, "../outside.jsonl").is_none());
        assert!(inside(&home, "").is_none());
        assert!(inside(&home, "d").is_none(), "a directory");
        let abs = dir.path().join("outside.jsonl");
        assert!(inside(&home, abs.to_str().unwrap()).is_none());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.path().join("outside.jsonl"), home.join("l.jsonl"))
                .unwrap();
            assert!(inside(&home, "l.jsonl").is_none(), "a link");
            std::os::unix::fs::symlink(dir.path(), home.join("up")).unwrap();
            assert!(
                inside(&home, "up/outside.jsonl").is_none(),
                "a link on the way"
            );
        }
    }
}
