//! The FUSE filesystem over a render, and what the host needs for it.
//!
//! One thread serves the kernel's requests; the gets run on the launcher's runtime,
//! blocked on from that thread, so a get here is the same call a `get` tool makes.

use crate::{classify, Error, Kind};
use dm_server::Memory;
use fuser::{
    Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo,
    LockOwner, MountOption, OpenAccMode, OpenFlags, ReplyAttr, ReplyData, ReplyDirectory,
    ReplyEntry, ReplyOpen, Request,
};
use serde_json::json;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How long the kernel may trust an answer about a name or an attribute. The render
/// refreshes after a write through this service, so a listing is a second behind at most.
const TTL: Duration = Duration::from_secs(1);

/// The device the kernel's FUSE driver is reached through.
const DEVICE: &str = "/dev/fuse";

/// The setuid helpers an unprivileged mount goes through, newest first.
const HELPERS: [&str; 2] = ["fusermount3", "fusermount"];

/// A system binary on the PATH, as the mount helper itself would be found.
fn on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

pub(crate) fn availability() -> Result<(), String> {
    match std::fs::metadata(DEVICE) {
        Ok(m) if m.file_type().is_char_device() => {}
        Ok(_) => return Err(format!("{DEVICE} is not the FUSE device")),
        Err(e) => return Err(format!("{DEVICE}: {e}; the projection is a render here")),
    }
    if HELPERS.iter().any(|h| on_path(h).is_some()) {
        Ok(())
    } else {
        Err(format!(
            "neither {} nor {} is on PATH, so an unprivileged mount cannot be made; the \
             projection is a render here",
            HELPERS[0], HELPERS[1]
        ))
    }
}

/// Whether `at` is a mount whose server is gone: a killed launcher leaves one, and every
/// call on it fails with "transport endpoint is not connected". The server is the
/// launcher, so this is the record of whether its launcher lives. A link at `at` is not
/// followed, and is not a dead mount.
fn is_dead(at: &Path) -> bool {
    if std::fs::symlink_metadata(at).is_ok_and(|m| m.file_type().is_symlink()) {
        return false;
    }
    // an open, which always reaches the server: the kernel answers a stat from its
    // cache for as long as the attributes' TTL after the server has gone
    std::fs::read_dir(at)
        .err()
        .is_some_and(|e| e.raw_os_error() == Some(libc_enotconn()))
}

/// `ENOTCONN` without a binding crate: the value is the same on every Linux target.
const fn libc_enotconn() -> i32 {
    107
}

/// Unmount a dead mount at `at`, through the helper, lazily, so a directory that was
/// a mountpoint can be removed. Nothing is done to a mount that answers.
pub(crate) fn clear_dead(at: &Path) {
    if !is_dead(at) {
        return;
    }
    for helper in HELPERS {
        let Some(bin) = on_path(helper) else {
            continue;
        };
        let done = std::process::Command::new(bin)
            .arg("-uz")
            .arg(at)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if done {
            return;
        }
    }
}

/// Mount `memory`'s view over `render` at `at`, serving from a thread of its own.
pub(crate) fn serve(
    memory: Arc<Memory>,
    render: &Path,
    at: &Path,
) -> Result<fuser::BackgroundSession, Error> {
    let rt = tokio::runtime::Handle::try_current().map_err(|e| {
        Error::Unavailable(format!("no runtime to serve the mount's gets from: {e}"))
    })?;
    let view = View::new(memory, render.to_path_buf(), rt);
    let mut cfg = Config::default();
    cfg.mount_options.extend([
        MountOption::RO,
        MountOption::NoSuid,
        MountOption::NoDev,
        MountOption::NoAtime,
        MountOption::FSName("amem".to_string()),
        MountOption::Subtype("memory".to_string()),
    ]);
    fuser::spawn_mount(view, at, &cfg).map_err(|source| Error::Mount {
        at: at.display().to_string(),
        source,
    })
}

/// Inode numbers for the paths the kernel has been told about, stable for the session.
#[derive(Default)]
struct Nodes {
    paths: HashMap<u64, PathBuf>,
    inos: HashMap<PathBuf, u64>,
    next: u64,
}

impl Nodes {
    fn new() -> Nodes {
        let mut n = Nodes {
            next: 2,
            ..Nodes::default()
        };
        n.paths.insert(1, PathBuf::new());
        n.inos.insert(PathBuf::new(), 1);
        n
    }

    fn ino_of(&mut self, rel: &Path) -> u64 {
        if let Some(&i) = self.inos.get(rel) {
            return i;
        }
        let i = self.next;
        self.next += 1;
        self.paths.insert(i, rel.to_path_buf());
        self.inos.insert(rel.to_path_buf(), i);
        i
    }

    fn path_of(&self, ino: u64) -> Option<PathBuf> {
        self.paths.get(&ino).cloned()
    }
}

/// The view: the render's shape, the memory's word.
struct View {
    memory: Arc<Memory>,
    render: PathBuf,
    rt: tokio::runtime::Handle,
    session: String,
    nodes: Mutex<Nodes>,
    /// What each open file handle serves: the entry's text as the get returned it, or
    /// the render's bytes for a free page.
    open: Mutex<HashMap<u64, Arc<Vec<u8>>>>,
    next_fh: AtomicU64,
}

/// What the render holds at a path the view shows.
struct Found {
    kind: Kind,
    meta: std::fs::Metadata,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl View {
    fn new(memory: Arc<Memory>, render: PathBuf, rt: tokio::runtime::Handle) -> View {
        View {
            session: memory.session_id().to_string(),
            memory,
            render,
            rt,
            nodes: Mutex::new(Nodes::new()),
            open: Mutex::new(HashMap::new()),
            next_fh: AtomicU64::new(1),
        }
    }

    /// What the view shows at `rel`, if the render holds it in the shape the kind needs:
    /// a real directory for a directory, a real file for a page or an entry, never a
    /// link, which the render refuses to follow as well.
    fn look(&self, rel: &Path) -> Option<Found> {
        let kind = classify(rel, &self.session)?;
        let meta = std::fs::symlink_metadata(self.render.join(rel)).ok()?;
        let fits = match kind {
            Kind::Dir => meta.is_dir(),
            Kind::Free | Kind::Entry(_) => meta.is_file(),
        };
        fits.then_some(Found { kind, meta })
    }

    fn attr(&self, ino: u64, f: &Found) -> FileAttr {
        let dir = f.kind == Kind::Dir;
        let when = |t: std::io::Result<SystemTime>| t.unwrap_or(UNIX_EPOCH);
        let size = if dir { 0 } else { f.meta.len() };
        FileAttr {
            ino: INodeNo(ino),
            size,
            blocks: size.div_ceil(512),
            atime: when(f.meta.accessed()),
            mtime: when(f.meta.modified()),
            ctime: when(f.meta.modified()),
            crtime: when(f.meta.created()),
            kind: if dir {
                FileType::Directory
            } else {
                FileType::RegularFile
            },
            perm: if dir { 0o555 } else { 0o444 },
            nlink: 1,
            uid: f.meta.uid(),
            gid: f.meta.gid(),
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

    /// The get an open of entry `id` is: the memory's own, through the session's filter,
    /// kept on the tape as a `get` call so a re-run sees the read. Returns the entry's
    /// on-disk text, which is what the render would have held.
    async fn get(&self, id: &str) -> Result<Vec<u8>, Errno> {
        let args = json!({ "id": id });
        match self.memory.get(id).await {
            Ok(e) => {
                let f = &e.front;
                let v = json!({
                    "id": f.id,
                    "domain": f.domain,
                    "type": f.entry_type.to_string(),
                    "classification": f.classification.to_string(),
                    "title": e.title(),
                    "body": e.body,
                    "via": "file",
                });
                self.memory.tape_call("get", &args, &v, false).await.ok();
                Ok(e.render().into_bytes())
            }
            Err(err) => {
                let why = err.to_string();
                self.memory
                    .tape_call("get", &args, &json!({ "error": why }), true)
                    .await
                    .ok();
                Err(match err {
                    dm_server::Error::NotFound(_) | dm_server::Error::Denied(_) => Errno::EACCES,
                    dm_server::Error::Moved { .. } | dm_server::Error::Gone(_) => Errno::ENOENT,
                    _ => Errno::EIO,
                })
            }
        }
    }

    fn handle(&self, bytes: Vec<u8>) -> FileHandle {
        let fh = self.next_fh.fetch_add(1, Ordering::SeqCst);
        lock(&self.open).insert(fh, Arc::new(bytes));
        FileHandle(fh)
    }
}

impl Filesystem for View {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Some(dir) = lock(&self.nodes).path_of(parent.0) else {
            return reply.error(Errno::ENOENT);
        };
        let rel = dir.join(name);
        let Some(found) = self.look(&rel) else {
            return reply.error(Errno::ENOENT);
        };
        let ino = lock(&self.nodes).ino_of(&rel);
        reply.entry(&TTL, &self.attr(ino, &found), Generation(0));
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let Some(rel) = lock(&self.nodes).path_of(ino.0) else {
            return reply.error(Errno::ENOENT);
        };
        match self.look(&rel) {
            Some(found) => reply.attr(&TTL, &self.attr(ino.0, &found)),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let Some(rel) = lock(&self.nodes).path_of(ino.0) else {
            return reply.error(Errno::ENOENT);
        };
        if !self.look(&rel).is_some_and(|f| f.kind == Kind::Dir) {
            return reply.error(Errno::ENOTDIR);
        }
        let Ok(dir) = std::fs::read_dir(self.render.join(&rel)) else {
            return reply.error(Errno::EIO);
        };
        let mut listed: Vec<(u64, FileType, std::ffi::OsString)> = vec![
            (ino.0, FileType::Directory, ".".into()),
            (ino.0, FileType::Directory, "..".into()),
        ];
        for e in dir.flatten() {
            let child = rel.join(e.file_name());
            let Some(found) = self.look(&child) else {
                continue;
            };
            let kind = if found.kind == Kind::Dir {
                FileType::Directory
            } else {
                FileType::RegularFile
            };
            let i = lock(&self.nodes).ino_of(&child);
            listed.push((i, kind, e.file_name()));
        }
        // names in a stable order, so an offset means the same thing on the next call
        listed[2..].sort_by(|a, b| a.2.cmp(&b.2));
        for (i, (ino, kind, name)) in listed.into_iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(ino), i as u64 + 1, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        if flags.acc_mode() != OpenAccMode::O_RDONLY {
            return reply.error(Errno::EROFS);
        }
        let Some(rel) = lock(&self.nodes).path_of(ino.0) else {
            return reply.error(Errno::ENOENT);
        };
        let Some(found) = self.look(&rel) else {
            return reply.error(Errno::ENOENT);
        };
        let bytes = match found.kind {
            Kind::Dir => return reply.error(Errno::EISDIR),
            Kind::Free => match std::fs::read(self.render.join(&rel)) {
                Ok(b) => b,
                Err(_) => return reply.error(Errno::EIO),
            },
            Kind::Entry(id) => match self.rt.block_on(self.get(&id)) {
                Ok(b) => b,
                Err(errno) => return reply.error(errno),
            },
        };
        // direct I/O: the kernel reads until a short read rather than trusting the
        // size it was told, which is the render's and may differ from the record's
        reply.opened(self.handle(bytes), FopenFlags::FOPEN_DIRECT_IO);
    }

    #[allow(clippy::too_many_arguments)] // the trait's signature
    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let Some(bytes) = lock(&self.open).get(&fh.0).cloned() else {
            return reply.error(Errno::EBADF);
        };
        let from = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(bytes.len());
        let to = from.saturating_add(size as usize).min(bytes.len());
        reply.data(&bytes[from..to]);
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: fuser::ReplyEmpty,
    ) {
        lock(&self.open).remove(&fh.0);
        reply.ok();
    }
}
