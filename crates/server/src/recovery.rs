//! Recovery bundles, the restore, and the restore drill.
//!
//! A backup of the record alone loses the tasks in flight and the writes that the index
//! had not taken. So a backup is a recovery bundle: the record and the ledger at
//! checkpoints that fit together, with the logs, the writes held in quarantine and the
//! configuration. The index is not in it: a restore builds the index again.
//!
//! A recovery bundle is a directory:
//!
//! ```text
//! recovery.yaml                 the manifest: the checkpoints, and a digest for each file
//! record/<domain>/...           the record of each granted domain, with its history
//! log/<domain>.jsonl|.seq|.ledger   the log of each domain and its cursors
//! ledger.jsonl                  each event of the ledger, a line for each
//! quarantine/...                the writes that a gate holds
//! refused/...                   the refusal record of each personal domain
//! drift/windows.jsonl           the drift windows: what an agent wrote in one is not promoted
//! config/...                    the grants, the registry, the prices and the policy
//! ```
//!
//! Five things are kept apart from every bundle, because a restore must not roll them
//! back:
//!
//! - **The grants.** The restore takes the grants in force now. The grants of the bundle
//!   go to `restored-config/` for a person to read, and nothing loads them.
//! - **The erasure journal.** The receipts of the erasures made after the bundle, of
//!   entries and of transcripts. The restore erases each of those again before the root
//!   serves anything.
//! - **The drift windows.** The windows in force now. The restore adds them to those of
//!   the bundle, so a window that was opened after the bundle holds in the restored root.
//! - **The refusal journal.** The refusals of proposals to a personal domain, each as
//!   the key of its origin. The restore removes each of those proposals that the bundle
//!   holds, and the refusal record takes each key.
//! - **The claim epoch.** A number in a file outside the bundle. Each restore takes the
//!   next number and begins that claim epoch on the ledger, so every claim that was made
//!   before the restore is over.
//!
//! A root that a restore is writing holds the marker `state/restoring`, and
//! [`Memory::open`] refuses it. The restore removes the marker as its last step.

use crate::{io, log, Error, Memory, GRANTS_FILE, REGISTRY_FILE};
use dm_core::erasure::{self, Receipt};
use dm_core::recovery::{
    self as format, BundledFile, DomainCheckpoint, LedgerCheckpoint, RecoveryManifest,
};
use dm_core::{Entry, Label, ReadFilter, Status};
use dm_ledger::{Checkpointed, Ledger};
use dm_policy::registry::Registry;
use dm_policy::Grants;
use serde::Serialize;
use std::path::{Path, PathBuf};
use time::OffsetDateTime;

/// Where a root keeps the recovery bundles it makes when the caller names no path.
pub const RECOVERY_DIR: &str = "state/recovery";

/// The marker of a root that a restore is writing.
pub const RESTORING: &str = "state/restoring";

/// The file of a root that holds the last claim epoch a restore took. It is in no bundle.
pub const EPOCH_FILE: &str = "claim-epoch";

/// Where a restore puts the configuration of the bundle. Nothing loads it.
pub const RESTORED_CONFIG: &str = "restored-config";

/// The file of a kept journal that holds the refusals of one domain, beside the receipts
/// of that domain: one origin key on each line.
const REFUSALS_FILE: &str = "refused.txt";
/// The directory of a kept journal that holds the receipts of the transcripts of one
/// domain, below the directory of that domain.
const JOURNAL_TRANSCRIPTS: &str = "transcripts";
/// The file of a kept journal that holds the drift windows.
const JOURNAL_WINDOWS: &str = "drift-windows.jsonl";

const QUARANTINE_DIR: &str = "state/quarantine";
const LEDGER_EVENTS: &str = "ledger.jsonl";
/// The drift windows as a bundle holds them. Without them, a restored root promotes what
/// an agent wrote inside a drift window.
const DRIFT_WINDOWS: &str = "drift/windows.jsonl";

/// The directory of the recovery bundle that a root makes at `stamp`, for a caller that
/// names no path of its own.
#[must_use]
pub fn recovery_dir(root: &Path, stamp: &str) -> PathBuf {
    root.join(RECOVERY_DIR).join(stamp)
}

/// The directories under [`RECOVERY_DIR`] of a root that hold the entry with a body: a
/// recovery bundle that a session made there, or the scratch root of a restore drill.
/// One line for each, for the receipt of an erasure: the erasure does not change a
/// bundle, so a person deletes the directory.
pub(crate) fn copies_under_root(root: &Path, domain: &str, id: &str) -> Vec<String> {
    let Ok(listing) = std::fs::read_dir(root.join(RECOVERY_DIR)) else {
        return Vec::new();
    };
    let below = Path::new("record").join(domain).join(format!("{id}.md"));
    let mut names: Vec<String> = listing
        .filter_map(Result::ok)
        .filter(|item| {
            // a file that does not read or parse counts as a body: it is named too
            match std::fs::read_to_string(item.path().join(&below)) {
                Ok(text) => {
                    Entry::parse(&text).map_or(true, |e| e.front.status != Some(Status::Erased))
                }
                Err(e) => e.kind() != std::io::ErrorKind::NotFound,
            }
        })
        .map(|item| item.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
        .into_iter()
        .map(|name| {
            format!(
                "{RECOVERY_DIR}/{name} under the root holds the entry with its body: \
                 delete that directory"
            )
        })
        .collect()
}

/// Refuse a root that a restore did not finish.
///
/// # Errors
///
/// [`Error::Denied`] when the root holds the marker [`RESTORING`].
pub(crate) fn refuse_unfinished(root: &Path) -> Result<(), Error> {
    if root.join(RESTORING).exists() {
        return Err(Error::Denied(format!(
            "a restore of this root did not finish, so the root serves nothing: {} is there. \
             Restore the bundle again into an empty directory",
            root.join(RESTORING).display()
        )));
    }
    Ok(())
}

fn refuse(why: impl std::fmt::Display) -> Error {
    Error::Denied(format!("recovery bundle: {why}"))
}

/// A name that can be one part of a path and nothing else.
pub(crate) fn valid_name(name: &str) -> Result<&str, Error> {
    let ok = !name.is_empty()
        && name.len() <= 128
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if ok {
        Ok(name)
    } else {
        Err(Error::Denied(format!("{name:?} is not a name")))
    }
}

/// Refuse a directory that holds something.
fn empty_or_absent(dir: &Path, what: &str) -> Result<(), Error> {
    match std::fs::read_dir(dir) {
        Ok(mut listing) => match listing.next() {
            None => Ok(()),
            Some(_) => Err(Error::Denied(format!(
                "{what} {} is not empty",
                dir.display()
            ))),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io(dir)(e)),
    }
}

/// The regular files below `dir`, each as its path below `dir`, sorted. A directory that
/// is not there has none. A symbolic link is refused: a copy would follow it out of the
/// root.
fn files_below(dir: &Path) -> Result<Vec<PathBuf>, Error> {
    let mut out = Vec::new();
    let mut waiting = vec![PathBuf::new()];
    while let Some(below) = waiting.pop() {
        let at = dir.join(&below);
        let listing = match std::fs::read_dir(&at) {
            Ok(listing) => listing,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && below.as_os_str().is_empty() => {
                return Ok(out)
            }
            Err(e) => return Err(io(&at)(e)),
        };
        for item in listing {
            let item = item.map_err(io(&at))?;
            // the type of the item itself: a link is not followed
            let kind = item.file_type().map_err(io(&at))?;
            if kind.is_symlink() {
                return Err(refuse(format!(
                    "{} is a symbolic link, and a bundle holds regular files only",
                    item.path().display()
                )));
            }
            if kind.is_dir() {
                waiting.push(below.join(item.file_name()));
            } else if kind.is_file() {
                out.push(below.join(item.file_name()));
            }
        }
    }
    out.sort();
    Ok(out)
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(io(dir))?;
    }
    std::fs::write(path, bytes).map_err(io(path))
}

/// Copy one regular file, when it is there. A symbolic link is refused.
fn copy_file(from: &Path, to: &Path) -> Result<(), Error> {
    let kind = match std::fs::symlink_metadata(from) {
        Ok(meta) => meta.file_type(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(io(from)(e)),
    };
    if kind.is_symlink() {
        return Err(refuse(format!(
            "{} is a symbolic link, and a bundle holds regular files only",
            from.display()
        )));
    }
    if kind.is_file() {
        write_file(to, &std::fs::read(from).map_err(io(from))?)?;
    }
    Ok(())
}

/// Copy each regular file below `from` to the same place below `to`.
fn copy_tree(from: &Path, to: &Path) -> Result<(), Error> {
    for below in files_below(from)? {
        copy_file(&from.join(&below), &to.join(&below))?;
    }
    Ok(())
}

/// A path below a bundle as the manifest writes it, with `/` between its parts.
fn manifest_path(below: &Path) -> Result<String, Error> {
    let mut parts = Vec::new();
    for part in below.components() {
        let part = part.as_os_str().to_str().ok_or_else(|| {
            refuse(format!(
                "the name of {} is not text",
                below.to_string_lossy()
            ))
        })?;
        parts.push(part);
    }
    Ok(parts.join("/"))
}

/// The number a cursor file holds, or zero when there is none.
fn number_in(file: &Path) -> u64 {
    std::fs::read_to_string(file)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

impl Memory {
    /// The owner in person: a recovery bundle holds each granted domain, the held-out
    /// one too, and the report of a drill names tasks and their holders. A harness
    /// reaches a session that the memory server serves, so an agent may have asked.
    fn owner_makes(&self, what: &str) -> Result<(), Error> {
        if self.author != self.grants.owner {
            return Err(Error::Denied(format!(
                "only the owner {} may make {what}",
                self.grants.owner
            )));
        }
        if self.served() {
            return Err(Error::Denied(format!(
                "a session of the memory server does not make {what}: the owner makes it at the command line, or in `amem serve --person`"
            )));
        }
        Ok(())
    }

    /// Make a recovery bundle of this root in `dest`. For each granted domain the log
    /// and the record are copied while no event can be appended to that log; then the
    /// events of the ledger, the writes in quarantine, the refusal records, the drift
    /// windows and the configuration; then the manifest, with a digest for each file. The index, the
    /// telemetry, the decision log, the read log and the claim epoch are not copied.
    /// Each domain's log gets a `send` event that names the bundle: an erasure made
    /// later names the bundle on its receipt as a copy it cannot reach.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for a session that is not the owner, a `dest` that holds
    /// something, or a symbolic link among the files; the file system, the index or the
    /// ledger.
    pub async fn recovery_bundle(&self, dest: &Path) -> Result<RecoveryManifest, Error> {
        self.owner_makes("a recovery bundle")?;
        self.serving()?;
        self.fresh()?;
        empty_or_absent(dest, "the directory for the bundle")?;
        std::fs::create_dir_all(dest).map_err(io(dest))?;
        let mut domains = Vec::new();
        for domain in self.domains() {
            domains.push(self.bundle_domain(&domain, dest).await?);
        }
        let ledger = self.bundle_ledger(dest).await?;
        self.bundle_held_and_config(dest)?;
        let mut files = Vec::new();
        for below in files_below(dest)? {
            let bytes = std::fs::read(dest.join(&below)).map_err(io(dest))?;
            files.push(BundledFile {
                path: manifest_path(&below)?,
                digest: format::digest_bytes(&bytes),
            });
        }
        let manifest = RecoveryManifest {
            format: format::FORMAT,
            made_at: OffsetDateTime::now_utc(),
            made_by: self.author.clone(),
            domains,
            ledger,
            files,
        };
        write_file(&dest.join(format::FILE), manifest.to_yaml().as_bytes())?;
        self.log_bundle_left(dest, &manifest).await?;
        Ok(manifest)
    }

    /// Copy the log of one domain, its cursors and its record, under the append lock of
    /// the log. The cursor of the index is read first, so it is not above the log.
    async fn bundle_domain(&self, domain: &str, dest: &Path) -> Result<DomainCheckpoint, Error> {
        let index_fed = self.index.lock().await.fed(domain)?;
        let logs = self.root.join(log::DIR);
        let record = Path::new("record").join(log::valid_domain(domain)?);
        log::while_no_append(&self.root, domain, |files, log_seq| {
            for name in files {
                copy_file(&logs.join(name), &dest.join("log").join(name))?;
            }
            copy_tree(&self.root.join(&record), &dest.join(&record))?;
            Ok(DomainCheckpoint {
                domain: domain.to_string(),
                log_seq,
                index_fed: index_fed.min(log_seq),
                ledger_copied: number_in(&dest.join("log").join(format!("{domain}.ledger"))),
            })
        })
    }

    /// Write each event of the ledger into the bundle, a line for each.
    async fn bundle_ledger(&self, dest: &Path) -> Result<LedgerCheckpoint, Error> {
        let (events, epoch) = {
            let l = self.ledger.lock().await;
            (l.checkpoint().await?, l.epoch().await?)
        };
        let mut text = String::new();
        for e in &events {
            let line = serde_json::to_string(e).map_err(|e| refuse(format!("the ledger: {e}")))?;
            text.push_str(&line);
            text.push('\n');
        }
        write_file(&dest.join(LEDGER_EVENTS), text.as_bytes())?;
        Ok(LedgerCheckpoint {
            events: events.len() as u64,
            last_seq: events.iter().map(|e| e.seq).max().unwrap_or(0),
            epoch,
        })
    }

    /// Copy the writes in quarantine, the refusal records, the drift windows and the
    /// configuration.
    fn bundle_held_and_config(&self, dest: &Path) -> Result<(), Error> {
        let root = &self.root;
        copy_tree(&root.join(QUARANTINE_DIR), &dest.join("quarantine"))?;
        copy_file(
            &root.join(crate::drift_windows::FILE),
            &dest.join(DRIFT_WINDOWS),
        )?;
        copy_tree(
            &root.join(crate::registry::REFUSED_DIR),
            &dest.join("refused"),
        )?;
        let config = dest.join("config");
        for name in [GRANTS_FILE, REGISTRY_FILE, "prices.toml"] {
            copy_file(&root.join(name), &config.join(name))?;
        }
        for below in files_below(&root.join("policy"))? {
            if below.extension().is_some_and(|x| x == "rego") {
                copy_file(
                    &root.join("policy").join(&below),
                    &config.join("policy").join(&below),
                )?;
            }
        }
        Ok(())
    }

    /// Say in the log of each granted domain that the bundle left: a survey of the
    /// entries of the domain, and then a send that names the bundle and the digest of
    /// its manifest. Both are at the highest classification: the bundle holds all.
    async fn log_bundle_left(&self, dest: &Path, manifest: &RecoveryManifest) -> Result<(), Error> {
        for domain in self.domains() {
            let ids = self.store.list(&domain).await?;
            self.log_survey(&domain, ids, Label::Restricted)?;
        }
        let name = dest
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let digest = dm_core::digest(&manifest.to_yaml());
        self.log_send_at(None, &format!("recovery:{name}"), digest, Label::Restricted)
            .await
    }

    /// The erasure journal of this root: the receipt of each erasure in each granted
    /// domain. A receipt holds digests and no body. A person keeps a copy of the journal
    /// apart from the bundles ([`write_journal`]), and a restore takes it.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for a session that is not the owner; a store error.
    pub async fn erasure_journal(&self) -> Result<Vec<Receipt>, Error> {
        self.owner_makes("a copy of the erasure journal")?;
        // no check for an outage, here and in the other journals: a restore into
        // another root takes them, and an outage is when the owner needs that restore.
        // A receipt holds digests and no body
        let mut out = Vec::new();
        for domain in self.domains() {
            out.extend(self.store.receipts(&domain).await?);
        }
        Ok(out)
    }

    /// The receipt of each early erasure of a transcript in each granted domain: the
    /// part of the erasure journal for transcripts ([`write_transcript_journal`]). A
    /// restore takes it, and erases each of those transcripts again.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for a session that is not the owner in person; a store error.
    pub async fn transcript_journal(&self) -> Result<Vec<Receipt>, Error> {
        self.owner_makes("a copy of the erasure journal")?;
        self.transcript_receipts().await
    }

    /// The text of the file of the drift windows of this root, when there is one
    /// ([`write_windows`]). A restore takes it, so a window that was opened after a
    /// bundle was made is in the restored root.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for a session that is not the owner in person; the file system.
    pub fn drift_journal(&self) -> Result<Option<String>, Error> {
        self.owner_makes("a copy of the drift windows")?;
        let path = self.root.join(crate::drift_windows::FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => Ok(Some(text)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io(&path)(e)),
        }
    }

    /// The refusal journal of this root: each refusal of a proposal to a personal domain,
    /// as the refusal record of each granted domain holds it. A refusal holds the key of
    /// an origin and no text. A person keeps a copy with the erasure journal
    /// ([`write_refusals`]), and a restore takes it.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for a session that is not the owner.
    pub fn refusal_journal(&self) -> Result<Vec<Refusal>, Error> {
        self.owner_makes("a copy of the refusal journal")?;
        let mut out = Vec::new();
        for domain in self.domains() {
            let keys = self.refused_keys(&domain);
            out.extend(keys.into_iter().map(|key| Refusal {
                domain: domain.clone(),
                key,
            }));
        }
        Ok(out)
    }

    /// Restore `bundle` into `scratch` and check what came back, without a write to
    /// this root's record or ledger. The restore takes the grants and the registry of
    /// this root, its erasure journal, its refusal journal and its claim epoch file. It
    /// always takes a file ledger, so a drill cannot write a ledger on a server. Only the claim epoch file of
    /// this root moves. Each of the paper's five cases is reported as recovered, absent
    /// or failed. A case is recovered only when the bundle held it.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`] for a session that is not the owner or a `scratch` that holds
    /// something; what [`restore`] returns for a bundle it refuses.
    pub async fn drill(&self, bundle: &Path, scratch: &Path) -> Result<Drill, Error> {
        self.owner_makes("a restore drill")?;
        // no check for an outage: a drill reads a bundle and the journals, and writes a
        // scratch root. In an outage the owner must know whether a bundle restores
        self.fresh()?;
        let journal = self.erasure_journal().await?;
        let transcripts = self.transcript_journal().await?;
        let windows = self.drift_journal()?;
        let refusals = self.refusal_journal()?;
        let grants_file = self.root.join(GRANTS_FILE);
        let grants = std::fs::read_to_string(&grants_file).map_err(io(&grants_file))?;
        let registry = std::fs::read_to_string(self.root.join(REGISTRY_FILE)).ok();
        let epoch_file = self.root.join(EPOCH_FILE);
        // read from the bundle before the restore: no case is reported from the
        // restored root alone
        let held = Held::read(bundle, &journal).await?;
        let authority = Authority {
            grants: &grants,
            registry: registry.as_deref(),
            journal: &journal,
            refusals: &refusals,
            transcripts: &transcripts,
            drift_windows: windows.as_deref(),
            epoch_file: &epoch_file,
            file_ledger: true,
        };
        let restored = restore(bundle, scratch, &authority).await?;
        let m = Memory::open(scratch)?;
        let cases = vec![
            unfinished_task(&m, &held).await,
            erasure_after_backup(&m, &held).await,
            awaiting_materialisation(&m, &held).await,
            not_built(
                "cross-domain-dependency",
                "coordination across domains is not built, so no bundle holds this case",
            ),
            uncertain_external_action(&m, &held).await,
        ];
        let passed = cases.iter().all(|c| c.outcome != Outcome::Failed);
        Ok(Drill {
            epoch: restored.epoch,
            cases,
            passed,
        })
    }
}

/// Keep the receipts of an erasure journal in `dir`, as `<dir>/<domain>/<id>.yaml`.
///
/// # Errors
///
/// [`Error::Denied`] for a receipt whose domain or id is not a name; the file system.
pub fn write_journal(dir: &Path, receipts: &[Receipt]) -> Result<(), Error> {
    for r in receipts {
        let file = dir
            .join(valid_name(&r.domain)?)
            .join(format!("{}.yaml", valid_name(&r.id)?));
        write_file(&file, r.to_yaml().as_bytes())?;
    }
    Ok(())
}

/// Read an erasure journal that [`write_journal`] kept. A directory that is not there
/// holds no receipt.
///
/// # Errors
///
/// [`Error::Denied`] for a file whose place does not fit the receipt it holds, or a
/// symbolic link; a receipt that does not parse; the file system.
pub fn read_journal(dir: &Path) -> Result<Vec<Receipt>, Error> {
    let mut out = Vec::new();
    for below in files_below(dir)? {
        let name = below.file_name();
        if name.is_some_and(|n| n == REFUSALS_FILE || n == JOURNAL_WINDOWS)
            || of_a_transcript(&below)
        {
            continue;
        }
        let path = dir.join(&below);
        let text = std::fs::read_to_string(&path).map_err(io(&path))?;
        let receipt = Receipt::from_yaml(&text)?;
        let place = Path::new(valid_name(&receipt.domain)?)
            .join(format!("{}.yaml", valid_name(&receipt.id)?));
        if place != below {
            return Err(Error::Denied(format!(
                "the erasure journal: {} holds the receipt of {} in {}",
                path.display(),
                receipt.id,
                receipt.domain
            )));
        }
        out.push(receipt);
    }
    Ok(out)
}

/// Whether a file of a kept journal is the receipt of a transcript:
/// `<domain>/transcripts/<session>.yaml`.
fn of_a_transcript(below: &Path) -> bool {
    let dir = below.parent().and_then(Path::file_name);
    below.components().count() == 3 && dir.is_some_and(|d| d == JOURNAL_TRANSCRIPTS)
}

/// Where a kept journal holds the receipt of a transcript, below its directory.
fn transcript_place(r: &Receipt) -> Result<PathBuf, Error> {
    Ok(Path::new(valid_name(&r.domain)?)
        .join(JOURNAL_TRANSCRIPTS)
        .join(format!("{}.yaml", valid_name(&r.id)?)))
}

/// Keep the receipts of the early erasures of transcripts in `dir`, beside the
/// receipts that [`write_journal`] keeps: `<dir>/<domain>/transcripts/<session>.yaml`.
///
/// # Errors
///
/// [`Error::Denied`] for a receipt whose domain or id is not a name; the file system.
pub fn write_transcript_journal(dir: &Path, receipts: &[Receipt]) -> Result<(), Error> {
    for r in receipts {
        write_file(&dir.join(transcript_place(r)?), r.to_yaml().as_bytes())?;
    }
    Ok(())
}

/// Read the receipts that [`write_transcript_journal`] kept. A directory that is not
/// there holds none.
///
/// # Errors
///
/// [`Error::Denied`] for a file whose place does not fit the receipt it holds, or a
/// symbolic link; a receipt that does not parse; the file system.
pub fn read_transcript_journal(dir: &Path) -> Result<Vec<Receipt>, Error> {
    let mut out = Vec::new();
    for below in files_below(dir)? {
        if !of_a_transcript(&below) {
            continue;
        }
        let path = dir.join(&below);
        let text = std::fs::read_to_string(&path).map_err(io(&path))?;
        let receipt = Receipt::from_yaml(&text)?;
        if transcript_place(&receipt)? != below {
            return Err(Error::Denied(format!(
                "the erasure journal: {} holds the receipt of the transcript {} in {}",
                path.display(),
                receipt.id,
                receipt.domain
            )));
        }
        out.push(receipt);
    }
    Ok(out)
}

/// Keep the text of the drift windows in `dir`, beside the receipts, as
/// `<dir>/drift-windows.jsonl`. With no text, nothing is written.
///
/// # Errors
///
/// The file system.
pub fn write_windows(dir: &Path, text: Option<&str>) -> Result<(), Error> {
    match text {
        Some(text) => write_file(&dir.join(JOURNAL_WINDOWS), text.as_bytes()),
        None => Ok(()),
    }
}

/// Read the text that [`write_windows`] kept, when it is there.
///
/// # Errors
///
/// The file system.
pub fn read_windows(dir: &Path) -> Result<Option<String>, Error> {
    let path = dir.join(JOURNAL_WINDOWS);
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io(&path)(e)),
    }
}

/// The lines of the drift windows of a bundle, and after them each line of the windows
/// in force now that the bundle does not hold. The file is a list of events, and a root
/// only adds to it, so the windows in force now begin with those of an earlier bundle.
fn windows_together(of_bundle: &str, now: &str) -> String {
    let mut lines: Vec<&str> = of_bundle.lines().collect();
    for line in now.lines() {
        if !lines.contains(&line) {
            lines.push(line);
        }
    }
    let mut text = lines.join("\n");
    if !text.is_empty() {
        text.push('\n');
    }
    text
}

/// One refusal of a proposal to a personal domain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    /// The personal domain.
    pub domain: String,
    /// The key of the origin of the proposal: a digest, and no text of the proposal.
    pub key: String,
}

/// A key of a refusal record: a digest in hexadecimal, and nothing else.
fn valid_key(key: &str) -> Result<&str, Error> {
    if key.len() == 64 && key.chars().all(|c| c.is_ascii_hexdigit()) {
        Ok(key)
    } else {
        Err(Error::Denied(format!(
            "{key:?} is not the key of a refusal"
        )))
    }
}

/// Keep the refusals of a refusal journal in `dir`, as `<dir>/<domain>/refused.txt`:
/// beside the receipts that [`write_journal`] keeps. The file of a domain is written
/// whole.
///
/// # Errors
///
/// [`Error::Denied`] for a refusal whose domain is not a name or whose key is not a
/// digest; the file system.
pub fn write_refusals(dir: &Path, refusals: &[Refusal]) -> Result<(), Error> {
    let mut by_domain: std::collections::BTreeMap<&str, String> = std::collections::BTreeMap::new();
    for r in refusals {
        let text = by_domain.entry(valid_name(&r.domain)?).or_default();
        text.push_str(valid_key(&r.key)?);
        text.push('\n');
    }
    for (domain, text) in by_domain {
        write_file(&dir.join(domain).join(REFUSALS_FILE), text.as_bytes())?;
    }
    Ok(())
}

/// Read the refusals that [`write_refusals`] kept. A directory that is not there holds
/// no refusal.
///
/// # Errors
///
/// [`Error::Denied`] for a file that is not in the directory of a domain, a line that
/// is not a digest, or a symbolic link; the file system.
pub fn read_refusals(dir: &Path) -> Result<Vec<Refusal>, Error> {
    let mut out = Vec::new();
    for below in files_below(dir)? {
        if below.file_name().is_none_or(|n| n != REFUSALS_FILE) {
            continue;
        }
        let path = dir.join(&below);
        let domain = below.parent().and_then(Path::to_str).unwrap_or_default();
        let domain = valid_name(domain).map_err(|_| {
            Error::Denied(format!(
                "the refusal journal: {} is not in the directory of a domain",
                path.display()
            ))
        })?;
        let text = std::fs::read_to_string(&path).map_err(io(&path))?;
        for key in text.lines() {
            out.push(Refusal {
                domain: domain.to_string(),
                key: valid_key(key)?.to_string(),
            });
        }
    }
    Ok(out)
}

/// Take the next claim epoch from the authority file: one more than the number in the
/// file and than `at_least`. The file holds the new number before this returns, so no
/// epoch is given twice. A file that is not there holds 0. The default for a root is
/// `<root>/claim-epoch`; the file is in no bundle, so a restore does not roll it back.
///
/// # Errors
///
/// [`Error::Denied`] when the file does not hold a number; the file system.
pub fn next_epoch(authority: &Path, at_least: u64) -> Result<u64, Error> {
    let beside = |suffix: &str| {
        let mut name = authority.file_name().unwrap_or_default().to_os_string();
        name.push(suffix);
        authority.with_file_name(name)
    };
    if let Some(dir) = authority.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(io(dir))?;
    }
    // two restores at one time take two numbers
    let lock_file = beside(".lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_file)
        .map_err(io(&lock_file))?;
    lock.lock().map_err(io(&lock_file))?;
    let held = match std::fs::read_to_string(authority) {
        Ok(text) => text.trim().parse::<u64>().map_err(|_| {
            Error::Denied(format!(
                "the claim epoch file {} does not hold a number",
                authority.display()
            ))
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
        Err(e) => return Err(io(authority)(e)),
    };
    let next = held.max(at_least) + 1;
    let temporary = beside(".tmp");
    // one handle that writes and then syncs: a sync needs write access on Windows
    std::fs::File::create(&temporary)
        .and_then(|mut f| {
            use std::io::Write as _;
            f.write_all(format!("{next}\n").as_bytes())?;
            f.sync_all()
        })
        .map_err(io(&temporary))?;
    std::fs::rename(&temporary, authority).map_err(io(authority))?;
    Ok(next)
}

/// What a restore takes from outside the bundle: each is in force now, and the bundle
/// does not roll it back.
#[derive(Clone, Copy, Debug)]
pub struct Authority<'a> {
    /// The text of the grants in force now. It becomes the grants of the restored root.
    pub grants: &'a str,
    /// The text of the registry in force now, when there is one.
    pub registry: Option<&'a str>,
    /// The erasure journal: the receipt of each erasure that the restored root must hold.
    pub journal: &'a [Receipt],
    /// The refusal journal: each refusal of a proposal that the restored root must hold.
    pub refusals: &'a [Refusal],
    /// The receipt of each early erasure of a transcript that the restored root must
    /// hold.
    pub transcripts: &'a [Receipt],
    /// The text of the file of the drift windows in force now, when there is one. The
    /// restored root holds each window of the bundle and each window of this text.
    pub drift_windows: Option<&'a str>,
    /// The claim epoch file ([`next_epoch`]).
    pub epoch_file: &'a Path,
    /// Whether the restored root keeps its ledger in the file under the root, whatever
    /// the grants say. The `[ledger]` table is then left out of the grants written.
    pub file_ledger: bool,
}

/// What a restore did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Restored {
    /// The claim epoch that the restore began.
    pub epoch: u64,
    /// The tasks that were claimed in the bundle and are open again.
    pub reopened: Vec<String>,
    /// The entries that the bundle held with a body and the journal says are erased:
    /// each was erased again.
    pub erased: Vec<String>,
    /// The entries of the journal that the restored record does not hold with a body
    /// and had no receipt for: the receipt is beside the record now.
    pub receipts_kept: Vec<String>,
    /// The transcripts that the bundle held with turns and the journal says are erased:
    /// each was erased again.
    pub transcripts_erased: Vec<String>,
    /// The proposals that the bundle held and the refusal journal says are refused: each
    /// was removed.
    pub proposals_refused: Vec<String>,
    /// How many entries the index was built from.
    pub entries: usize,
    /// How many events of the ledger were restored.
    pub ledger_events: u64,
}

/// Restore a recovery bundle into `into`, a directory that is empty or not there. Each
/// step is a rule, in this order:
///
/// 1. The manifest is read and checked, and the digest of every file is compared. A
///    bundle that fails writes nothing into `into`.
/// 2. `into` gets the marker [`RESTORING`], then the record, the logs, the quarantine,
///    the refusal records and the drift windows of the bundle, and the grants and the
///    registry of `authority`. The drift windows of `authority` are added to those of
///    the bundle.
///    The configuration of the bundle goes to [`RESTORED_CONFIG`].
/// 3. The ledger takes its events, each with its sequence number.
/// 4. The claim epoch file gives the next epoch, and the ledger begins it.
/// 5. Each entry of the erasure journal that the record holds with a body is erased as a
///    live erasure does it. For an entry that the record does not hold, the receipt is
///    put beside the record, so an import of that text is refused. Each transcript of
///    the journal that the record holds with turns is erased again in the same way.
/// 6. Each refusal of the refusal journal goes into the refusal record of its domain. A
///    proposal that the bundle holds with the key of a refusal is removed, and its task
///    is invalidated.
/// 7. The index is built again from the record and the log.
/// 8. The marker is removed.
///
/// # Errors
///
/// [`Error::Denied`] for a bundle that is not whole or an `into` that holds something;
/// grants or a registry that do not parse; the file system, the ledger, the store or
/// the index. After step 2 a failure leaves the marker, and the root serves nothing.
pub async fn restore(
    bundle: &Path,
    into: &Path,
    authority: &Authority<'_>,
) -> Result<Restored, Error> {
    let manifest = verified(bundle)?;
    let grants = grants_to_write(authority)?;
    if into.join(GRANTS_FILE).exists() {
        return Err(Error::Denied(format!(
            "{} holds a root already: it has {GRANTS_FILE}",
            into.display()
        )));
    }
    empty_or_absent(into, "the directory to restore into")?;
    lay_out(bundle, into, &manifest, &grants, authority)?;
    let memory = Memory::open_unchecked(into)?;
    let ledger_events = restore_ledger(&memory, bundle, &manifest).await?;
    let (epoch, reopened) = begin_next_epoch(&memory, authority, &manifest).await?;
    let (erased, receipts_kept) = apply_journal(&memory, authority.journal).await?;
    let transcripts_erased = apply_transcript_journal(&memory, authority.transcripts).await?;
    let proposals_refused = apply_refusals(&memory, authority.refusals).await?;
    let (entries, _) = memory.reindex().await?;
    let marker = into.join(RESTORING);
    std::fs::remove_file(&marker).map_err(io(&marker))?;
    Ok(Restored {
        epoch,
        reopened,
        erased,
        receipts_kept,
        transcripts_erased,
        proposals_refused,
        entries,
        ledger_events,
    })
}

/// Where a file of the bundle goes below the restored root. `None` for the events of the
/// ledger, which the ledger takes.
fn laid_out_at(path: &str) -> Result<Option<PathBuf>, Error> {
    if path == DRIFT_WINDOWS {
        return Ok(Some(PathBuf::from(crate::drift_windows::FILE)));
    }
    let (top, rest) = path.split_once('/').unwrap_or((path, ""));
    let dir = match top {
        LEDGER_EVENTS if rest.is_empty() => return Ok(None),
        "record" => "record",
        "log" => log::DIR,
        "quarantine" => QUARANTINE_DIR,
        "refused" => crate::registry::REFUSED_DIR,
        "config" => RESTORED_CONFIG,
        _ => "",
    };
    if dir.is_empty() || rest.is_empty() {
        return Err(refuse(format!("{path:?} is no part of a bundle")));
    }
    Ok(Some(Path::new(dir).join(rest)))
}

/// The bytes of a file of the bundle, when they are what the manifest names.
fn checked_bytes(bundle: &Path, file: &BundledFile) -> Result<Vec<u8>, Error> {
    let path = bundle.join(&file.path);
    let kind = std::fs::symlink_metadata(&path)
        .map_err(io(&path))?
        .file_type();
    if !kind.is_file() {
        return Err(refuse(format!("{} is not a regular file", path.display())));
    }
    let bytes = std::fs::read(&path).map_err(io(&path))?;
    if format::digest_bytes(&bytes) != file.digest {
        return Err(refuse(format!(
            "{} is not the file that the manifest names: its digest is another",
            path.display()
        )));
    }
    Ok(bytes)
}

/// The manifest of a bundle whose every file is there and is what the manifest names.
fn verified(bundle: &Path) -> Result<RecoveryManifest, Error> {
    let path = bundle.join(format::FILE);
    let text = std::fs::read_to_string(&path).map_err(io(&path))?;
    let manifest = RecoveryManifest::from_yaml(&text)?;
    manifest.usable()?;
    for file in &manifest.files {
        laid_out_at(&file.path)?;
        checked_bytes(bundle, file)?;
    }
    Ok(manifest)
}

/// The grants that the restored root gets: the grants in force, without the `[ledger]`
/// table when the restored root keeps a file ledger. The registry is parsed too, so a
/// restore does not start with one that the root cannot load.
fn grants_to_write(authority: &Authority<'_>) -> Result<String, Error> {
    if let Some(registry) = authority.registry {
        Registry::parse(registry)?;
    }
    let mut grants = Grants::parse(authority.grants)?;
    if authority.file_ledger && grants.ledger.is_some() {
        grants.ledger = None;
        return Ok(grants.render()?);
    }
    Ok(authority.grants.to_string())
}

/// Step 2 of [`restore`]: the marker first, then the files of the bundle, each checked
/// again as it is read, then the grants and the registry in force.
fn lay_out(
    bundle: &Path,
    into: &Path,
    manifest: &RecoveryManifest,
    grants: &str,
    authority: &Authority<'_>,
) -> Result<(), Error> {
    let registry = authority.registry;
    write_file(
        &into.join(RESTORING),
        b"A restore of this root began and did not finish.\n",
    )?;
    let record = into.join("record");
    std::fs::create_dir_all(&record).map_err(io(&record))?;
    for file in &manifest.files {
        if let Some(below) = laid_out_at(&file.path)? {
            write_file(&into.join(below), &checked_bytes(bundle, file)?)?;
        }
    }
    write_file(&into.join(GRANTS_FILE), grants.as_bytes())?;
    if let Some(registry) = registry {
        write_file(&into.join(REGISTRY_FILE), registry.as_bytes())?;
    }
    if let Some(now) = authority.drift_windows {
        let path = into.join(crate::drift_windows::FILE);
        let of_bundle = std::fs::read_to_string(&path).unwrap_or_default();
        write_file(&path, windows_together(&of_bundle, now).as_bytes())?;
    }
    Ok(())
}

/// The events of the ledger that a bundle holds.
fn ledger_events(bundle: &Path, manifest: &RecoveryManifest) -> Result<Vec<Checkpointed>, Error> {
    let file = manifest
        .files
        .iter()
        .find(|f| f.path == LEDGER_EVENTS)
        .ok_or_else(|| refuse(format!("the manifest names no {LEDGER_EVENTS}")))?;
    let text = String::from_utf8(checked_bytes(bundle, file)?)
        .map_err(|e| refuse(format!("{LEDGER_EVENTS}: {e}")))?;
    let events = text
        .lines()
        .map(|line| {
            serde_json::from_str::<Checkpointed>(line)
                .map_err(|e| refuse(format!("{LEDGER_EVENTS}: {e}")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if events.len() as u64 != manifest.ledger.events {
        return Err(refuse(format!(
            "{LEDGER_EVENTS} holds {} events, and the manifest says {}",
            events.len(),
            manifest.ledger.events
        )));
    }
    Ok(events)
}

/// Step 3 of [`restore`].
async fn restore_ledger(
    memory: &Memory,
    bundle: &Path,
    manifest: &RecoveryManifest,
) -> Result<u64, Error> {
    let events = ledger_events(bundle, manifest)?;
    Ok(memory.ledger.lock().await.restore(&events).await?)
}

/// Step 4 of [`restore`]: the next epoch from the authority, above the epoch of the
/// bundle, and the ledger begins it. Returns the epoch and the tasks that are open again.
async fn begin_next_epoch(
    memory: &Memory,
    authority: &Authority<'_>,
    manifest: &RecoveryManifest,
) -> Result<(u64, Vec<String>), Error> {
    let mut ledger = memory.ledger.lock().await;
    let in_force = ledger.epoch().await?.max(manifest.ledger.epoch);
    let epoch = next_epoch(authority.epoch_file, in_force)?;
    let reopened = ledger.begin_epoch(epoch).await?;
    Ok((epoch, reopened))
}

/// Step 5 of [`restore`]. Returns the entries erased again, and the entries whose
/// receipt was put beside the record.
async fn apply_journal(
    memory: &Memory,
    journal: &[Receipt],
) -> Result<(Vec<String>, Vec<String>), Error> {
    let (mut erased, mut kept) = (Vec::new(), Vec::new());
    for receipt in journal {
        let (domain, id) = (valid_name(&receipt.domain)?, valid_name(&receipt.id)?);
        let held = memory.store.get(domain, id).await?;
        match held.filter(|e| e.front.status != Some(Status::Erased)) {
            Some(entry) => {
                erase_again(memory, receipt, &entry).await?;
                erased.push(receipt.id.clone());
            }
            None if memory.store.receipt(domain, id).await?.is_none() => {
                memory.store.put_receipt(receipt).await?;
                kept.push(receipt.id.clone());
            }
            None => {}
        }
    }
    Ok((erased, kept))
}

/// The part of step 5 of [`restore`] for transcripts. Returns the transcripts erased
/// again.
async fn apply_transcript_journal(
    memory: &Memory,
    journal: &[Receipt],
) -> Result<Vec<String>, Error> {
    let mut erased = Vec::new();
    for receipt in journal {
        valid_name(&receipt.domain)?;
        valid_name(&receipt.id)?;
        if memory.erase_transcript_again(receipt).await? {
            erased.push(receipt.id.clone());
        }
    }
    Ok(erased)
}

/// Step 6 of [`restore`]. Returns the proposals removed.
async fn apply_refusals(memory: &Memory, refusals: &[Refusal]) -> Result<Vec<String>, Error> {
    let mut removed = Vec::new();
    for r in refusals {
        let (domain, key) = (valid_name(&r.domain)?, valid_key(&r.key)?);
        removed.extend(memory.refuse_again(domain, key).await?);
    }
    Ok(removed)
}

/// Erase an entry that the bundle held with a body. In a granted domain this is the
/// ordinary erasure, with the reason of the journal: every version, the index, the
/// text in the ledger and the entries downstream. In a domain that the grants in force
/// do not hold, no session reads the entry, and the erasure is of the record alone: the
/// stub over every version, and the receipt of the journal.
async fn erase_again(memory: &Memory, receipt: &Receipt, entry: &Entry) -> Result<(), Error> {
    if memory.grants.domains.contains_key(&receipt.domain) {
        memory.erase(&receipt.id, receipt.reason).await?;
        return Ok(());
    }
    let mut stub = erasure::stub(entry, receipt.reason);
    stub.front.domain.clone_from(&receipt.domain);
    stub.front.id.clone_from(&receipt.id);
    memory
        .store
        .erase(&receipt.domain, &receipt.id, &stub)
        .await?;
    memory.store.put_receipt(receipt).await?;
    Ok(())
}

/// How one case of a restore drill ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    /// The bundle held the case, and the restored root has what the case needs.
    Recovered,
    /// The bundle did not hold the case, or the mechanism of the case is not built.
    Absent,
    /// The restored root does not have what the case needs.
    Failed,
}

/// One case of a restore drill.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Case {
    /// The name of the case.
    pub name: String,
    /// How it ended.
    pub outcome: Outcome,
    /// What was checked, or why the case is absent.
    pub detail: String,
}

/// The result of a restore drill.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Drill {
    /// The claim epoch that the drill's restore began.
    pub epoch: u64,
    /// The five cases.
    pub cases: Vec<Case>,
    /// Whether no case failed.
    pub passed: bool,
}

fn case(name: &str, outcome: Outcome, detail: impl Into<String>) -> Case {
    Case {
        name: name.to_string(),
        outcome,
        detail: detail.into(),
    }
}

fn not_built(name: &str, why: &str) -> Case {
    case(name, Outcome::Absent, why)
}

/// A task that the bundle's ledger held under a claim.
struct ClaimHeld {
    id: String,
    holder: String,
    generation: u64,
}

/// An entry of the erasure journal that the bundle holds with a body.
struct ErasedLater {
    receipt: Receipt,
    /// Words of the entry that a search finds it by: its title, or its first line.
    words: String,
}

/// What a bundle held, read from the bundle before a drill restores it.
struct Held {
    /// The tasks that were proposed, open, claimed or blocked.
    unfinished: Vec<String>,
    claimed: Vec<ClaimHeld>,
    /// Each effect that nobody had settled, with its task: an external action that was
    /// begun, and of which the ledger does not know whether it took place.
    unsettled: Vec<(String, String)>,
    erased_later: Vec<ErasedLater>,
    /// The entries that a `put` of the log names above the cursor of the index, each
    /// with its domain: versions that the index did not hold.
    awaiting: Vec<(String, String)>,
}

impl Held {
    async fn read(bundle: &Path, journal: &[Receipt]) -> Result<Held, Error> {
        let manifest = verified(bundle)?;
        let tasks = tasks_held(bundle, &manifest).await?;
        let mut erased_later = Vec::new();
        for receipt in journal {
            if let Some(words) = body_held(bundle, &manifest, receipt)? {
                erased_later.push(ErasedLater {
                    receipt: receipt.clone(),
                    words,
                });
            }
        }
        let mut awaiting = Vec::new();
        for d in &manifest.domains {
            awaiting.extend(versions_awaiting(bundle, &manifest, d)?);
        }
        Ok(Held {
            unfinished: tasks.unfinished,
            claimed: tasks.claimed,
            unsettled: tasks.unsettled,
            erased_later,
            awaiting,
        })
    }
}

/// The unfinished tasks of a bundle.
#[derive(Default)]
struct TasksHeld {
    unfinished: Vec<String>,
    claimed: Vec<ClaimHeld>,
    unsettled: Vec<(String, String)>,
}

/// The unfinished tasks of a bundle, those of them under a claim, and the effects on
/// them that nobody had settled: its events are replayed into a ledger in memory.
async fn tasks_held(bundle: &Path, manifest: &RecoveryManifest) -> Result<TasksHeld, Error> {
    use dm_ledger::Status as Task;
    let mut ledger = Ledger::in_memory()?;
    ledger.restore(&ledger_events(bundle, manifest)?).await?;
    let mut held = TasksHeld::default();
    for d in &manifest.domains {
        for status in [Task::Proposed, Task::Open, Task::Claimed, Task::Blocked] {
            for task in ledger.list(&d.domain, status).await? {
                if let (Task::Claimed, Some(holder)) = (status, &task.holder) {
                    held.claimed.push(ClaimHeld {
                        id: task.id.clone(),
                        holder: holder.clone(),
                        generation: task.generation,
                    });
                }
                for action in &task.effects {
                    held.unsettled.push((task.id.clone(), action.clone()));
                }
                held.unfinished.push(task.id);
            }
        }
    }
    Ok(held)
}

/// The file of the bundle at `path`, when the manifest names it.
fn listed<'a>(manifest: &'a RecoveryManifest, path: &str) -> Option<&'a BundledFile> {
    manifest.files.iter().find(|f| f.path == path)
}

/// Words to search for the entry of `receipt`, when the bundle holds that entry with a
/// body: its title, or the first line of its body.
fn body_held(
    bundle: &Path,
    manifest: &RecoveryManifest,
    receipt: &Receipt,
) -> Result<Option<String>, Error> {
    let path = format!(
        "record/{}/{}.md",
        valid_name(&receipt.domain)?,
        valid_name(&receipt.id)?
    );
    let Some(file) = listed(manifest, &path) else {
        return Ok(None);
    };
    let text = String::from_utf8_lossy(&checked_bytes(bundle, file)?).into_owned();
    let Ok(entry) = Entry::parse(&text) else {
        return Ok(None);
    };
    if entry.front.status == Some(Status::Erased) {
        return Ok(None);
    }
    let first_line = entry.body.lines().find(|l| !l.trim().is_empty());
    let words = entry
        .front
        .title
        .clone()
        .or_else(|| first_line.map(|l| l.trim_start_matches('#').trim().to_string()))
        .unwrap_or_default();
    Ok(Some(words))
}

/// The entries that the log of one domain of the bundle names in a `put` above the
/// cursor of the index.
fn versions_awaiting(
    bundle: &Path,
    manifest: &RecoveryManifest,
    d: &DomainCheckpoint,
) -> Result<Vec<(String, String)>, Error> {
    let path = format!("log/{}.jsonl", valid_name(&d.domain)?);
    let Some(file) = listed(manifest, &path) else {
        return Ok(Vec::new());
    };
    let text = String::from_utf8_lossy(&checked_bytes(bundle, file)?).into_owned();
    let mut out = Vec::new();
    for event in text
        .lines()
        .filter_map(|l| serde_json::from_str::<log::Event>(l).ok())
    {
        if let (true, log::Change::Put { id, .. }) = (event.seq > d.index_fed, event.change) {
            if !out.contains(&(d.domain.clone(), id.clone())) {
                out.push((d.domain.clone(), id));
            }
        }
    }
    Ok(out)
}

const UNFINISHED_TASK: &str = "unfinished-task";
const ERASURE_AFTER_BACKUP: &str = "erasure-after-backup";
const AWAITING: &str = "write-awaiting-materialisation";
const UNCERTAIN_ACTION: &str = "uncertain-external-action";

/// The case of a task that was not finished when the bundle was made: each such task is
/// in the restored ledger and none is under a claim, and the holder of a claim from
/// before the restore is refused.
async fn unfinished_task(m: &Memory, held: &Held) -> Case {
    let mut ledger = m.ledger.lock().await;
    let mut open_claim = None;
    for id in &held.unfinished {
        match ledger.get(id).await {
            Ok(t) if t.status == dm_ledger::Status::Claimed => {
                let why = format!("the task {id} is under a claim after the restore");
                return case(UNFINISHED_TASK, Outcome::Failed, why);
            }
            Ok(t) => {
                let was = held.claimed.iter().find(|c| &c.id == id);
                open_claim = open_claim.or(was.filter(|_| !t.status.is_terminal()));
            }
            Err(e) => {
                let why = format!("the task {id} is not in the restored ledger: {e}");
                return case(UNFINISHED_TASK, Outcome::Failed, why);
            }
        }
    }
    let Some(claim) = open_claim else {
        let why = "the bundle held no claimed task that is still open after the restore";
        return case(UNFINISHED_TASK, Outcome::Absent, why);
    };
    let by = dm_ledger::Held {
        holder: &claim.holder,
        generation: claim.generation,
    };
    let lease = time::Duration::minutes(1);
    match ledger.heartbeat(&claim.id, by, lease).await {
        Err(dm_ledger::Error::Fenced { .. }) => case(
            UNFINISHED_TASK,
            Outcome::Recovered,
            format!(
                "{} unfinished tasks are in the restored ledger and none is under a claim; \
                 the heartbeat of {} under its claim on {} from before the restore is refused",
                held.unfinished.len(),
                claim.holder,
                claim.id
            ),
        ),
        other => {
            let said = other.map_or_else(|e| e.to_string(), |_| "it was renewed".to_string());
            let why = format!(
                "the heartbeat of {} under its claim on {} from before the restore was not \
                 refused as a claim of an earlier epoch: {said}",
                claim.holder, claim.id
            );
            case(UNFINISHED_TASK, Outcome::Failed, why)
        }
    }
}

/// The case of an external action that nobody had settled when the bundle was made:
/// the restored ledger still names the effect on its task, and holds the task back from
/// every claim. Thus nobody does the action a second time before its holder or a person
/// settles the effect.
async fn uncertain_external_action(m: &Memory, held: &Held) -> Case {
    if held.unsettled.is_empty() {
        let why = "the bundle held no task with an effect that nobody had settled";
        return case(UNCERTAIN_ACTION, Outcome::Absent, why);
    }
    let ledger = m.ledger.lock().await;
    for (id, action) in &held.unsettled {
        let why = match ledger.get(id).await {
            Ok(t) if !t.effects.contains(action) => {
                format!("the restored task {id} does not name the effect {action}")
            }
            Ok(t) if t.holder.is_some() || t.held_back().is_none() => {
                format!("the restored ledger does not hold the task {id} back from a claim")
            }
            Ok(_) => continue,
            Err(e) => format!("the task {id} is not in the restored ledger: {e}"),
        };
        return case(UNCERTAIN_ACTION, Outcome::Failed, why);
    }
    let why = format!(
        "{} effects that nobody had settled are on their tasks in the restored ledger, \
         and the ledger holds each task back from every claim",
        held.unsettled.len()
    );
    case(UNCERTAIN_ACTION, Outcome::Recovered, why)
}

/// Why the restored root still has something of an entry that the journal says is
/// erased, or `None` when it has only the stub and the receipt.
async fn left_of(m: &Memory, e: &ErasedLater) -> Result<Option<String>, Error> {
    let (domain, id) = (&e.receipt.domain, &e.receipt.id);
    let is_stub = |text: &str| {
        Entry::parse(text).is_ok_and(|entry| entry.front.status == Some(Status::Erased))
    };
    match m.store.get_text(domain, id).await? {
        Some(text) if is_stub(&text) => {}
        _ => return Ok(Some(format!("the record holds {id} with a body"))),
    }
    for version in m.store.history(domain, id).await? {
        if !is_stub(&version.text) {
            return Ok(Some(format!(
                "the archived version {} of {id} holds a body",
                version.name
            )));
        }
    }
    if m.store.receipt(domain, id).await?.is_none() {
        return Ok(Some(format!("{id} has no receipt")));
    }
    let granted = m.grants.domains.contains_key(domain);
    if granted && !e.words.is_empty() {
        let hits = m.search(&e.words, Some(domain), 50).await?;
        if hits.iter().any(|h| &h.id == id) {
            return Ok(Some(format!("a search finds {id}")));
        }
    }
    Ok(None)
}

/// The case of an entry that was erased after the bundle was made: in the restored root
/// each version of it is a stub, a search does not find it, and its receipt is there.
async fn erasure_after_backup(m: &Memory, held: &Held) -> Case {
    if held.erased_later.is_empty() {
        let why = "the bundle held no entry with a body that the erasure journal names";
        return case(ERASURE_AFTER_BACKUP, Outcome::Absent, why);
    }
    for e in &held.erased_later {
        match left_of(m, e).await {
            Ok(None) => {}
            Ok(Some(why)) => return case(ERASURE_AFTER_BACKUP, Outcome::Failed, why),
            Err(err) => return case(ERASURE_AFTER_BACKUP, Outcome::Failed, err.to_string()),
        }
    }
    let why = format!(
        "{} entries that were erased after the bundle are stubs in every version, a search \
         finds none of them, and each has its receipt",
        held.erased_later.len()
    );
    case(ERASURE_AFTER_BACKUP, Outcome::Recovered, why)
}

/// Why the index of the restored root is behind its log, or `None` when every entry
/// that the log says is live is in the index and no version waits for a feed.
async fn behind_the_log(m: &Memory) -> Result<Option<String>, Error> {
    let every = ReadFilter {
        include_inactive: true,
        ..ReadFilter::any_domain(Label::Restricted)
    };
    for domain in m.domains() {
        let events = log::read(m.root(), &domain, 0)?;
        for (id, now) in log::state(&events) {
            let indexed = m.index.lock().await.meta(&id, &every)?.is_some();
            if now.is_some() && !indexed {
                return Ok(Some(format!(
                    "the log of {domain} says that {id} is live, and the index does not hold it"
                )));
            }
        }
        let unfed = m.freshness(Some(&domain))?.unfed;
        if unfed > 0 {
            return Ok(Some(format!(
                "{unfed} versions in the log of {domain} wait for the index"
            )));
        }
    }
    Ok(None)
}

/// The case of a write that the log held and the index did not when the bundle was
/// made: in the restored root the index holds every entry that the log says is live.
async fn awaiting_materialisation(m: &Memory, held: &Held) -> Case {
    match behind_the_log(m).await {
        Ok(None) => {}
        Ok(Some(why)) => return case(AWAITING, Outcome::Failed, why),
        Err(e) => return case(AWAITING, Outcome::Failed, e.to_string()),
    }
    if held.awaiting.is_empty() {
        let why = "the log of the bundle named no version above the cursor of the index";
        return case(AWAITING, Outcome::Absent, why);
    }
    let why = format!(
        "{} versions that the index did not hold when the bundle was made are in the \
         restored index, with every entry that the log says is live",
        held.awaiting.len()
    );
    case(AWAITING, Outcome::Recovered, why)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WriteRequest;
    use dm_core::erasure::Reason;
    use dm_ledger::{Status as TaskStatus, TaskSpec};
    use tempfile::TempDir;
    use time::Duration;

    const OWNER: &str = "human:test:me";
    const DOMAIN: &str = "dom-me";
    const WORKER: &str = "agent:worker";

    fn req(body: &str) -> WriteRequest {
        WriteRequest {
            body: body.into(),
            ..WriteRequest::default()
        }
    }

    fn task(title: &str) -> TaskSpec {
        TaskSpec {
            domain: DOMAIN.into(),
            title: title.into(),
            body: "Do it, and say how.\n".into(),
            ..TaskSpec::default()
        }
    }

    async fn admitted(m: &Memory, title: &str) -> String {
        let id = m.create_task(task(title)).await.unwrap().task.id;
        // a filing that the grant of the domain admits at once is admitted already
        let _ = m.admit(&id).await;
        id
    }

    async fn found(m: &Memory, word: &str) -> Vec<String> {
        let hits = m.search(word, None, 10).await.unwrap();
        hits.into_iter().map(|h| h.id).collect()
    }

    async fn open_tasks(m: &Memory) -> Vec<String> {
        let mut ids: Vec<String> = m
            .tasks(None, TaskStatus::Open)
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect();
        ids.sort();
        ids
    }

    /// A root, the place for its bundle, and a place to restore into.
    struct Site {
        dir: TempDir,
        m: Memory,
    }

    impl Site {
        fn new() -> Site {
            let dir = tempfile::tempdir().unwrap();
            let m = Memory::init(&dir.path().join("live"), OWNER, DOMAIN).unwrap();
            Site { dir, m }
        }

        fn root(&self) -> PathBuf {
            self.dir.path().join("live")
        }

        fn at(&self, name: &str) -> PathBuf {
            self.dir.path().join(name)
        }

        fn grants(&self) -> String {
            std::fs::read_to_string(self.root().join(GRANTS_FILE)).unwrap()
        }

        async fn bundle(&self, name: &str) -> RecoveryManifest {
            self.m.recovery_bundle(&self.at(name)).await.unwrap()
        }

        /// Restore the bundle `from` into `into`, with the grants of the live root.
        async fn restore(&self, from: &str, into: &str, journal: &[Receipt]) -> Restored {
            let grants = self.grants();
            let epoch_file = self.at("claim-epoch");
            let authority = Authority {
                grants: &grants,
                registry: None,
                journal,
                refusals: &[],
                transcripts: &[],
                drift_windows: None,
                epoch_file: &epoch_file,
                file_ledger: false,
            };
            restore(&self.at(from), &self.at(into), &authority)
                .await
                .unwrap()
        }

        /// As [`Site::restore`], with the receipts of transcripts and the drift windows
        /// of the live root.
        async fn restore_with(
            &self,
            from: &str,
            into: &str,
            transcripts: &[Receipt],
            windows: Option<&str>,
        ) -> Restored {
            let grants = self.grants();
            let epoch_file = self.at("claim-epoch");
            let authority = Authority {
                grants: &grants,
                registry: None,
                journal: &[],
                refusals: &[],
                transcripts,
                drift_windows: windows,
                epoch_file: &epoch_file,
                file_ledger: false,
            };
            restore(&self.at(from), &self.at(into), &authority)
                .await
                .unwrap()
        }

        /// What a restore of the bundle `from` into `into` is refused with.
        async fn refused(&self, from: &str, into: &str) -> Error {
            let grants = self.grants();
            let epoch_file = self.at("claim-epoch");
            let authority = Authority {
                grants: &grants,
                registry: None,
                journal: &[],
                refusals: &[],
                transcripts: &[],
                drift_windows: None,
                epoch_file: &epoch_file,
                file_ledger: false,
            };
            restore(&self.at(from), &self.at(into), &authority)
                .await
                .unwrap_err()
        }
    }

    /// Two entries, an open task, and a task that `agent:worker` holds. Returns the
    /// entries, the open task, and the claimed task with the generation of its claim.
    async fn with_work(site: &Site) -> ([String; 2], String, (String, u64)) {
        let m = &site.m;
        let port = m
            .write(req("# Port\n\nIt listens on 8080.\n"))
            .await
            .unwrap();
        let host = m
            .write(req("# Host\n\nIt runs on basalt.\n"))
            .await
            .unwrap();
        let open = admitted(m, "Rotate the key").await;
        let held = admitted(m, "Move the vault").await;
        let worker = Memory::open(&site.root()).unwrap().with_author(WORKER);
        let claim = worker.claim(&held, Duration::minutes(30)).await.unwrap();
        ([port.id, host.id], open, (held, claim.generation))
    }

    fn is_fenced<T>(r: &Result<T, Error>) -> bool {
        matches!(r, Err(Error::Ledger(dm_ledger::Error::Fenced { .. })))
    }

    #[tokio::test]
    async fn a_bundle_restores_the_entries_and_the_tasks_and_ends_the_claims() {
        let site = Site::new();
        let (entries, open, (held, generation)) = with_work(&site).await;
        let manifest = site.bundle("bundle").await;
        assert_eq!(manifest.ledger.epoch, 0);
        assert!(manifest.files.iter().all(|f| !f.path.contains("index")));
        let restored = site.restore("bundle", "restored", &[]).await;
        assert_eq!(
            (restored.epoch, &restored.reopened, restored.entries),
            (1, &vec![held.clone()], 2)
        );
        assert_eq!(restored.ledger_events, manifest.ledger.events);
        let m = Memory::open(&site.at("restored")).unwrap();
        for id in &entries {
            assert_eq!(&m.get(id).await.unwrap().front.id, id);
        }
        assert_eq!(found(&m, "basalt").await, [entries[1].clone()]);
        let mut both = vec![open, held.clone()];
        both.sort();
        assert_eq!(open_tasks(&m).await, both);
        // the holder of the claim from before the restore, with its name and a lease
        // that is still live by the clock
        let worker = Memory::open(&site.at("restored"))
            .unwrap()
            .with_author(WORKER);
        let late = worker
            .heartbeat(&held, Some(generation), Duration::minutes(30))
            .await;
        assert!(is_fenced(&late), "{:?}", late.map(|t| t.generation));
        // in the live root that claim is still in force
        let live = Memory::open(&site.root()).unwrap().with_author(WORKER);
        live.heartbeat(&held, Some(generation), Duration::minutes(30))
            .await
            .unwrap();
    }

    /// An agent wrote an entry inside a drift window. The window is closed, and the
    /// entry stays held from promotion in the restored root too.
    #[tokio::test]
    async fn a_restored_root_holds_the_drift_windows_of_the_bundle() {
        let site = Site::new();
        let worker = Memory::open(&site.root()).unwrap().with_author(WORKER);
        let from = OffsetDateTime::now_utc() - Duration::minutes(1);
        site.m
            .open_drift_window("gateway", "model-1", from, &[], "the probes moved")
            .unwrap();
        let inside = worker.write(req("# Port\n\nIt listens on 8080.\n")).await;
        let inside = inside.unwrap().id;
        let until = OffsetDateTime::now_utc() + Duration::minutes(1);
        site.m
            .close_drift_window("gateway", "model-1", until, "a new baseline")
            .unwrap();
        let windows = site.m.drift_windows().unwrap();
        assert_eq!(windows.len(), 1);
        let held = |m: &Memory, e: &Entry| m.frozen_by(e).unwrap();
        let entry = site.m.get(&inside).await.unwrap();
        assert_eq!(held(&site.m, &entry).as_ref(), Some(&windows[0]));
        site.bundle("bundle").await;
        site.restore("bundle", "restored", &[]).await;
        let m = Memory::open(&site.at("restored")).unwrap();
        assert_eq!(m.drift_windows().unwrap(), windows);
        let entry = m.get(&inside).await.unwrap();
        assert_eq!(held(&m, &entry).as_ref(), Some(&windows[0]));
    }

    /// A window that was opened after the bundle was made, from a time before it,
    /// holds an entry of the bundle. The restore and the drill take the windows in
    /// force, so the restored root holds that entry from promotion.
    #[tokio::test]
    async fn a_window_opened_after_the_bundle_holds_in_the_restored_root() {
        let site = Site::new();
        let worker = Memory::open(&site.root()).unwrap().with_author(WORKER);
        let from = OffsetDateTime::now_utc() - Duration::minutes(1);
        let earlier = from - Duration::hours(2);
        let closed = from - Duration::hours(1);
        let open = |model, from| {
            site.m
                .open_drift_window("gateway", model, from, &[], "moved")
        };
        open("model-0", earlier).unwrap();
        site.m
            .close_drift_window("gateway", "model-0", closed, "a new baseline")
            .unwrap();
        let inside = worker.write(req("# Port\n\nIt listens on 8080.\n")).await;
        let inside = inside.unwrap().id;
        site.bundle("bundle").await;
        let late = open("model-1", from).unwrap();
        let live = site.m.drift_windows().unwrap();
        assert_eq!(live.len(), 2);
        let windows = site.m.drift_journal().unwrap();
        write_windows(&site.at("journal"), windows.as_deref()).unwrap();
        assert_eq!(read_journal(&site.at("journal")).unwrap(), Vec::new());
        let windows = read_windows(&site.at("journal")).unwrap();
        site.restore_with("bundle", "restored", &[], windows.as_deref())
            .await;
        let m = Memory::open(&site.at("restored")).unwrap();
        assert_eq!(m.drift_windows().unwrap(), live);
        let entry = m.get(&inside).await.unwrap();
        assert_eq!(m.frozen_by(&entry).unwrap(), Some(late));
        // the drill takes the windows of its own root
        let drill = site.m.drill(&site.at("bundle"), &site.at("scratch")).await;
        drill.unwrap();
        let scratch = Memory::open(&site.at("scratch")).unwrap();
        assert_eq!(scratch.drift_windows().unwrap(), live);
    }

    /// Each file below `dir` whose bytes hold `word`.
    fn holding(dir: &Path, word: &str) -> Vec<PathBuf> {
        let found = files_below(dir).unwrap().into_iter().filter(|below| {
            let bytes = std::fs::read(dir.join(below)).unwrap();
            bytes.windows(word.len()).any(|w| w == word.as_bytes())
        });
        found.collect()
    }

    fn zanzibar() -> crate::transcripts::Keep {
        crate::transcripts::Keep {
            domain: None,
            harness: "claude".into(),
            origin: "a.jsonl".into(),
            raw: "{\"type\":\"user\",\"message\":{\"content\":\"The word is zanzibar.\"}}\n".into(),
            turns: vec![("user".into(), "The word is zanzibar.".into())],
        }
    }

    #[tokio::test]
    async fn a_transcript_erased_after_the_bundle_is_erased_in_the_restored_root() {
        use crate::transcripts::State;
        let site = Site::new();
        let kept = site.m.keep_transcript(zanzibar()).await.unwrap();
        site.bundle("bundle").await;
        assert!(!holding(&site.at("bundle"), "zanzibar").is_empty());
        site.m
            .erase_transcript(&kept.session, Reason::Sensitive)
            .await
            .unwrap();
        // the journal as a person keeps it: the receipt of a transcript is no receipt
        // of an entry
        let journal = site.m.transcript_journal().await.unwrap();
        assert_eq!(journal.len(), 1);
        assert_eq!(site.m.erasure_journal().await.unwrap(), Vec::new());
        write_transcript_journal(&site.at("journal"), &journal).unwrap();
        assert_eq!(read_journal(&site.at("journal")).unwrap(), Vec::new());
        let journal = read_transcript_journal(&site.at("journal")).unwrap();
        let restored = site
            .restore_with("bundle", "restored", &journal, None)
            .await;
        assert_eq!(
            restored.transcripts_erased,
            std::slice::from_ref(&kept.session)
        );
        let record = site.at("restored").join("record");
        assert_eq!(holding(&record, "zanzibar"), Vec::<PathBuf>::new());
        let m = Memory::open(&site.at("restored")).unwrap();
        let stub = &m.transcripts().await.unwrap()[0];
        assert_eq!(
            (stub.session.as_str(), stub.state),
            (kept.session.as_str(), State::Erased)
        );
        let receipt = m.transcript_erasure(&kept.session).await.unwrap().unwrap();
        assert_eq!(receipt.reason, Reason::Sensitive);
        // the drill takes the receipts of its own root
        let drill = site.m.drill(&site.at("bundle"), &site.at("scratch")).await;
        drill.unwrap();
        let scratch = site.at("scratch").join("record");
        assert_eq!(holding(&scratch, "zanzibar"), Vec::<PathBuf>::new());
    }

    /// A harness reaches a session of the owner that the memory server serves, so an
    /// agent may have asked: such a session makes no bundle, runs no drill and reads no
    /// journal.
    #[tokio::test]
    async fn a_served_session_of_the_owner_makes_no_bundle_and_runs_no_drill() {
        let site = Site::new();
        site.bundle("bundle").await;
        let served = Memory::open(&site.root()).unwrap();
        served.mark_served();
        let bundle = served.recovery_bundle(&site.at("served-bundle")).await;
        assert!(matches!(bundle, Err(Error::Denied(_))));
        assert!(!site.at("served-bundle").exists());
        let drill = served.drill(&site.at("bundle"), &site.at("scratch")).await;
        assert!(matches!(drill, Err(Error::Denied(_))));
        assert!(!site.at("scratch").exists());
        assert!(!site.root().join(EPOCH_FILE).exists());
        assert!(served.erasure_journal().await.is_err());
        assert!(served.transcript_journal().await.is_err());
    }

    #[tokio::test]
    async fn each_restore_takes_the_next_epoch_from_the_file_that_no_bundle_holds() {
        let site = Site::new();
        with_work(&site).await;
        site.bundle("bundle").await;
        assert_eq!(site.restore("bundle", "first", &[]).await.epoch, 1);
        assert_eq!(site.restore("bundle", "second", &[]).await.epoch, 2);
        assert_eq!(
            std::fs::read_to_string(site.at("claim-epoch")).unwrap(),
            "2\n"
        );
        std::fs::write(site.at("claim-epoch"), "7\n").unwrap();
        let third = site.restore("bundle", "third", &[]).await;
        assert_eq!(third.epoch, 8);
        let m = Memory::open(&site.at("third")).unwrap();
        assert_eq!(m.ledger.lock().await.epoch().await.unwrap(), 8);
        // a bundle of a root in epoch 8 restores into a later one, whatever the file says
        m.recovery_bundle(&site.at("later")).await.unwrap();
        std::fs::write(site.at("claim-epoch"), "3\n").unwrap();
        assert_eq!(site.restore("later", "fourth", &[]).await.epoch, 9);
    }

    #[test]
    fn the_epoch_file_gives_each_number_once_and_refuses_what_is_no_number() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("authority").join("claim-epoch");
        assert_eq!(next_epoch(&file, 0).unwrap(), 1);
        assert_eq!(next_epoch(&file, 0).unwrap(), 2);
        assert_eq!(next_epoch(&file, 5).unwrap(), 6);
        assert_eq!(next_epoch(&file, 2).unwrap(), 7);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "7\n");
        std::fs::write(&file, "seven\n").unwrap();
        assert!(matches!(next_epoch(&file, 0), Err(Error::Denied(_))));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "seven\n");
    }

    /// Whether the text of a version is the stub of an erased entry.
    fn is_stub(text: &str) -> bool {
        Entry::parse(text).unwrap().front.status == Some(Status::Erased)
    }

    /// An entry with two versions in the record, the second through the log.
    async fn with_two_versions(m: &Memory) -> String {
        let id = m
            .write(req("# Port\n\nIt listens on 8080.\n"))
            .await
            .unwrap()
            .id;
        let mut entry = m.get(&id).await.unwrap();
        entry.body = "# Port\n\nIt listens on 8081 now.\n".into();
        m.put_logged(&entry).await.unwrap();
        m.feed(None).await.unwrap();
        assert_eq!(m.store.history(DOMAIN, &id).await.unwrap().len(), 1);
        id
    }

    #[tokio::test]
    async fn an_entry_erased_after_the_bundle_is_erased_in_the_restored_root() {
        let site = Site::new();
        // written before the session reads the entry that is erased later: an entry
        // written after that read is downstream of it, and the erasure withholds it
        let other = site.m.write(req("# Host\n\nIt runs on basalt.\n")).await;
        let other = other.unwrap().id;
        let id = with_two_versions(&site.m).await;
        site.bundle("bundle").await;
        site.m.erase(&id, Reason::Requested).await.unwrap();
        let journal = site.m.erasure_journal().await.unwrap();
        assert_eq!(journal.len(), 1);
        let restored = site.restore("bundle", "with-journal", &journal).await;
        assert_eq!(restored.erased, std::slice::from_ref(&id));
        let m = Memory::open(&site.at("with-journal")).unwrap();
        assert!(is_stub(
            &m.store.get_text(DOMAIN, &id).await.unwrap().unwrap()
        ));
        let history = m.store.history(DOMAIN, &id).await.unwrap();
        assert!(!history.is_empty());
        assert!(history.iter().all(|v| is_stub(&v.text)), "{history:?}");
        assert!(found(&m, "8081").await.is_empty());
        assert!(found(&m, "8080").await.is_empty());
        assert_eq!(found(&m, "basalt").await, [other]);
        let receipt = m.receipt(&id).await.unwrap().unwrap();
        assert_eq!(receipt.reason, Reason::Requested);
        assert!(receipt.is_finished());
        // without the journal the bundle serves what it held: the journal is what erases
        site.restore("bundle", "without-journal", &[]).await;
        let m = Memory::open(&site.at("without-journal")).unwrap();
        assert_eq!(found(&m, "8081").await, [id]);
    }

    #[tokio::test]
    async fn a_receipt_of_an_entry_the_bundle_does_not_hold_is_kept_beside_the_record() {
        let site = Site::new();
        site.bundle("bundle").await;
        let id = site.m.write(req("# Port\n\nIt listens on 8080.\n")).await;
        let id = id.unwrap().id;
        site.m.erase(&id, Reason::Mistaken).await.unwrap();
        let journal = site.m.erasure_journal().await.unwrap();
        let restored = site.restore("bundle", "restored", &journal).await;
        assert_eq!(
            (restored.erased.len(), &restored.receipts_kept),
            (0, &vec![id.clone()])
        );
        let m = Memory::open(&site.at("restored")).unwrap();
        assert_eq!(m.receipt(&id).await.unwrap().unwrap(), journal[0]);
    }

    /// Give the live root a second domain, and write one entry there. Returns the id.
    async fn second_domain(site: &Site) -> String {
        let mut grants = Grants::parse(&site.grants()).unwrap();
        let first = grants.domains[DOMAIN].clone();
        grants.domains.insert("dom-two".into(), first);
        std::fs::write(site.root().join(GRANTS_FILE), grants.render().unwrap()).unwrap();
        std::fs::create_dir_all(site.root().join("record/dom-two")).unwrap();
        let wide = Memory::open(&site.root()).unwrap();
        let request = WriteRequest {
            domain: Some("dom-two".into()),
            ..req("# Vault\n\nThe vault is in granite.\n")
        };
        wide.write(request).await.unwrap().id
    }

    #[tokio::test]
    async fn the_restored_root_has_the_grants_in_force_and_not_the_grants_of_the_bundle() {
        let site = Site::new();
        let narrow = site.grants();
        let kept = site.m.write(req("# Host\n\nIt runs on basalt.\n")).await;
        let kept = kept.unwrap().id;
        let two = second_domain(&site).await;
        let wide = Memory::open(&site.root()).unwrap();
        wide.recovery_bundle(&site.at("bundle")).await.unwrap();
        wide.erase(&two, Reason::Legal).await.unwrap();
        let journal = wide.erasure_journal().await.unwrap();
        // the grant of the second domain ended after the bundle was made
        let epoch_file = site.at("claim-epoch");
        let authority = Authority {
            grants: &narrow,
            registry: None,
            journal: &journal,
            refusals: &[],
            transcripts: &[],
            drift_windows: None,
            epoch_file: &epoch_file,
            file_ledger: false,
        };
        let into = site.at("restored");
        let restored = restore(&site.at("bundle"), &into, &authority).await;
        assert_eq!(restored.unwrap().erased, std::slice::from_ref(&two));
        let m = Memory::open(&into).unwrap();
        assert_eq!(m.domains(), [DOMAIN]);
        assert_eq!(
            std::fs::read_to_string(into.join(GRANTS_FILE)).unwrap(),
            narrow
        );
        assert!(m.get(&two).await.is_err());
        assert!(found(&m, "granite").await.is_empty());
        assert_eq!(found(&m, "basalt").await, [kept]);
        // the grants of the bundle are there to read, and name the domain
        let old = into.join(RESTORED_CONFIG).join(GRANTS_FILE);
        assert!(std::fs::read_to_string(old).unwrap().contains("dom-two"));
        // the erasure of the journal reached the entry that no session reads
        let text = std::fs::read_to_string(into.join(format!("record/dom-two/{two}.md")));
        assert!(is_stub(&text.unwrap()));
    }

    #[tokio::test]
    async fn a_restore_into_a_file_ledger_leaves_the_ledger_table_out_of_the_grants() {
        let site = Site::new();
        site.bundle("bundle").await;
        let with_server = format!(
            "{}\n[ledger]\nurl = \"postgres://nobody@127.0.0.1:1/none\"\n",
            site.grants()
        );
        let epoch_file = site.at("claim-epoch");
        let authority = Authority {
            grants: &with_server,
            registry: None,
            journal: &[],
            refusals: &[],
            transcripts: &[],
            drift_windows: None,
            epoch_file: &epoch_file,
            file_ledger: true,
        };
        let into = site.at("restored");
        restore(&site.at("bundle"), &into, &authority)
            .await
            .unwrap();
        let written = std::fs::read_to_string(into.join(GRANTS_FILE)).unwrap();
        assert!(Grants::parse(&written).unwrap().ledger.is_none());
        assert!(into.join(crate::LEDGER_FILE).exists());
    }

    /// A copy of the bundle `from` under the name `to`, for a test that changes it.
    fn copy_of(site: &Site, from: &str, to: &str) -> PathBuf {
        copy_tree(&site.at(from), &site.at(to)).unwrap();
        site.at(to)
    }

    #[tokio::test]
    async fn a_bundle_that_is_not_whole_is_refused_and_nothing_is_written() {
        let site = Site::new();
        let ([port, _], ..) = with_work(&site).await;
        let manifest = site.bundle("bundle").await;
        let entry = format!("record/{DOMAIN}/{port}.md");
        assert!(manifest.files.iter().any(|f| f.path == entry));

        let changed = copy_of(&site, "bundle", "changed");
        let text = std::fs::read_to_string(changed.join(&entry)).unwrap();
        std::fs::write(changed.join(&entry), text.replace("8080", "9090")).unwrap();
        let why = site.refused("changed", "into-changed").await.to_string();
        assert!(why.contains("digest"), "{why}");
        assert!(!site.at("into-changed").exists());

        let missing = copy_of(&site, "bundle", "missing");
        std::fs::remove_file(missing.join(LEDGER_EVENTS)).unwrap();
        site.refused("missing", "into-missing").await;
        assert!(!site.at("into-missing").exists());

        let outside = copy_of(&site, "bundle", "outside");
        let mut escaping = manifest.clone();
        escaping.files[0].path = "../x".into();
        std::fs::write(outside.join(format::FILE), escaping.to_yaml()).unwrap();
        let why = site.refused("outside", "into-outside").await.to_string();
        assert!(why.contains("does not stay in the bundle"), "{why}");
        assert!(!site.at("into-outside").exists());

        let unknown = copy_of(&site, "bundle", "unknown");
        let mut extra = manifest.clone();
        extra.files[0].path = GRANTS_FILE.into();
        std::fs::write(unknown.join(format::FILE), extra.to_yaml()).unwrap();
        let why = site.refused("unknown", "into-unknown").await.to_string();
        assert!(why.contains("no part of a bundle"), "{why}");
        assert!(!site.at("into-unknown").join(GRANTS_FILE).exists());
        // and the bundle as it was made restores
        site.restore("bundle", "whole", &[]).await;
    }

    #[tokio::test]
    async fn a_restore_is_refused_into_a_directory_that_holds_something() {
        let site = Site::new();
        site.bundle("bundle").await;
        let why = site.refused("bundle", "live").await.to_string();
        assert!(why.contains(GRANTS_FILE), "{why}");
        std::fs::create_dir_all(site.at("used")).unwrap();
        std::fs::write(site.at("used").join("note.txt"), "here").unwrap();
        let why = site.refused("bundle", "used").await.to_string();
        assert!(why.contains("not empty"), "{why}");
    }

    #[tokio::test]
    async fn a_root_that_a_restore_did_not_finish_is_not_opened() {
        let site = Site::new();
        site.m
            .write(req("# Port\n\nIt listens on 8080.\n"))
            .await
            .unwrap();
        std::fs::write(site.root().join(RESTORING), "").unwrap();
        let refused = Memory::open(&site.root());
        let why = refused.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(
            why.contains("a restore of this root did not finish"),
            "{why}"
        );
        std::fs::remove_file(site.root().join(RESTORING)).unwrap();
        Memory::open(&site.root()).unwrap();
    }

    #[tokio::test]
    async fn a_restore_that_stops_leaves_a_root_that_is_not_opened() {
        let site = Site::new();
        site.bundle("bundle").await;
        // an epoch file that holds no number stops the restore after the files are laid out
        std::fs::write(site.at("claim-epoch"), "not a number\n").unwrap();
        site.refused("bundle", "stopped").await;
        assert!(site.at("stopped").join(GRANTS_FILE).exists());
        assert!(matches!(
            Memory::open(&site.at("stopped")),
            Err(Error::Denied(_))
        ));
    }

    /// What a writer leaves that stopped between the log and the index: the `put` of a
    /// version in the log, and the version in the record.
    async fn put_without_feed(m: &Memory, id: &str, body: &str) {
        let entry = Entry::parse(&format!(
            "---\nid: {id}\ntype: fact\ndomain: {DOMAIN}\nauthor: {OWNER}\n\
             created: 2026-10-01T09:00:00Z\ntrust_tier: raw\nclassification: internal\n---\n{body}"
        ))
        .unwrap();
        let put = log::Change::Put {
            id: id.into(),
            digest: dm_core::digest(&entry.render()),
            active: true,
        };
        log::append(m.root(), DOMAIN, OWNER, put).unwrap();
        m.store.put(&entry).await.unwrap();
    }

    fn outcome(drill: &Drill, name: &str) -> Outcome {
        let found = drill.cases.iter().find(|c| c.name == name);
        found.unwrap_or_else(|| panic!("no case {name}")).outcome
    }

    #[tokio::test]
    async fn a_version_that_the_index_did_not_hold_is_found_after_the_restore() {
        let site = Site::new();
        site.m
            .write(req("# Port\n\nIt listens on 8080.\n"))
            .await
            .unwrap();
        put_without_feed(&site.m, "mem-me-early001", "Stranded before the feed.\n").await;
        assert!(
            found(&site.m, "Stranded").await.is_empty(),
            "the index is behind"
        );
        let manifest = site.bundle("bundle").await;
        let at = &manifest.domains[0];
        assert!(at.index_fed < at.log_seq, "{at:?}");
        site.restore("bundle", "restored", &[]).await;
        let m = Memory::open(&site.at("restored")).unwrap();
        assert_eq!(found(&m, "Stranded").await, ["mem-me-early001"]);
        assert_eq!(m.freshness(None).unwrap().unfed, 0);
        let drill = site.m.drill(&site.at("bundle"), &site.at("scratch")).await;
        let drill = drill.unwrap();
        assert_eq!(outcome(&drill, AWAITING), Outcome::Recovered);
        assert!(drill.passed);
    }

    #[tokio::test]
    async fn a_drill_reports_a_case_that_the_bundle_did_not_hold_as_absent() {
        let site = Site::new();
        site.m
            .write(req("# Port\n\nIt listens on 8080.\n"))
            .await
            .unwrap();
        let open = admitted(&site.m, "Rotate the key").await;
        // a read after the last feed: the log is past the cursor, and names no version
        site.m.search("8080", None, 10).await.unwrap();
        let manifest = site.bundle("bundle").await;
        assert!(manifest.domains[0].index_fed < manifest.domains[0].log_seq);
        let drill = site.m.drill(&site.at("bundle"), &site.at("scratch")).await;
        let drill = drill.unwrap();
        let names: Vec<&str> = drill.cases.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            [
                UNFINISHED_TASK,
                ERASURE_AFTER_BACKUP,
                AWAITING,
                "cross-domain-dependency",
                UNCERTAIN_ACTION
            ]
        );
        for c in &drill.cases {
            assert_eq!(c.outcome, Outcome::Absent, "{c:?}");
        }
        assert!(drill.passed);
        assert_eq!(drill.epoch, 1);
        // the restored root still has the task
        let m = Memory::open(&site.at("scratch")).unwrap();
        assert_eq!(open_tasks(&m).await, [open]);
    }

    #[tokio::test]
    async fn a_drill_recovers_what_the_bundle_held_and_writes_only_the_epoch_file() {
        let site = Site::new();
        let ([port, host], open, (held, generation)) = with_work(&site).await;
        site.bundle("bundle").await;
        site.m.erase(&port, Reason::Requested).await.unwrap();
        let events = site.m.ledger.lock().await.checkpoint().await.unwrap();
        let log_before = log::read(&site.root(), DOMAIN, 0).unwrap();
        let drill = site.m.drill(&site.at("bundle"), &site.at("scratch")).await;
        let drill = drill.unwrap();
        // the live root: its ledger, its log, its entries and its tasks are as they were
        let after = site.m.ledger.lock().await.checkpoint().await.unwrap();
        assert_eq!(after, events);
        assert_eq!(log::read(&site.root(), DOMAIN, 0).unwrap(), log_before);
        assert_eq!(outcome(&drill, UNFINISHED_TASK), Outcome::Recovered);
        assert_eq!(outcome(&drill, ERASURE_AFTER_BACKUP), Outcome::Recovered);
        assert_eq!(outcome(&drill, AWAITING), Outcome::Absent);
        assert!(drill.passed, "{drill:?}");
        let live = Memory::open(&site.root()).unwrap();
        assert_eq!(live.get(&host).await.unwrap().front.id, host);
        assert_eq!(open_tasks(&live).await, [open]);
        let claimed = live.tasks(None, TaskStatus::Claimed).await.unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(
            (claimed[0].id.as_str(), claimed[0].generation),
            (held.as_str(), generation)
        );
        assert_eq!(live.ledger.lock().await.epoch().await.unwrap(), 0);
        assert_eq!(
            std::fs::read_to_string(site.root().join(EPOCH_FILE)).unwrap(),
            "1\n"
        );
        // the next drill is in the next epoch, and needs an empty place
        let again = site.m.drill(&site.at("bundle"), &site.at("scratch")).await;
        assert!(matches!(again, Err(Error::Denied(_))));
        let next = site
            .m
            .drill(&site.at("bundle"), &site.at("scratch-2"))
            .await;
        assert_eq!(next.unwrap().epoch, 2);
    }

    /// The holder of a task began an external action and did not settle it. After the
    /// restore its claim is over, and the task is held back until the effect is settled.
    #[tokio::test]
    async fn a_drill_recovers_an_effect_that_nobody_had_settled() {
        let site = Site::new();
        let (_, _, (held, generation)) = with_work(&site).await;
        let worker = Memory::open(&site.root()).unwrap().with_author(WORKER);
        worker
            .effect_begun(&held, Some(generation), "act-1")
            .await
            .unwrap();
        site.bundle("bundle").await;
        let drill = site.m.drill(&site.at("bundle"), &site.at("scratch")).await;
        let drill = drill.unwrap();
        assert_eq!(outcome(&drill, UNCERTAIN_ACTION), Outcome::Recovered);
        assert_eq!(outcome(&drill, UNFINISHED_TASK), Outcome::Recovered);
        assert!(drill.passed, "{drill:?}");
        // in the restored root the task is nobody's, and nobody claims it
        let restored = Memory::open(&site.at("scratch")).unwrap();
        let task = restored.get_task(&held).await.unwrap();
        assert_eq!(
            (task.holder.as_deref(), &task.effects[..]),
            (None, &["act-1".to_string()][..])
        );
        let other = Memory::open(&site.at("scratch"))
            .unwrap()
            .with_author("agent:b");
        let refused = other.claim(&held, Duration::minutes(5)).await;
        assert!(
            matches!(
                refused,
                Err(Error::Ledger(dm_ledger::Error::WrongState { .. }))
            ),
            "{:?}",
            refused.map(|c| c.generation)
        );
        // a person settles the effect, and the task is on offer again
        restored
            .effect_settled(&held, None, "act-1", dm_ledger::Outcome::NotApplied)
            .await
            .unwrap();
        other.claim(&held, Duration::minutes(5)).await.unwrap();
    }

    #[tokio::test]
    async fn an_agent_makes_no_bundle_and_runs_no_drill_and_reads_no_journal() {
        let site = Site::new();
        site.bundle("bundle").await;
        let agent = Memory::open(&site.root()).unwrap().with_author(WORKER);
        let bundle = agent.recovery_bundle(&site.at("agent-bundle")).await;
        assert!(matches!(bundle, Err(Error::Denied(_))));
        assert!(!site.at("agent-bundle").exists());
        let drill = agent.drill(&site.at("bundle"), &site.at("scratch")).await;
        assert!(matches!(drill, Err(Error::Denied(_))));
        assert!(!site.at("scratch").exists());
        assert!(!site.root().join(EPOCH_FILE).exists());
        assert!(matches!(
            agent.erasure_journal().await,
            Err(Error::Denied(_))
        ));
    }

    #[tokio::test]
    async fn a_bundle_goes_into_an_empty_place_and_names_itself_in_the_log() {
        let site = Site::new();
        let id = site.m.write(req("# Port\n\nIt listens on 8080.\n")).await;
        let id = id.unwrap().id;
        let again = site.m.recovery_bundle(&site.root()).await;
        assert!(matches!(again, Err(Error::Denied(_))));
        let before = log::read(&site.root(), DOMAIN, 0).unwrap().len();
        let manifest = site.bundle("bundle").await;
        let after = log::read(&site.root(), DOMAIN, 0).unwrap();
        let sent: Vec<&log::Event> = after[before..]
            .iter()
            .filter(
                |e| matches!(&e.change, log::Change::Send { to, .. } if to == "recovery:bundle"),
            )
            .collect();
        assert_eq!(sent.len(), 1, "{:?}", &after[before..]);
        // the copy of the log in the bundle ends before that send
        assert_eq!(manifest.domains[0].log_seq, before as u64);
        // an erasure made later names the bundle as a copy it cannot reach
        let receipt = site.m.erase(&id, Reason::Requested).await.unwrap();
        assert!(
            receipt.sends.iter().any(|s| s.to == "recovery:bundle"),
            "{:?}",
            receipt.sends
        );
        assert_eq!(
            recovery_dir(&site.root(), "2026-11-24"),
            site.root().join("state/recovery/2026-11-24")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_symbolic_link_in_the_record_is_refused() {
        let site = Site::new();
        let outside = site.at("secret.md");
        std::fs::write(&outside, "outside the root").unwrap();
        let link = site.root().join(format!("record/{DOMAIN}/link.md"));
        std::os::unix::fs::symlink(&outside, link).unwrap();
        let refused = site.m.recovery_bundle(&site.at("bundle")).await;
        let why = refused.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(why.contains("symbolic link"), "{why}");
    }

    #[tokio::test]
    async fn the_journal_is_kept_as_a_file_for_each_receipt_and_read_back() {
        let site = Site::new();
        let id = site.m.write(req("# Port\n\nIt listens on 8080.\n")).await;
        let id = id.unwrap().id;
        site.m.erase(&id, Reason::Sensitive).await.unwrap();
        let journal = site.m.erasure_journal().await.unwrap();
        let dir = site.at("journal");
        assert!(read_journal(&dir).unwrap().is_empty());
        write_journal(&dir, &journal).unwrap();
        assert!(dir.join(DOMAIN).join(format!("{id}.yaml")).exists());
        assert_eq!(read_journal(&dir).unwrap(), journal);
        // a receipt holds digests and no body
        let text = std::fs::read_to_string(dir.join(DOMAIN).join(format!("{id}.yaml")));
        assert!(!text.unwrap().contains("8080"));
        // a name that is no name does not become a path
        let mut bad = journal[0].clone();
        bad.id = "../escape".into();
        assert!(matches!(write_journal(&dir, &[bad]), Err(Error::Denied(_))));
        assert!(!site.at("escape.yaml").exists());
        // a receipt in the place of another is refused
        let moved = dir.join(DOMAIN).join("mem-me-other.yaml");
        std::fs::rename(dir.join(DOMAIN).join(format!("{id}.yaml")), moved).unwrap();
        assert!(matches!(read_journal(&dir), Err(Error::Denied(_))));
    }

    const PROPOSAL: &str = "# Commits\n\nThe person prefers small commits that a reviewer reads.\n";

    /// The tasks of proposals to a personal domain that wait for the person.
    async fn proposal_tasks(m: &Memory) -> usize {
        let mut n = 0;
        for status in [TaskStatus::Proposed, TaskStatus::Open] {
            let tasks = m.tasks(None, status).await.unwrap();
            let of_proposals = tasks
                .iter()
                .filter(|t| t.spec.origin.as_deref() == Some(crate::registry::PROPOSAL_ORIGIN));
            n += of_proposals.count();
        }
        n
    }

    #[tokio::test]
    async fn a_proposal_refused_after_the_bundle_is_removed_in_the_restored_root() {
        let site = Site::new();
        // the owner of the grants is the person, by an assertion
        let person = "human:test,2026-11-01:me";
        let registry = format!(
            "[domains.{DOMAIN}]\nperson = \"{person}\"\n[[assertions]]\nwas = \"{OWNER}\"\n\
             is = \"{person}\"\nsince = 2026-11-01\n"
        );
        std::fs::write(site.root().join(REGISTRY_FILE), &registry).unwrap();
        let owner = Memory::open(&site.root()).unwrap();
        let offer = |root: PathBuf| async move {
            let agent = Memory::open(&root).unwrap().with_author(WORKER);
            agent.write(req(PROPOSAL)).await.unwrap_err().to_string()
        };
        assert!(offer(site.root()).await.contains("held as a proposal"));
        let id = owner.proposals().unwrap()[0].front.id.clone();
        owner.recovery_bundle(&site.at("bundle")).await.unwrap();
        owner.answer_proposal(&id, false).await.unwrap();
        // the journal holds the refusal as a key, and is read back from its file
        let refusals = owner.refusal_journal().unwrap();
        assert_eq!(refusals.len(), 1);
        let kept = site.at("journal");
        assert!(read_refusals(&kept).unwrap().is_empty());
        write_refusals(&kept, &refusals).unwrap();
        assert_eq!(read_refusals(&kept).unwrap(), refusals);
        assert!(read_journal(&kept).unwrap().is_empty());
        let text = std::fs::read_to_string(kept.join(DOMAIN).join(REFUSALS_FILE)).unwrap();
        assert!(!text.contains("small commits"), "{text}");
        let (grants, epoch_file) = (site.grants(), site.at("claim-epoch"));
        let authority = Authority {
            grants: &grants,
            registry: Some(&registry),
            journal: &[],
            refusals: &refusals,
            transcripts: &[],
            drift_windows: None,
            epoch_file: &epoch_file,
            file_ledger: false,
        };
        let into = site.at("restored");
        let restored = restore(&site.at("bundle"), &into, &authority)
            .await
            .unwrap();
        assert_eq!(restored.proposals_refused, vec![id]);
        let back = Memory::open(&into).unwrap();
        assert!(back.proposals().unwrap().is_empty());
        assert_eq!(files_below(&into.join(QUARANTINE_DIR)).unwrap().len(), 0);
        assert_eq!(proposal_tasks(&back).await, 0);
        // offered again in the restored root: refused, and nothing is held
        let again = offer(into.clone()).await;
        assert!(again.contains("refused this proposal"), "{again}");
        assert!(back.proposals().unwrap().is_empty());
        // a drill takes the refusal journal of its root
        let scratch = site.at("scratch");
        owner.drill(&site.at("bundle"), &scratch).await.unwrap();
        let drilled = Memory::open(&scratch).unwrap();
        assert!(drilled.proposals().unwrap().is_empty());
    }

    #[test]
    fn a_refusal_journal_takes_a_digest_in_the_directory_of_a_domain_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let refusal = |domain: &str, key: &str| Refusal {
            domain: domain.into(),
            key: key.into(),
        };
        let key = "a".repeat(64);
        for bad in [refusal("../escape", &key), refusal(DOMAIN, "../../x")] {
            let err = write_refusals(dir.path(), &[bad]).unwrap_err();
            assert!(matches!(err, Error::Denied(_)), "{err}");
        }
        assert_eq!(files_below(dir.path()).unwrap().len(), 0);
        write_file(&dir.path().join(DOMAIN).join(REFUSALS_FILE), b"not a key\n").unwrap();
        let err = read_refusals(dir.path()).unwrap_err();
        assert!(err.to_string().contains("is not the key"), "{err}");
    }
}
