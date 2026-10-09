//! Canary entries: a small set per domain with known-correct content and summaries,
//! planted on purpose, so that any curation pass that alters them improperly trips an
//! alarm before trust is spent anywhere else. The launcher keeps what it planted, with
//! digests, under the record's state; a check reads each canary back and compares.

use dm_core::{digest, EntryType};
use dm_server::{Memory, WriteRequest};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Where the planted canaries are recorded.
pub const FILE: &str = "state/canaries.json";

/// One planted canary.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Canary {
    pub id: String,
    pub domain: String,
    pub body_digest: String,
    pub summary_digest: String,
}

/// What a check found.
#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub planted: usize,
    pub intact: usize,
    /// Each tripped canary, with what happened to it.
    pub tripped: Vec<(String, String)>,
}

fn path(root: &Path) -> PathBuf {
    root.join(FILE)
}

/// The canaries planted in this record.
///
/// # Errors
///
/// When the file exists and cannot be read.
pub fn planted(root: &Path) -> Result<Vec<Canary>, Box<dyn std::error::Error>> {
    let p = path(root);
    if !p.exists() {
        return Ok(Vec::new());
    }
    Ok(serde_json::from_str(&std::fs::read_to_string(p)?)?)
}

/// The known-correct texts, numbered: each is a plain fact a librarian has no reason to
/// touch, with a summary that already entails it.
fn text(n: usize) -> (String, String, String) {
    (
        format!("Canary {n}: a planted entry"),
        format!(
            "# Canary {n}: a planted entry\n\nThis entry was planted by the launcher as a canary: its body and summary are known and recorded, and a check compares them. It states one fact, which is true by construction: canary {n} was planted with the number {n}. It is not to be merged, re-summarised, split or demoted; a curation pass that changes it has changed something it should not have.\n"
        ),
        format!("Canary {n}, planted by the launcher: a known entry whose only fact is its own number, kept to catch a curation pass that alters what it should not."),
    )
}

/// Plant `count` canaries in a domain and record them. Planting again adds to what is
/// there.
///
/// # Errors
///
/// When the record refuses the write or the state cannot be written.
pub async fn plant(
    memory: &Memory,
    root: &Path,
    domain: Option<&str>,
    count: usize,
) -> Result<Vec<Canary>, Box<dyn std::error::Error>> {
    let domain = memory.resolve_domain(domain)?;
    let mut all = planted(root)?;
    let start = all.len() + 1;
    let mut new = Vec::new();
    for n in start..start + count {
        let (title, body, summary) = text(n);
        let written = memory
            .write(WriteRequest {
                domain: Some(domain.clone()),
                entry_type: EntryType::Fact,
                title: Some(title),
                body: body.clone(),
                source: vec!["canary".into()],
                // a canary is not for review: its decay clock is the check
                review_by: Some(
                    time::OffsetDateTime::now_utc().date() + time::Duration::days(3650),
                ),
                summary: Some(summary.clone()),
                ..WriteRequest::default()
            })
            .await?;
        new.push(Canary {
            id: written.id,
            domain: domain.clone(),
            body_digest: digest(&body),
            summary_digest: digest(&summary),
        });
    }
    all.extend(new.iter().cloned());
    std::fs::create_dir_all(root.join("state"))?;
    std::fs::write(path(root), serde_json::to_string_pretty(&all)?)?;
    Ok(new)
}

/// Read every planted canary back and compare. A canary that cannot be read any more,
/// superseded, demoted or erased, is tripped, as is one whose body or summary differs
/// from what was planted.
///
/// # Errors
///
/// When the state cannot be read.
pub async fn check(memory: &Memory, root: &Path) -> Result<Report, Box<dyn std::error::Error>> {
    let all = planted(root)?;
    let mut report = Report {
        planted: all.len(),
        ..Report::default()
    };
    for c in &all {
        match memory.get(&c.id).await {
            Err(_) => report.tripped.push((
                c.id.clone(),
                "no longer readable as a live entry: superseded, demoted or erased".into(),
            )),
            Ok(e) => {
                let mut what = Vec::new();
                if digest(&e.body) != c.body_digest {
                    what.push("its body changed");
                }
                let summary = e.front.summary.clone().unwrap_or_default();
                if digest(&summary) != c.summary_digest {
                    what.push("its summary changed");
                }
                if what.is_empty() {
                    report.intact += 1;
                } else {
                    report.tripped.push((c.id.clone(), what.join(" and ")));
                }
            }
        }
    }
    Ok(report)
}

/// File a task for each tripped canary, once per canary and reason, for the owner.
///
/// # Errors
///
/// A ledger error.
pub async fn file(
    memory: &Memory,
    report: &Report,
    domain: &str,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let mut ids = Vec::new();
    for (id, what) in &report.tripped {
        let created = memory
            .file_own(dm_ledger::TaskSpec {
                domain: domain.to_string(),
                title: format!("Canary tripped: {id}"),
                body: format!(
                    "The canary `{id}` was altered: {what}. A canary is a planted entry no curation pass should touch. Find which run changed it, from the entries that supersede it and the tasks that were worked, and judge whether that run's other writes are to be trusted; the record keeps every version, so what was changed can be inspected and reverted. This is for the owner, not for a librarian."
                ),
                requirements: vec!["human".into()],
                depends_on: Vec::new(),
                origin: Some("canary:tripped".into()),
                upstream_event: Some(digest(&format!("{id}\n{what}"))[..16].to_string()),
                created_by: String::new(),
                classification: dm_core::Label::Public,
                priority: 10,
            })
            .await?;
        if !created.deduplicated {
            ids.push(created.task.id);
        }
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn planted_canaries_are_intact_until_one_is_altered() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("m");
        let m = Memory::init(&root, "human:test:me", "dom-t").unwrap();
        let new = plant(&m, &root, None, 2).await.unwrap();
        assert_eq!(new.len(), 2);
        assert_eq!(planted(&root).unwrap().len(), 2);
        let r = check(&m, &root).await.unwrap();
        assert_eq!((r.planted, r.intact, r.tripped.len()), (2, 2, 0));
        // a curation pass rewrites one's summary: superseded, so no longer live
        m.write(WriteRequest {
            domain: Some("dom-t".into()),
            entry_type: EntryType::Fact,
            title: Some("Canary 1: a planted entry".into()),
            body: "# Canary 1\n\nRewritten.\n".into(),
            summary: Some("Something else.".into()),
            supersedes: Some(new[0].id.clone()),
            ..WriteRequest::default()
        })
        .await
        .unwrap();
        let r = check(&m, &root).await.unwrap();
        assert_eq!((r.intact, r.tripped.len()), (1, 1));
        assert!(
            r.tripped[0].1.contains("no longer readable"),
            "{:?}",
            r.tripped
        );
        let filed = file(&m, &r, "dom-t").await.unwrap();
        assert_eq!(filed.len(), 1);
        assert!(
            file(&m, &r, "dom-t").await.unwrap().is_empty(),
            "once per canary and reason"
        );
        let t = m.get_task(&filed[0]).await.unwrap();
        assert_eq!(t.spec.requirements, vec!["human".to_string()]);
    }
}
