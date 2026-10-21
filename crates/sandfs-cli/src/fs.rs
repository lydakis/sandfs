//! The mounted filesystem: kernel requests turned into workspace calls.
//!
//! The kernel addresses files by inode number and the workspace by path, so [`Inner`]
//! keeps a table between the two and moves entries when something is renamed. Files
//! are never opened or closed here: the kernel is told it needn't ask (no-open mode),
//! which saves two round trips per file, so reads and writes name only the inode.
//! Writes land in a per-inode buffer that reaches the workspace once the file has gone
//! [`SETTLE`] without a write, or on `fsync`, so a file written in many small chunks is
//! copied once, not once per chunk. A mount with saved state writes that buffer to disk,
//! as a new object of the workspace (a [`Draft`]), so a file being written takes no
//! memory however large it is, and a write in the middle of a large file copies it on
//! disk rather than reading it into memory. [`Shared`] adds a background thread that does that
//! and saves the workspace's changes, and the [`fuser::Filesystem`] impl at the bottom
//! only locks, calls into [`Inner`] and replies. The NFS server on macOS (`nfs`) calls
//! the same [`Inner`], which has no opens or closes either; fuser lends it the types
//! but never mounts there.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::ffi::OsStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use bytes::Bytes;
#[cfg(fuse)]
use fuser::{
    CopyFileRangeFlags, FileHandle, Filesystem, FopenFlags, Generation, InitFlags, KernelConfig, LockOwner, OpenFlags,
    ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty, ReplyEntry, ReplyOpen,
    ReplyStatfs, ReplyWrite, Request, WriteFlags,
};
use fuser::{Errno, FileAttr, FileType, INodeNo, RenameFlags, TimeOrNow};
use sandfs_core::{Error, Kind, VPath, Workspace};

use crate::persist::{Draft, Drafts, Saved, Snapshot};
use crate::store::subtree_keys;

/// The entry for a name that doesn't exist, which the kernel caches for [`TTL`].
#[cfg(fuse)]
fn missing() -> FileAttr {
    let never = SystemTime::UNIX_EPOCH;
    FileAttr {
        ino: INodeNo(0),
        size: 0,
        blocks: 0,
        atime: never,
        mtime: never,
        ctime: never,
        crtime: never,
        kind: FileType::RegularFile,
        perm: 0,
        nlink: 0,
        uid: 0,
        gid: 0,
        rdev: 0,
        blksize: 0,
        flags: 0,
    }
}

/// How long the kernel may cache attributes and names. Every change goes through this
/// filesystem, so the kernel already sees its own updates and nothing goes stale;
/// a short TTL would only make it ask again (a round trip per path, per second).
#[cfg(any(fuse, test))]
const TTL: Duration = Duration::from_secs(60);
pub const ROOT_INO: u64 = 1;
/// The largest file a mount takes: what a file offset can address. A file being
/// written is a file on disk, so the disk is the real limit.
pub const MAX_FILE_SIZE: usize = i64::MAX as usize;
/// The most bytes one `copy_file_range` call reads and writes when it can't share a
/// whole file; the caller asks again for the rest.
#[cfg_attr(not(fuse), allow(dead_code))] // macOS's NFS has no copy request
const COPY_PIECE: u64 = 8 << 20;
/// The largest file kept in memory while it is written, where there are no drafts.
const MAX_MEMORY_FILE: u64 = 4 << 30;
/// How many whole files, and bytes of them, [`Inner::reading`] keeps for sources that
/// can't read a range on its own. The kernel asks for a file in chunks and caches
/// what it gets, so only the few files being read right now are worth keeping.
const RECENT_FILES: usize = 16;
const RECENT_BYTES: usize = 256 << 20;
/// How long a file goes without a write before its buffer is committed and saved.
/// Nothing tells this filesystem when a file is closed, so a quiet spell stands in.
pub const SETTLE: Duration = Duration::from_millis(250);

/// How many listings of folders that weren't opened are kept going at once, for
/// kernels that list folders without opening them.
#[cfg(any(fuse, test))]
const LISTINGS_KEPT: usize = 64;

// renameat2's flags, which only Linux's FUSE passes (NFS has none). Swapping two paths,
// and overlay whiteouts, are not supported.
#[cfg(target_os = "linux")]
const RENAME_NOREPLACE: u32 = libc::RENAME_NOREPLACE;

#[cfg(target_os = "linux")]
fn rename_unsupported(flags: RenameFlags) -> bool {
    flags.bits() & (libc::RENAME_EXCHANGE | libc::RENAME_WHITEOUT) != 0
}

#[cfg(not(target_os = "linux"))]
fn rename_unsupported(_: RenameFlags) -> bool {
    false
}

#[cfg(target_os = "linux")]
fn rename_noreplace(flags: RenameFlags) -> bool {
    flags.bits() & RENAME_NOREPLACE != 0
}

#[cfg(not(target_os = "linux"))]
fn rename_noreplace(_: RenameFlags) -> bool {
    false
}

pub type FsResult<T> = Result<T, Errno>;

/// The errno for a workspace error. Unexpected errors are logged, since EIO alone says
/// nothing about what failed.
pub fn errno(err: &Error) -> Errno {
    match err {
        Error::NotFound(_) => Errno::ENOENT,
        Error::AlreadyExists(_) => Errno::EEXIST,
        Error::NotADirectory(_) => Errno::ENOTDIR,
        Error::IsADirectory(_) => Errno::EISDIR,
        Error::DirectoryNotEmpty(_) => Errno::ENOTEMPTY,
        Error::ReadOnly(_) => Errno::EROFS,
        Error::InvalidPath(_) => Errno::EINVAL,
        // An operation that would have to follow a link, as with O_NOFOLLOW.
        Error::Unsupported(_, reason) if reason.contains("symbolic link") => Errno::ELOOP,
        Error::Unsupported(..) => Errno::EPERM,
        other => {
            eprintln!("sandfs: {other}");
            Errno::EIO
        }
    }
}

/// A read [`Inner::reading`] started.
pub enum Reading {
    /// The bytes, read already.
    Read(Bytes),
    /// A saved file, to read from disk.
    Stored(sandfs_core::Stored),
}

impl Reading {
    /// The bytes asked for: `size` of them from `offset`, or fewer at the end.
    pub fn read(self, offset: u64, size: u32) -> FsResult<Bytes> {
        match self {
            Reading::Read(data) => Ok(data),
            Reading::Stored(stored) => stored.source.read_range(&stored.key, offset, size.into()).fs(),
        }
    }
}

trait OrErrno<T> {
    fn fs(self) -> FsResult<T>;
}

impl<T> OrErrno<T> for sandfs_core::Result<T> {
    fn fs(self) -> FsResult<T> {
        self.map_err(|err| errno(&err))
    }
}

struct Node {
    path: VPath,
    /// Lookups the kernel has not forgotten yet.
    lookups: u64,
    /// False once the path was removed or replaced while the kernel still held the inode.
    linked: bool,
    /// Whether real attributes were worked out for this inode. Once they were, the
    /// kernel may hold its real size, and working them out again costs nothing.
    sized: Cell<bool>,
    /// For a removed file, the workspace as it was before: the kernel may still have
    /// it open, and an open file stays readable until the kernel forgets the inode.
    removed_from: Option<Workspace>,
}

/// An open directory, listed once so `readdir` pages through a stable list.
struct DirHandle {
    ino: u64,
    entries: Vec<(u64, FileType, String)>,
    /// Files the listing gave no size for, with their executable bits. Their sizes
    /// aren't looked up for `readdirplus`, since that can mean reading the file.
    #[cfg_attr(not(any(fuse, nfs, test)), allow(dead_code))]
    sizeless: HashMap<String, bool>,
}

/// A file's contents with writes the workspace hasn't seen yet.
struct Buffer {
    body: Body,
    size: u64,
    written: Instant,
}

/// Where a buffer's bytes are: a draft on disk, or memory for a mount with no saved
/// state to keep drafts in.
enum Body {
    Memory(Vec<u8>),
    Draft(Draft),
}

impl Buffer {
    fn new(body: Body, size: u64) -> Self {
        Buffer { body, size, written: Instant::now() }
    }

    fn read(&self, offset: u64, len: u64) -> FsResult<Bytes> {
        match &self.body {
            Body::Memory(data) => {
                let start = usize::try_from(offset).unwrap_or(usize::MAX).min(data.len());
                let end = start.saturating_add(usize::try_from(len).unwrap_or(usize::MAX)).min(data.len());
                Ok(Bytes::copy_from_slice(&data[start..end]))
            }
            Body::Draft(draft) => draft.read_at(offset, len).map_err(io_errno),
        }
    }

    /// Write `data` at `offset` and return how many bytes were written: fewer than
    /// `data` when the disk filled up partway.
    fn write(&mut self, offset: u64, data: &[u8]) -> FsResult<usize> {
        let end = offset.checked_add(data.len() as u64).ok_or(Errno::EFBIG)?;
        let written = match &mut self.body {
            Body::Memory(buffer) => {
                let end = usize::try_from(end).ok().filter(|end| *end as u64 <= MAX_MEMORY_FILE).ok_or(Errno::EFBIG)?;
                if buffer.len() < end {
                    buffer.resize(end, 0);
                }
                buffer[end - data.len()..end].copy_from_slice(data);
                data.len()
            }
            Body::Draft(draft) => draft.write_at(offset, data).map_err(io_errno)?,
        };
        self.size = self.size.max(offset + written as u64);
        Ok(written)
    }

    fn set_len(&mut self, size: u64) -> FsResult<()> {
        match &mut self.body {
            Body::Memory(data) => {
                let size = usize::try_from(size).ok().filter(|size| *size as u64 <= MAX_MEMORY_FILE);
                data.resize(size.ok_or(Errno::EFBIG)?, 0);
            }
            Body::Draft(draft) => draft.set_len(size).map_err(io_errno)?,
        }
        self.size = size;
        Ok(())
    }

    /// Hand the contents to `ws` at `path`.
    fn commit(self, ws: &mut Workspace, path: &str) -> FsResult<u64> {
        match self.body {
            Body::Memory(data) => ws.write(path, Bytes::from(data)).fs(),
            Body::Draft(draft) => ws.write_stored(path, draft.finish().map_err(io_errno)?).fs(),
        }
    }
}

/// A failed write to a draft, as the error the disk gave (a full disk is `ENOSPC`).
fn io_errno(err: std::io::Error) -> Errno {
    eprintln!("sandfs: writing a file failed: {err}");
    Errno::from(err)
}

/// A draft that couldn't be made, as [`io_errno`].
fn draft_errno(err: anyhow::Error) -> Errno {
    match err.downcast::<std::io::Error>() {
        Ok(err) => io_errno(err),
        Err(err) => {
            eprintln!("sandfs: writing a file failed: {err:#}");
            Errno::EIO
        }
    }
}

/// The filesystem state, behind one lock.
pub struct Inner {
    ws: Workspace,
    nodes: HashMap<u64, Node>,
    /// Path to inode, ordered so a renamed directory's entries are one range.
    inos: BTreeMap<String, u64>,
    next_ino: u64,
    handles: HashMap<u64, DirHandle>,
    next_fh: u64,
    /// The handle each folder is being listed through when the kernel lists folders
    /// without opening them, by folder inode.
    #[cfg(any(fuse, test))]
    listings: HashMap<u64, u64>,
    /// Open handles per directory.
    open: HashMap<u64, u32>,
    /// File contents written but not yet committed to the workspace.
    buffers: HashMap<u64, Buffer>,
    /// Where buffers are written; without it (no saved state) they stay in memory.
    pub drafts: Option<Drafts>,
    /// Modification times of touched paths; others report their source's, or `base_time`.
    times: BTreeMap<String, SystemTime>,
    /// Directories created or removed here, which a save must record.
    dirs: BTreeSet<String>,
    base_time: SystemTime,
    uid: u32,
    gid: u32,
    /// Set by every change, cleared when a save takes a snapshot.
    unsaved: bool,
    /// Set by every write, cleared once no buffer is left to commit.
    writing: bool,
    /// Read base files in the ranges asked for, straight from the source: right for a
    /// source that can seek and that the OS already caches (a host folder). Git reads
    /// a whole blob for every range, so its files are read whole into `recent`.
    pub ranged_reads: bool,
    /// Whole files read lately, by inode and version, latest read last, so the kernel's
    /// chunks of one file read it once. At most [`RECENT_FILES`] and [`RECENT_BYTES`],
    /// except that the newest file stays however large it is. Nothing else is kept: the kernel caches
    /// what it read, and a copy here for every file would hold each one twice.
    recent: RefCell<VecDeque<(u64, u64, Bytes)>>,
    /// Keep a removed file readable for the handles still open on it, until the
    /// kernel forgets it. Off for NFS, which never forgets an inode and whose client
    /// renames a file that is still open instead of removing it, so keeping removed
    /// files would only pin their contents in memory.
    pub keep_removed: bool,
    /// Keep files macOS makes for its own bookkeeping out of saves, so they never
    /// show up among the changes, and the `._name` ones out of listings too. See
    /// [`crate::persist::is_bookkeeping`].
    pub skip_bookkeeping: bool,
    /// The workspace as it was mounted, which tells a `._name` file macOS made from one
    /// that is part of the data.
    #[cfg(any(nfs, test))]
    mounted: Workspace,
}

impl Inner {
    /// Serve `ws`, whose source is mounted at `root`, with saved times and directories.
    pub fn new(
        ws: Workspace,
        root: &VPath,
        times: BTreeMap<String, SystemTime>,
        dirs: BTreeSet<String>,
        base_time: SystemTime,
        (uid, gid): (u32, u32),
    ) -> Self {
        let mut inner = Inner {
            #[cfg(any(nfs, test))]
            mounted: ws.fork(),
            ws,
            nodes: HashMap::new(),
            inos: BTreeMap::new(),
            next_ino: ROOT_INO + 1,
            handles: HashMap::new(),
            #[cfg(any(fuse, test))]
            listings: HashMap::new(),
            next_fh: 1,
            open: HashMap::new(),
            buffers: HashMap::new(),
            drafts: None,
            times,
            dirs,
            base_time,
            uid,
            gid,
            unsaved: false,
            writing: false,
            ranged_reads: false,
            recent: RefCell::default(),
            keep_removed: true,
            skip_bookkeeping: false,
        };
        inner.inos.insert(root.to_string(), ROOT_INO);
        inner.nodes.insert(
            ROOT_INO,
            Node { path: root.clone(), lookups: 0, linked: true, sized: Cell::new(false), removed_from: None },
        );
        inner
    }

    /// A copy of what a save needs; the workspace part is an O(1) fork. Files written
    /// within `settle` stay out (see [`Inner::commit_written`]).
    pub fn snapshot(&mut self, settle: Duration) -> Snapshot {
        self.commit_written(settle);
        self.unsaved = false;
        Snapshot { ws: self.ws.fork(), times: self.times.clone(), dirs: self.dirs.clone() }
    }

    /// Whether the file `name` in folder `parent` is one macOS made to hold another
    /// file's extended attributes (`._name`). Listings leave those out, so programs
    /// that walk folders (git, cargo, find) never see them; macOS opens them by name.
    #[cfg(any(nfs, test))]
    pub fn is_added_appledouble(&self, parent: u64, name: &str, kind: FileType) -> bool {
        self.skip_bookkeeping
            && kind == FileType::RegularFile
            && name.starts_with("._")
            && self.child(parent, OsStr::new(name)).is_ok_and(|path| !self.mounted.exists(path.as_str()))
    }

    /// Remove the files of [`Inner::is_added_appledouble`] in folder `ino` when they
    /// are all it holds: a listing shows the folder empty, so it should be removable.
    #[cfg(any(nfs, test))]
    pub fn clear_appledouble(&mut self, ino: u64) -> FsResult<()> {
        let path = self.path(ino)?;
        let entries = self.ws.list(path.as_str()).fs()?;
        let all_added = entries.iter().all(|entry| {
            matches!(entry.kind, Kind::File { .. })
                && self.is_added_appledouble(ino, &entry.name, FileType::RegularFile)
        });
        if all_added {
            for entry in entries {
                self.unlink(ino, OsStr::new(&entry.name))?;
            }
        }
        Ok(())
    }

    // NFS clients never forget an inode, and resume a listing after the inode its last
    // page ended on, which may have been removed or renamed since.

    /// The folder holding `ino`.
    #[cfg(any(nfs, test))]
    pub fn parent_of(&mut self, ino: u64) -> Option<u64> {
        if ino == ROOT_INO {
            return Some(ROOT_INO);
        }
        let parent = self.path(ino).ok()?.parent()?;
        Some(self.ino_for(&parent))
    }

    /// How many inodes are known, to check none are kept for nothing.
    #[cfg(test)]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// The name `ino` was last seen under, even if it is gone now.
    #[cfg(any(nfs, test))]
    pub fn last_name_of(&self, ino: u64) -> Option<String> {
        self.nodes.get(&ino)?.path.name().map(str::to_owned)
    }

    /// Count a listed entry as looked up, so its inode outlives its name.
    #[cfg(any(nfs, test))]
    pub fn hold(&mut self, ino: u64) {
        if let Some(node) = self.nodes.get_mut(&ino) {
            node.lookups += 1;
        }
    }

    // ------------------------------------------------------------------
    // Inodes and attributes
    // ------------------------------------------------------------------

    /// Refuse to start changing `ino` where the workspace is read-only. Writes collect
    /// in a buffer and reach the workspace only when saved, too late to refuse them.
    fn check_writable(&self, ino: u64) -> FsResult<()> {
        match self.nodes.get(&ino) {
            Some(node) if node.linked && self.ws.is_read_only(node.path.as_str()) => Err(Errno::EROFS),
            _ => Ok(()),
        }
    }

    fn path(&self, ino: u64) -> FsResult<VPath> {
        match self.nodes.get(&ino) {
            Some(node) if node.linked => Ok(node.path.clone()),
            _ => Err(Errno::ENOENT),
        }
    }

    fn child(&self, parent: u64, name: &OsStr) -> FsResult<VPath> {
        let name = name.to_str().ok_or(Errno::EINVAL)?;
        self.path(parent)?.join(name).map_err(|_| Errno::EINVAL)
    }

    fn ino_for(&mut self, path: &VPath) -> u64 {
        if let Some(&ino) = self.inos.get(path.as_str()) {
            return ino;
        }
        // Every name of a hard-linked file is one inode, so the kernel caches its
        // pages once and a write through one name is read through the others.
        if let Some(ino) = self.linked_ino(path) {
            self.inos.insert(path.to_string(), ino);
            return ino;
        }
        let ino = self.next_ino;
        self.next_ino += 1;
        self.inos.insert(path.to_string(), ino);
        self.nodes.insert(
            ino,
            Node { path: path.clone(), lookups: 0, linked: true, sized: Cell::new(false), removed_from: None },
        );
        ino
    }

    /// The inode another name of the file at `path` already has.
    fn linked_ino(&self, path: &VPath) -> Option<u64> {
        if !self.ws.has_hard_links() {
            return None;
        }
        let names = self.ws.hard_links(path.as_str()).ok()?;
        names.iter().find_map(|name| self.inos.get(name.as_str()).copied())
    }

    /// The other names of the file at `path`, if it has any.
    fn other_names(&self, path: &VPath) -> Vec<VPath> {
        if !self.ws.has_hard_links() {
            return Vec::new();
        }
        let mut names = self.ws.hard_links(path.as_str()).unwrap_or_default();
        names.retain(|name| name != path);
        names
    }

    /// Reply data for a name the kernel will now hold: counts as one lookup.
    fn entry(&mut self, path: &VPath) -> FsResult<FileAttr> {
        let ino = self.ino_for(path);
        let attr = self.attr(ino)?;
        if let Some(node) = self.nodes.get_mut(&ino) {
            node.lookups += 1;
        }
        Ok(attr)
    }

    pub fn attr(&self, ino: u64) -> FsResult<FileAttr> {
        let node = self.nodes.get(&ino).ok_or(Errno::ENOENT)?;
        if !node.linked {
            // A removed file or link the kernel may still have open.
            let (kind, perm, size, modified) = match (self.buffers.get(&ino), &node.removed_from) {
                (Some(buffer), _) => (FileType::RegularFile, 0o644, buffer.size, None),
                (None, Some(before)) => {
                    let meta = before.stat(node.path.as_str()).fs()?;
                    let (kind, perm) = file_type(meta.kind);
                    (kind, perm, meta.size, meta.modified)
                }
                (None, None) => return Err(Errno::ENOENT),
            };
            node.sized.set(true);
            return Ok(self.make_attr(ino, kind, perm, size, modified.unwrap_or(self.base_time)));
        }
        let path = node.path.as_str();
        let meta = self.ws.stat(path).fs()?;
        node.sized.set(true);
        let size = match self.buffers.get(&ino) {
            Some(buffer) => buffer.size,
            None if meta.kind.is_dir() => 0,
            None => meta.size,
        };
        let (kind, mut perm) = file_type(meta.kind);
        if kind != FileType::Symlink && self.ws.is_read_only(path) {
            perm &= !0o222;
        }
        // A file nobody wrote keeps its source's time, the same in every mount and fork,
        // so build and test caches that check times find it unchanged.
        let mtime = self.times.get(path).copied().or(meta.modified).unwrap_or(self.base_time);
        let mut attr = self.make_attr(ino, kind, perm, size, mtime);
        if kind != FileType::Directory {
            attr.nlink = self.nlink(path);
        }
        Ok(attr)
    }

    /// How many names the file at `path` has: more than one through hard links.
    fn nlink(&self, path: &str) -> u32 {
        if !self.ws.has_hard_links() {
            return 1;
        }
        self.ws.hard_links(path).map_or(1, |names| names.len() as u32)
    }

    /// Set the modification time of the file at `path` and of its other names, since
    /// they are one inode and any of them may be the one asked about.
    fn set_time(&mut self, path: &VPath, time: SystemTime) {
        for other in self.other_names(path) {
            self.times.insert(other.to_string(), time);
        }
        self.times.insert(path.to_string(), time);
    }

    /// Note the source's time of a file nobody wrote that just got a new name. Its next
    /// save keeps it apart from the source, so only `times` can carry that time on.
    fn keep_source_time(&mut self, path: &VPath) {
        if !self.times.contains_key(path.as_str())
            && let Ok(Some(time)) = self.ws.modified(path.as_str())
        {
            self.times.insert(path.to_string(), time);
        }
    }

    /// Attributes for a file whose size isn't known without reading it: size 0, for
    /// the kernel to ask about again.
    #[cfg(any(fuse, test))]
    fn sizeless_attr(&self, ino: u64, executable: bool) -> FsResult<FileAttr> {
        let node = self.nodes.get(&ino).ok_or(Errno::ENOENT)?;
        let path = node.path.as_str();
        if !node.linked || !self.ws.exists(path) {
            return Err(Errno::ENOENT);
        }
        let mtime = self.times.get(path).copied().unwrap_or(self.base_time);
        let mut attr = self.make_attr(ino, FileType::RegularFile, if executable { 0o755 } else { 0o644 }, 0, mtime);
        attr.nlink = self.nlink(path);
        Ok(attr)
    }

    fn make_attr(&self, ino: u64, kind: FileType, perm: u16, size: u64, mtime: SystemTime) -> FileAttr {
        FileAttr {
            ino: INodeNo(ino),
            size,
            blocks: size.div_ceil(512),
            atime: mtime,
            mtime,
            ctime: mtime,
            crtime: mtime,
            kind,
            perm,
            nlink: if kind == FileType::Directory { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

    pub fn lookup(&mut self, parent: u64, name: &OsStr) -> FsResult<FileAttr> {
        let path = self.child(parent, name)?;
        self.ws.stat(path.as_str()).fs()?;
        self.entry(&path)
    }

    #[cfg(any(fuse, test))]
    pub fn forget(&mut self, ino: u64, count: u64) {
        if ino == ROOT_INO {
            return;
        }
        if let Some(node) = self.nodes.get_mut(&ino) {
            node.lookups = node.lookups.saturating_sub(count);
        }
        self.maybe_drop(ino);
    }

    fn is_open(&self, ino: u64) -> bool {
        self.open.get(&ino).is_some_and(|count| *count > 0)
    }

    /// Drop an inode the kernel no longer holds and no listing has open. The kernel
    /// holds every file that is open, so a forgotten file is a closed one.
    fn maybe_drop(&mut self, ino: u64) {
        let unused = self.nodes.get(&ino).is_some_and(|node| node.lookups == 0) && !self.is_open(ino);
        if unused && ino != ROOT_INO {
            if let Some(node) = self.nodes.remove(&ino) {
                // The kernel can forget a closed file well before its writes settle,
                // as soon as memory runs short. Its writes belong to its path.
                if node.linked
                    && let Some(buffer) = self.buffers.remove(&ino)
                {
                    let _ = buffer.commit(&mut self.ws, node.path.as_str());
                    self.unsaved = true;
                }
                let others = if node.linked { self.other_names(&node.path) } else { Vec::new() };
                for name in others.iter().chain([&node.path]) {
                    if self.inos.get(name.as_str()) == Some(&ino) {
                        self.inos.remove(name.as_str());
                    }
                }
            }
            self.buffers.remove(&ino);
        }
    }

    /// Mark the inode at `path` as removed. A removed file keeps `before`, the
    /// workspace from before the removal, until the kernel forgets it, so reads and
    /// writes through handles still open on it keep working. Forking is O(1).
    ///
    /// `others` are the file's other names (hard links), which keep the inode alive.
    fn unlink_path(&mut self, path: &VPath, before: Option<Workspace>, others: &[VPath]) {
        let Some(ino) = self.inos.remove(path.as_str()) else { return };
        if let Some(other) = others.first() {
            if let Some(node) = self.nodes.get_mut(&ino)
                && node.path == *path
            {
                node.path = other.clone();
            }
            self.inos.entry(other.to_string()).or_insert(ino);
            return;
        }
        if let Some(node) = self.nodes.get_mut(&ino) {
            node.linked = false;
            node.removed_from = before.filter(|_| self.keep_removed);
            // Without removed files kept readable (NFS), nothing ever says a client is
            // done with an inode, so a removed one goes now; a client still holding its
            // handle gets an error.
            if !self.keep_removed {
                node.lookups = 0;
            }
        }
        if !self.keep_removed {
            self.buffers.remove(&ino);
        }
        self.maybe_drop(ino);
    }

    fn touch(&mut self, path: &VPath) {
        self.times.insert(path.to_string(), SystemTime::now());
        if let Some(parent) = path.parent() {
            self.times.insert(parent.to_string(), SystemTime::now());
        }
        self.unsaved = true;
    }

    // ------------------------------------------------------------------
    // Namespace operations
    // ------------------------------------------------------------------

    pub fn setattr(
        &mut self,
        ino: u64,
        mode: Option<u32>,
        size: Option<u64>,
        mtime: Option<TimeOrNow>,
    ) -> FsResult<FileAttr> {
        if size.is_some() || mtime.is_some() {
            self.check_writable(ino)?;
        }
        if let Some(size) = size {
            if size > MAX_FILE_SIZE as u64 {
                return Err(Errno::EFBIG);
            }
            // Only a regular file has a size to set, as truncate(2) says; the kernel
            // follows links first, but an NFS client may name one.
            let (ws, path) = self.contents(ino)?;
            match ws.read_link(path.as_str()) {
                Ok(_) => return Err(Errno::EINVAL),
                // Not a link: the one refusal that lets the size be set.
                Err(Error::Unsupported(..)) => {}
                Err(err) => return Err(errno(&err)),
            }
            if size == 0 {
                // Emptying a file (O_TRUNC arrives this way) needn't read it first.
                let empty = self.empty_buffer()?;
                self.buffers.insert(ino, empty);
                self.writing = true;
            } else {
                self.change_buffer(ino, |buffer| buffer.set_len(size))?;
            }
            // ftruncate on a removed file that is still open changes only its buffer.
            if !self.nodes.get(&ino).is_some_and(|node| node.linked) {
                return self.attr(ino);
            }
            let path = self.path(ino)?;
            self.touch(&path);
        }
        if let Some(mode) = mode {
            let path = self.path(ino)?;
            if let Kind::File { .. } = self.ws.stat(path.as_str()).fs()?.kind {
                // Only the executable bit is kept; other mode bits are not stored. The
                // bytes stay the same, and so does their time.
                self.keep_source_time(&path);
                self.ws.set_executable(path.as_str(), mode & 0o111 != 0).fs()?;
                self.unsaved = true;
            }
        }
        if let Some(mtime) = mtime {
            let path = self.path(ino)?;
            let time = match mtime {
                TimeOrNow::Now => SystemTime::now(),
                TimeOrNow::SpecificTime(time) => time,
            };
            self.set_time(&path, time);
            self.unsaved = true;
        }
        self.attr(ino)
    }

    pub fn mkdir(&mut self, parent: u64, name: &OsStr) -> FsResult<FileAttr> {
        let path = self.child(parent, name)?;
        self.ws.mkdir(path.as_str(), false).fs()?;
        self.dirs.insert(path.to_string());
        self.touch(&path);
        self.entry(&path)
    }

    /// symlink(2): a link whose target is stored as given and resolved by the system.
    pub fn symlink(&mut self, parent: u64, name: &OsStr, target: &OsStr) -> FsResult<FileAttr> {
        let path = self.child(parent, name)?;
        let target = target.to_str().ok_or(Errno::EINVAL)?;
        self.ws.symlink(path.as_str(), target).fs()?;
        self.touch(&path);
        self.entry(&path)
    }

    /// readlink(2): the target of the link at `ino`.
    pub fn readlink(&self, ino: u64) -> FsResult<String> {
        let (ws, path) = self.contents(ino)?;
        ws.read_link(path.as_str()).map_err(|err| match err {
            Error::Unsupported(..) => Errno::EINVAL,
            err => errno(&err),
        })
    }

    /// link(2): a second name for the file at `ino`, which stays one inode.
    #[cfg(any(fuse, test))]
    pub fn link(&mut self, ino: u64, new_parent: u64, new_name: &OsStr) -> FsResult<FileAttr> {
        let from = self.path(ino)?;
        let to = self.child(new_parent, new_name)?;
        self.ws.hard_link(from.as_str(), to.as_str()).fs()?;
        self.inos.insert(to.to_string(), ino);
        // The file keeps its time; only the directory it gained a name in changes.
        self.keep_source_time(&from);
        match self.times.get(from.as_str()).copied() {
            Some(time) => self.times.insert(to.to_string(), time),
            None => self.times.remove(to.as_str()),
        };
        self.touch_parent(&to);
        self.entry(&to)
    }

    pub fn unlink(&mut self, parent: u64, name: &OsStr) -> FsResult<()> {
        let path = self.child(parent, name)?;
        if self.ws.stat(path.as_str()).fs()?.kind.is_dir() {
            return Err(Errno::EISDIR);
        }
        let before = self.ws.fork();
        let others = self.other_names(&path);
        self.ws.remove(path.as_str(), false).fs()?;
        self.unlink_path(&path, Some(before), &others);
        self.times.remove(path.as_str());
        self.touch_parent(&path);
        Ok(())
    }

    pub fn rmdir(&mut self, parent: u64, name: &OsStr) -> FsResult<()> {
        let path = self.child(parent, name)?;
        if !self.ws.stat(path.as_str()).fs()?.kind.is_dir() {
            return Err(Errno::ENOTDIR);
        }
        self.ws.remove(path.as_str(), false).fs()?;
        self.unlink_path(&path, None, &[]);
        self.times.remove(path.as_str());
        self.dirs.insert(path.to_string());
        self.touch_parent(&path);
        Ok(())
    }

    fn touch_parent(&mut self, path: &VPath) {
        if let Some(parent) = path.parent() {
            self.times.insert(parent.to_string(), SystemTime::now());
        }
        self.unsaved = true;
    }

    /// rename(2): replaces an existing destination of a compatible kind.
    pub fn rename(
        &mut self,
        parent: u64,
        name: &OsStr,
        new_parent: u64,
        new_name: &OsStr,
        flags: RenameFlags,
    ) -> FsResult<()> {
        if rename_unsupported(flags) {
            return Err(Errno::EINVAL);
        }
        let from = self.child(parent, name)?;
        let to = self.child(new_parent, new_name)?;
        let moving_dir = self.ws.stat(from.as_str()).fs()?.kind.is_dir();
        if from == to {
            return Ok(());
        }
        if to.starts_with(&from) {
            return Err(Errno::EINVAL);
        }
        let replacing = match self.ws.stat(to.as_str()) {
            Ok(meta) => {
                if rename_noreplace(flags) {
                    return Err(Errno::EEXIST);
                }
                // Two names of one file: rename(2) leaves both.
                if !moving_dir && self.other_names(&to).contains(&from) {
                    return Ok(());
                }
                match (moving_dir, meta.kind.is_dir()) {
                    (true, false) => return Err(Errno::ENOTDIR),
                    (false, true) => return Err(Errno::EISDIR),
                    (true, true) if !self.ws.list(to.as_str()).fs()?.is_empty() => return Err(Errno::ENOTEMPTY),
                    _ => true,
                }
            }
            Err(Error::NotFound(_)) => false,
            Err(err) => return Err(errno(&err)),
        };
        // Work on a fork so a failure halfway leaves the workspace as it was.
        let mut next = self.ws.fork();
        let others = if replacing && !moving_dir { self.other_names(&to) } else { Vec::new() };
        if replacing {
            next.remove(to.as_str(), true).fs()?;
        }
        next.rename(from.as_str(), to.as_str()).fs()?;
        if replacing {
            let before = (!moving_dir).then(|| self.ws.fork());
            self.unlink_path(&to, before, &others);
            self.times.remove(to.as_str());
        }
        self.ws = next;

        self.move_subtree(&from, &to);
        if moving_dir {
            // Empty directories inside the moved one are invisible to `changes`.
            let mut stack = vec![to.clone()];
            while let Some(dir) = stack.pop() {
                for entry in self.ws.list(dir.as_str()).fs()? {
                    let child = dir.join(&entry.name).map_err(|_| Errno::EINVAL)?;
                    if entry.kind.is_dir() {
                        stack.push(child);
                    } else {
                        self.keep_source_time(&child);
                    }
                }
                self.dirs.insert(dir.to_string());
            }
            self.dirs.insert(from.to_string());
        } else {
            self.keep_source_time(&to);
        }
        self.touch_parent(&from);
        self.touch_parent(&to);
        Ok(())
    }

    /// Re-key inodes and times at or below `from` to `to`.
    fn move_subtree(&mut self, from: &VPath, to: &VPath) {
        let moved = |map_keys: Vec<String>| -> Vec<(String, String)> {
            map_keys.into_iter().map(|key| (key.clone(), format!("{to}{}", &key[from.as_str().len()..]))).collect()
        };
        let ino_keys = moved(subtree_keys(&self.inos, from));
        for (old, new) in ino_keys {
            if let Some(ino) = self.inos.remove(&old) {
                if let (Some(node), Ok(path)) = (self.nodes.get_mut(&ino), VPath::parse(&new)) {
                    node.path = path;
                }
                self.inos.insert(new, ino);
            }
        }
        let time_keys = moved(subtree_keys(&self.times, from));
        for (old, new) in time_keys {
            if let Some(time) = self.times.remove(&old) {
                self.times.insert(new, time);
            }
        }
    }

    // ------------------------------------------------------------------
    // Files
    // ------------------------------------------------------------------

    /// Create a file. With `exclusive` an existing file is an error; otherwise it is
    /// emptied when `truncate` is set.
    pub fn create(
        &mut self,
        parent: u64,
        name: &OsStr,
        mode: u32,
        exclusive: bool,
        truncate: bool,
    ) -> FsResult<FileAttr> {
        let path = self.child(parent, name)?;
        match self.ws.stat(path.as_str()) {
            Ok(_) if exclusive => return Err(Errno::EEXIST),
            Ok(meta) if meta.kind.is_dir() => return Err(Errno::EISDIR),
            // Emptying a link would empty what it points to, which sandfs never follows.
            Ok(meta) if truncate && meta.kind == Kind::Symlink => return Err(Errno::ELOOP),
            Ok(_) => {
                if truncate {
                    if self.ws.is_read_only(path.as_str()) {
                        return Err(Errno::EROFS);
                    }
                    let ino = self.ino_for(&path);
                    let empty = self.empty_buffer()?;
                    self.buffers.insert(ino, empty);
                    self.writing = true;
                    self.touch(&path);
                }
            }
            Err(Error::NotFound(_)) => {
                self.ws.write(path.as_str(), Bytes::new()).fs()?;
                if mode & 0o111 != 0 {
                    self.ws.set_executable(path.as_str(), true).fs()?;
                }
                self.touch(&path);
            }
            Err(err) => return Err(errno(&err)),
        }
        self.entry(&path)
    }

    /// The workspace and path holding `ino`'s contents: the workspace itself, or for
    /// a removed file the kernel may still have open, the one from before the removal.
    fn contents(&self, ino: u64) -> FsResult<(&Workspace, &VPath)> {
        match self.nodes.get(&ino) {
            Some(Node { linked: true, path, .. }) => Ok((&self.ws, path)),
            Some(Node { removed_from: Some(before), path, .. }) => Ok((before, path)),
            _ => Err(Errno::ENOENT),
        }
    }

    #[cfg(test)]
    pub fn read(&self, ino: u64, offset: u64, size: u32) -> FsResult<Bytes> {
        self.reading(ino, offset, size)?.read(offset, size)
    }

    /// A read of `ino`, done here unless the file is saved on disk: then the caller
    /// reads it ([`Reading::read`]) after letting go of the lock, so reading a file
    /// holds up no other request.
    pub fn reading(&self, ino: u64, offset: u64, size: u32) -> FsResult<Reading> {
        let range = |len: usize| {
            let start = usize::try_from(offset).unwrap_or(usize::MAX).min(len);
            start..start.saturating_add(size as usize).min(len)
        };
        if let Some(buffer) = self.buffers.get(&ino) {
            return buffer.read(offset, size.into()).map(Reading::Read);
        }
        let (ws, path) = self.contents(ino)?;
        // Saved files are on disk, where reading a range is cheap.
        if let Some(stored) = ws.stored(path.as_str()).fs()? {
            return Ok(Reading::Stored(stored));
        }
        if self.ranged_reads {
            return ws.read_range(path.as_str(), offset, size.into()).fs().map(Reading::Read);
        }
        let meta = ws.stat(path.as_str()).fs()?;
        let data = match self.kept(ino, meta.version) {
            Some(data) => data,
            None => {
                // A range covering the whole file is read from the source without the
                // workspace keeping it.
                let data = ws.read_range(path.as_str(), 0, meta.size).fs()?;
                self.remember(ino, meta.version, data.clone());
                data
            }
        };
        Ok(Reading::Read(data.slice(range(data.len()))))
    }

    /// The kept copy of `ino` at `version`, moved to the back as the latest read, so
    /// a file read in chunks between other files stays kept.
    fn kept(&self, ino: u64, version: u64) -> Option<Bytes> {
        let mut recent = self.recent.borrow_mut();
        let at = recent.iter().position(|(i, v, _)| (*i, *v) == (ino, version))?;
        let entry = recent.remove(at)?;
        let data = entry.2.clone();
        recent.push_back(entry);
        Some(data)
    }

    /// Keep a whole file in `recent`, dropping the oldest ones beyond the bounds.
    fn remember(&self, ino: u64, version: u64, data: Bytes) {
        let mut recent = self.recent.borrow_mut();
        recent.retain(|(i, _, _)| *i != ino);
        recent.push_back((ino, version, data));
        let mut total: usize = recent.iter().map(|(_, _, data)| data.len()).sum();
        while recent.len() > 1 && (recent.len() > RECENT_FILES || total > RECENT_BYTES) {
            total -= recent.pop_front().map_or(0, |(_, _, data)| data.len());
        }
    }

    /// A new, empty buffer.
    fn empty_buffer(&self) -> FsResult<Buffer> {
        let body = match &self.drafts {
            Some(drafts) => Body::Draft(drafts.empty().map_err(draft_errno)?),
            None => Body::Memory(Vec::new()),
        };
        Ok(Buffer::new(body, 0))
    }

    /// The write buffer for `ino`, filled from the workspace on first use.
    fn buffer(&mut self, ino: u64) -> FsResult<&mut Buffer> {
        if !self.buffers.contains_key(&ino) {
            self.check_writable(ino)?;
            let (ws, path) = self.contents(ino)?;
            let size = ws.stat(path.as_str()).fs()?.size;
            let body = match &self.drafts {
                // On disk: a saved file is copied there, which the disk may do without
                // copying a byte, and anything else is read in chunks.
                Some(drafts) => Body::Draft(drafts.copy(ws, path.as_str(), self.ranged_reads).map_err(draft_errno)?),
                // Check before reading: a large source file would otherwise be read whole.
                None if size > MAX_MEMORY_FILE => return Err(Errno::EFBIG),
                // A ranged read keeps no copy of a base file in the workspace's cache.
                None => Body::Memory(ws.read_range(path.as_str(), 0, size).fs()?.to_vec()),
            };
            self.buffers.insert(ino, Buffer::new(body, size));
        }
        self.writing = true;
        let buffer = self.buffers.get_mut(&ino).ok_or(Errno::EIO)?;
        buffer.written = Instant::now();
        Ok(buffer)
    }

    /// Apply `change` to the write buffer for `ino`. A first change that fails left
    /// the file as it was, so the buffer it made is dropped rather than committed.
    fn change_buffer<T>(&mut self, ino: u64, change: impl FnOnce(&mut Buffer) -> FsResult<T>) -> FsResult<T> {
        let new = !self.buffers.contains_key(&ino);
        let result = change(self.buffer(ino)?);
        if result.is_err() && new {
            self.buffers.remove(&ino);
        }
        result
    }

    pub fn write(&mut self, ino: u64, offset: u64, data: &[u8]) -> FsResult<u32> {
        if offset.checked_add(data.len() as u64).is_none_or(|end| end > MAX_FILE_SIZE as u64) {
            return Err(Errno::EFBIG);
        }
        let written = self.change_buffer(ino, |buffer| buffer.write(offset, data))?;
        if let Ok(path) = self.path(ino) {
            self.set_time(&path, SystemTime::now());
        }
        Ok(u32::try_from(written).unwrap_or(u32::MAX))
    }

    /// Copy up to `len` bytes of `src` from `offset_in` into `dst` at `offset_out`, as
    /// `copy_file_range` asks (`std::fs::copy`, and programs copying instead of
    /// hard-linking). A whole file copied into an empty one becomes a copy in the
    /// workspace, which shares the bytes (a saved file's object on disk, or memory), so
    /// the copy reads and stores nothing; a file of the source is copied to disk in
    /// chunks. Any other range is written a piece at a time, as a write would be.
    #[cfg_attr(not(fuse), allow(dead_code))]
    pub fn copy_range(&mut self, src: u64, offset_in: u64, dst: u64, offset_out: u64, len: u64) -> FsResult<u32> {
        self.check_writable(dst)?;
        let size = match self.buffers.get(&src) {
            Some(buffer) => buffer.size,
            None => {
                let (ws, path) = self.contents(src)?;
                ws.stat(path.as_str()).fs()?.size
            }
        };
        let linked = |ino| matches!(self.nodes.get(&ino), Some(Node { linked: true, .. }));
        if offset_in == 0 && offset_out == 0 && len >= size && linked(src) && linked(dst) && self.attr(dst)?.size == 0 {
            let (from, to) = (self.path(src)?, self.path(dst)?);
            // A file still being written is committed first, as it would be once settled.
            if let Some(buffer) = self.buffers.remove(&src) {
                buffer.commit(&mut self.ws, from.as_str())?;
            }
            // The copy takes the source's mode; the destination keeps the one it was made with.
            let executable = matches!(self.ws.stat(to.as_str()).fs()?.kind, Kind::File { executable: true });
            // A file not saved yet (a file of the source) is copied into a draft in
            // chunks, as a first write would copy it: shared, the next save would read it
            // whole into memory to write it out.
            let stored = match (&self.drafts, self.ws.stored(from.as_str()).fs()?) {
                (Some(drafts), None) => {
                    let draft = drafts.copy(&self.ws, from.as_str(), self.ranged_reads).map_err(draft_errno)?;
                    Some(draft.finish().map_err(io_errno)?)
                }
                _ => None,
            };
            // Only now does the destination's buffer go: until the copy is ready, it
            // holds a truncation the workspace hasn't seen, which a failed copy keeps.
            self.buffers.remove(&dst);
            match stored {
                Some(stored) => {
                    self.ws.write_stored(to.as_str(), stored).fs()?;
                }
                None => {
                    self.ws.remove(to.as_str(), false).fs()?;
                    self.ws.copy(from.as_str(), to.as_str(), false).fs()?;
                }
            }
            self.ws.set_executable(to.as_str(), executable).fs()?;
            self.times.insert(to.to_string(), SystemTime::now());
            self.unsaved = true;
            // The reply counts bytes in 32 bits. Past that the caller asks again for the
            // rest, which then goes piece by piece over the shared copy.
            return Ok(u32::try_from(size).unwrap_or(u32::MAX));
        }
        let len = len.min(COPY_PIECE);
        let piece = match self.buffers.get(&src) {
            Some(buffer) => buffer.read(offset_in, len)?,
            None => {
                let (ws, path) = self.contents(src)?;
                ws.read_range(path.as_str(), offset_in, len).fs()?
            }
        };
        if piece.is_empty() {
            return Ok(0);
        }
        self.write(dst, offset_out, &piece)
    }

    /// Move into the workspace every buffer that has gone `settle` without a write. A
    /// file still being written stays buffered: committing it would copy it, and its
    /// next write would copy it back. A removed file's buffer stays where it is: it
    /// belongs to nobody but the handles still open on it.
    pub fn commit_written(&mut self, settle: Duration) {
        let now = Instant::now();
        let mut waiting = false;
        let inos: Vec<u64> = self.buffers.keys().copied().collect();
        for ino in inos {
            let Ok(path) = self.path(ino) else { continue };
            if now.duration_since(self.buffers[&ino].written) < settle {
                waiting = true;
                continue;
            }
            let Some(buffer) = self.buffers.remove(&ino) else { continue };
            // A failure is logged by `errno`; the write is lost either way, as it
            // would be if a commit at close had failed.
            let _ = buffer.commit(&mut self.ws, path.as_str());
            self.unsaved = true;
        }
        self.writing = waiting;
    }

    // ------------------------------------------------------------------
    // Directories
    // ------------------------------------------------------------------

    /// Open a directory, listing it once so `readdir` pages through a stable list.
    pub fn opendir(&mut self, ino: u64) -> FsResult<u64> {
        let path = self.path(ino)?;
        let listing = self.ws.list(path.as_str()).fs()?;
        let parent = match path.parent() {
            Some(parent) if ino != ROOT_INO => self.ino_for(&parent),
            _ => ROOT_INO,
        };
        let mut entries =
            vec![(ino, FileType::Directory, ".".to_owned()), (parent, FileType::Directory, "..".to_owned())];
        let mut sizeless = HashMap::new();
        for entry in listing {
            if let (Kind::File { executable }, None) = (entry.kind, entry.size) {
                sizeless.insert(entry.name.clone(), executable);
            }
            let child = path.join(&entry.name).map_err(|_| Errno::EIO)?;
            let kind = match entry.kind {
                Kind::Dir => FileType::Directory,
                Kind::File { .. } => FileType::RegularFile,
                Kind::Symlink => FileType::Symlink,
            };
            entries.push((self.ino_for(&child), kind, entry.name));
        }
        let fh = self.next_fh;
        self.next_fh += 1;
        self.handles.insert(fh, DirHandle { ino, entries, sizeless });
        // The listing keeps its folder's inode until releasedir, since readdirplus
        // resolves names through it.
        *self.open.entry(ino).or_default() += 1;
        Ok(fh)
    }

    /// Directory entries from position `offset` on; each entry's offset is its
    /// position plus one, which is where the next call resumes.
    pub fn readdir(&self, fh: u64, offset: u64) -> FsResult<&[(u64, FileType, String)]> {
        let handle = self.handles.get(&fh).ok_or(Errno::EBADF)?;
        Ok(handle.entries.get(usize::try_from(offset).unwrap_or(usize::MAX)..).unwrap_or_default())
    }

    /// The files listing `fh` has no size for and whose size nobody worked out since,
    /// which can mean reading them: an NFS listing leaves their attributes out.
    #[cfg(any(nfs, test))]
    pub fn sizeless(&self, fh: u64) -> FsResult<std::collections::HashSet<u64>> {
        let handle = self.handles.get(&fh).ok_or(Errno::EBADF)?;
        let sizeless = handle.entries.iter().filter(|(ino, _, name)| {
            handle.sizeless.contains_key(name)
                && !self.buffers.contains_key(ino)
                && !self.nodes.get(ino).is_some_and(|node| node.sized.get())
        });
        Ok(sizeless.map(|(ino, ..)| *ino).collect())
    }

    /// Like [`Inner::readdir`], with each entry's attributes and how long the kernel
    /// may keep them, handed to `add` until it reports the reply full. The kernel counts
    /// every entry it accepts as a lookup, except `.` and `..`, so those entries count
    /// here too.
    ///
    /// A file the listing has no size for, and whose real attributes were never worked
    /// out, gets size 0 and no time to keep it, so the kernel asks for its attributes
    /// when something needs them (`ls -l`, a read) and a walk that needs only names
    /// (`find`) reads no file to find sizes. One whose attributes were worked out gets
    /// them again: the kernel may hold its real size, and size 0 would shrink the file
    /// under anything reading it, such as a program mapping it into memory.
    #[cfg(any(fuse, test))]
    pub fn readdirplus(
        &mut self,
        fh: u64,
        offset: u64,
        mut add: impl FnMut(u64, u64, &str, &FileAttr, Duration) -> bool,
    ) -> FsResult<()> {
        let mut position = usize::try_from(offset).unwrap_or(usize::MAX);
        loop {
            let Some(DirHandle { ino: dir, entries, sizeless }) = self.handles.get(&fh) else {
                return Err(Errno::EBADF);
            };
            let dir = *dir;
            let Some((saved, _, name)) = entries.get(position).cloned() else { return Ok(()) };
            let executable = sizeless.get(&name).copied();
            position += 1;
            let dots = name == "." || name == "..";
            // Every other name is resolved again: the kernel keeps what this returns for
            // the cache TTL, so a child renamed, removed or forgotten since `opendir` must
            // never be published under its old name or with a dropped inode.
            let ino = if dots {
                saved
            } else {
                let Ok(path) = self.path(dir).and_then(|path| path.join(&name).map_err(|_| Errno::EIO)) else {
                    continue;
                };
                self.ino_for(&path)
            };
            let sized = self.nodes.get(&ino).is_some_and(|node| node.sized.get());
            let attr = match executable.filter(|_| !sized && !self.buffers.contains_key(&ino)) {
                Some(executable) => self.sizeless_attr(ino, executable).map(|attr| (attr, Duration::ZERO)),
                None => self.attr(ino).map(|attr| (attr, TTL)),
            };
            let Ok((attr, ttl)) = attr else {
                self.maybe_drop(ino);
                continue;
            };
            if add(ino, position as u64, &name, &attr, ttl) {
                if !dots {
                    self.maybe_drop(ino);
                }
                return Ok(());
            }
            if !dots && let Some(node) = self.nodes.get_mut(&ino) {
                node.lookups += 1;
            }
        }
    }

    /// The handle to list folder `ino` through from `offset` on, for a kernel that lists
    /// folders without opening them. A listing from the start takes a fresh look at
    /// the folder; one that goes on reuses the handle it started with.
    #[cfg(any(fuse, test))]
    pub fn listing(&mut self, ino: u64, offset: u64) -> FsResult<u64> {
        match self.listings.get(&ino) {
            Some(&fh) if offset > 0 => Ok(fh),
            _ => {
                self.end_listing(ino);
                let fh = self.opendir(ino)?;
                self.listings.insert(ino, fh);
                // Nothing tells sandfs that a reader gave up halfway, so only the
                // newest listings are kept; a reader going on with an older one gets a
                // fresh look at the folder from where it was.
                while self.listings.len() > LISTINGS_KEPT {
                    let Some(oldest) = self.listings.iter().min_by_key(|(_, fh)| **fh).map(|(&ino, _)| ino) else {
                        break;
                    };
                    self.end_listing(oldest);
                }
                Ok(fh)
            }
        }
    }

    /// Let go of the handle folder `ino` was listed through, once its listing is done.
    #[cfg(any(fuse, test))]
    pub fn end_listing(&mut self, ino: u64) {
        if let Some(fh) = self.listings.remove(&ino) {
            self.releasedir(fh);
        }
    }

    pub fn releasedir(&mut self, fh: u64) {
        let Some(DirHandle { ino, .. }) = self.handles.remove(&fh) else { return };
        if let Some(count) = self.open.get_mut(&ino) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.open.remove(&ino);
            }
        }
        self.maybe_drop(ino);
    }
}

/// The filesystem state plus what saving needs, shared with the saver thread.
pub struct Shared {
    inner: Mutex<Inner>,
    saved: Mutex<Saved>,
    root: VPath,
    wake: Condvar,
    stop: AtomicBool,
    /// Whether the kernel took no-open mode, so opens and flushes can be refused.
    #[cfg(fuse)]
    no_open: AtomicBool,
    /// Whether the kernel lists folders without opening them, and keeps their listings.
    #[cfg(fuse)]
    no_opendir: AtomicBool,
}

impl Shared {
    pub fn new(inner: Inner, saved: Saved, root: VPath) -> Arc<Self> {
        Arc::new(Shared {
            inner: Mutex::new(inner),
            saved: Mutex::new(saved),
            root,
            wake: Condvar::new(),
            stop: AtomicBool::new(false),
            #[cfg(fuse)]
            no_open: AtomicBool::new(false),
            #[cfg(fuse)]
            no_opendir: AtomicBool::new(false),
        })
    }

    pub fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Save the workspace's changes now, including every write; `durable` also
    /// flushes them to disk, as `fsync` promises.
    pub fn save(&self, durable: bool) -> anyhow::Result<()> {
        self.save_settled(Duration::ZERO, durable)
    }

    /// Save, leaving out files written within `settle`.
    fn save_settled(&self, settle: Duration, durable: bool) -> anyhow::Result<()> {
        let mut saved = self.saved.lock().unwrap_or_else(PoisonError::into_inner);
        let (snapshot, skip_bookkeeping) = {
            let mut inner = self.lock();
            inner.commit_written(settle);
            // Nothing changed since the last save took its snapshot, and that save
            // succeeded (a failed one sets `unsaved` again): its index still holds.
            // Rewriting it would cost a pass over every change, at every `fsync` and fork.
            // Replaced files still waiting to be deleted take a save, as before.
            if !inner.unsaved && !inner.writing && !saved.deleting_later() {
                drop(inner);
                return if durable { saved.flush() } else { Ok(()) };
            }
            (inner.snapshot(settle), inner.skip_bookkeeping)
        };
        saved.skip_bookkeeping = skip_bookkeeping;
        let result = saved.save(&snapshot, &self.root, durable);
        let mut inner = self.lock();
        // Saved files are read from disk from now on, unless written again meanwhile.
        for (path, version, stored) in saved.take_written() {
            let _ = inner.ws.store(path.as_str(), version, stored);
        }
        if result.is_err() {
            inner.unsaved = true;
        }
        result
    }

    /// Start a thread that commits and saves shortly after each burst of changes,
    /// until [`Shared::stop`].
    pub fn spawn_saver(self: &Arc<Self>) -> JoinHandle<()> {
        let shared = self.clone();
        std::thread::spawn(move || {
            loop {
                {
                    let mut inner = shared.lock();
                    while !inner.unsaved && !inner.writing && !shared.stop.load(Ordering::SeqCst) {
                        inner = shared.wake.wait(inner).unwrap_or_else(PoisonError::into_inner);
                    }
                }
                if shared.stop.load(Ordering::SeqCst) {
                    return;
                }
                // Let a burst of writes settle so it costs one save, not one per write.
                // A stop cuts the wait short: the final save after it does the work, and
                // an unmount waits for that save.
                let changed = {
                    let inner = shared.lock();
                    let stopping = || shared.stop.load(Ordering::SeqCst);
                    let (mut inner, _) = shared
                        .wake
                        .wait_timeout_while(inner, SETTLE, |_| !stopping())
                        .unwrap_or_else(PoisonError::into_inner);
                    if stopping() {
                        return;
                    }
                    inner.commit_written(SETTLE);
                    inner.unsaved
                };
                // Only files still being written: the next pass commits them.
                if !changed {
                    continue;
                }
                if let Err(err) = shared.save_settled(SETTLE, false) {
                    eprintln!("sandfs: saving changes failed: {err:#}");
                    std::thread::sleep(Duration::from_secs(1));
                }
            }
        })
    }

    pub fn stop(&self) {
        let _guard = self.lock();
        self.stop.store(true, Ordering::SeqCst);
        self.wake.notify_all();
    }

    /// Wake the saver if the last request left something unsaved. Most requests
    /// change nothing, and waking a thread costs more than the request itself.
    pub fn changed(&self) {
        let pending = {
            let inner = self.lock();
            inner.unsaved || inner.writing
        };
        if pending {
            self.wake.notify_one();
        }
    }
}

/// The [`Filesystem`] handed to fuser.
#[cfg(fuse)]
pub struct SandFs(pub Arc<Shared>);

#[cfg(fuse)]
impl SandFs {
    fn reply_entry(&self, reply: ReplyEntry, result: FsResult<FileAttr>) {
        self.0.changed();
        match result {
            Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
            Err(err) => reply.error(err),
        }
    }

    fn reply_empty(&self, reply: ReplyEmpty, result: FsResult<()>) {
        self.0.changed();
        match result {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(err),
        }
    }
}

#[cfg(fuse)]
impl SandFs {
    /// The handle a page of folder `ino` from `offset` is read through: the kernel's
    /// own, or where the kernel lists folders without opening them, one of
    /// [`Inner::listing`]'s.
    fn listing(&self, inner: &mut Inner, ino: u64, fh: u64, offset: u64) -> FsResult<u64> {
        if self.0.no_opendir.load(Ordering::SeqCst) { inner.listing(ino, offset) } else { Ok(fh) }
    }

    /// After a page of `added` entries: the kernel takes a listing as whole only after
    /// a page with nothing in it, so a handle of [`Inner::listing`]'s goes with that
    /// page, and until then the next page reuses it.
    fn end_listing(&self, inner: &mut Inner, ino: u64, added: usize) {
        if added == 0 && self.0.no_opendir.load(Ordering::SeqCst) {
            inner.end_listing(ino);
        }
    }
}

#[cfg(fuse)]
impl Filesystem for SandFs {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> std::io::Result<()> {
        // Listings carry attributes, so a walk that stats every entry (`ls -l`, `find`)
        // costs a round trip per page of entries instead of one per entry. Kernels
        // without it fall back to plain listings.
        let _ = config.add_capabilities(InitFlags::FUSE_DO_READDIRPLUS);
        let _ = config.add_capabilities(InitFlags::FUSE_READDIRPLUS_AUTO);
        // Opening a file costs a round trip and closing it another, which is half the
        // cost of reading a small file. With no-open the kernel skips both; files are
        // never told apart by handle here anyway.
        let no_open = config.add_capabilities(InitFlags::FUSE_NO_OPEN_SUPPORT).is_ok();
        self.0.no_open.store(no_open, Ordering::SeqCst);
        // The same for folders, and more: without an open the kernel keeps each
        // folder's listing once read, until something changes the folder through this
        // mount. Nothing else changes a mounted workspace, so a walk over folders already
        // seen (`find`, an editor's file tree, git looking for new files) sends nothing.
        let no_opendir = config.add_capabilities(InitFlags::FUSE_NO_OPENDIR_SUPPORT).is_ok();
        self.0.no_opendir.store(no_opendir, Ordering::SeqCst);
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        match self.0.lock().lookup(parent.0, name) {
            Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
            // A name that isn't there is cached too (inode 0 means "no such entry"), so
            // a program probing for files, like a module loader trying each extension,
            // asks once. Whatever creates the name goes through the kernel, which drops
            // the cached absence.
            Err(Errno::ENOENT) => reply.entry(&TTL, &missing(), Generation(0)),
            Err(err) => reply.error(err),
        }
    }

    fn forget(&self, _req: &Request, ino: INodeNo, nlookup: u64) {
        self.0.lock().forget(ino.0, nlookup);
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.0.lock().attr(ino.0) {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(err) => reply.error(err),
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        // Ownership and access times are not stored, so changing them is a no-op.
        let result = self.0.lock().setattr(ino.0, mode, size, mtime);
        self.0.changed();
        match result {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(err) => reply.error(err),
        }
    }

    fn mkdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, _mode: u32, _umask: u32, reply: ReplyEntry) {
        let result = self.0.lock().mkdir(parent.0, name);
        self.reply_entry(reply, result);
    }

    fn symlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, target: &std::path::Path, reply: ReplyEntry) {
        let result = self.0.lock().symlink(parent.0, name, target.as_os_str());
        self.reply_entry(reply, result);
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        match self.0.lock().readlink(ino.0) {
            Ok(target) => reply.data(target.as_bytes()),
            Err(err) => reply.error(err),
        }
    }

    fn link(&self, _req: &Request, ino: INodeNo, newparent: INodeNo, newname: &OsStr, reply: ReplyEntry) {
        let result = self.0.lock().link(ino.0, newparent.0, newname);
        self.reply_entry(reply, result);
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let result = self.0.lock().unlink(parent.0, name);
        self.reply_empty(reply, result);
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let result = self.0.lock().rmdir(parent.0, name);
        self.reply_empty(reply, result);
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let result = self.0.lock().rename(parent.0, name, newparent.0, newname, flags);
        self.reply_empty(reply, result);
    }

    /// Answered once at most: ENOSYS tells the kernel to stop sending opens and keep
    /// every file's cached pages, since nothing changes a file behind its back. Kernels
    /// without no-open get a handle that keeps no pages.
    fn open(&self, _req: &Request, _ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        if self.0.no_open.load(Ordering::SeqCst) {
            reply.error(Errno::ENOSYS);
        } else {
            reply.opened(FileHandle(0), FopenFlags::empty());
        }
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let reading = self.0.lock().reading(ino.0, offset, size);
        match reading.and_then(|reading| reading.read(offset, size)) {
            Ok(data) => reply.data(&data),
            Err(err) => reply.error(err),
        }
    }

    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let result = self.0.lock().write(ino.0, offset, data);
        self.0.changed();
        match result {
            Ok(written) => reply.written(written),
            Err(err) => reply.error(err),
        }
    }

    fn copy_file_range(
        &self,
        _req: &Request,
        ino_in: INodeNo,
        _fh_in: FileHandle,
        offset_in: u64,
        ino_out: INodeNo,
        _fh_out: FileHandle,
        offset_out: u64,
        len: u64,
        _flags: CopyFileRangeFlags,
        reply: ReplyWrite,
    ) {
        let result = self.0.lock().copy_range(ino_in.0, offset_in, ino_out.0, offset_out, len);
        self.0.changed();
        match result {
            Ok(copied) => reply.written(copied),
            Err(err) => reply.error(err),
        }
    }

    /// A close needs nothing from this filesystem: buffers are committed once they
    /// settle. ENOSYS tells the kernel to stop asking, which saves a round trip per close.
    fn flush(&self, _req: &Request, _ino: INodeNo, _fh: FileHandle, _lock_owner: LockOwner, reply: ReplyEmpty) {
        if self.0.no_open.load(Ordering::SeqCst) {
            reply.error(Errno::ENOSYS);
        } else {
            reply.ok();
        }
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        // Only files made by `create` (or opened on kernels without no-open) are
        // released, and their handles hold nothing.
        reply.ok();
    }

    fn fsync(&self, _req: &Request, _ino: INodeNo, _fh: FileHandle, _datasync: bool, reply: ReplyEmpty) {
        let result = self.0.save(true).map_err(|err| log_io(&err));
        self.reply_empty(reply, result);
    }

    /// Answered once at most where the kernel lists folders without opening them: see
    /// `init`. Listings then go through [`Inner::listing`].
    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        if self.0.no_opendir.load(Ordering::SeqCst) {
            return reply.error(Errno::ENOSYS);
        }
        match self.0.lock().opendir(ino.0) {
            Ok(fh) => reply.opened(FileHandle(fh), FopenFlags::empty()),
            Err(err) => reply.error(err),
        }
    }

    fn readdir(&self, _req: &Request, ino: INodeNo, fh: FileHandle, offset: u64, mut reply: ReplyDirectory) {
        let mut inner = self.0.lock();
        let fh = match self.listing(&mut inner, ino.0, fh.0, offset) {
            Ok(fh) => fh,
            Err(err) => return reply.error(err),
        };
        let mut added = 0;
        match inner.readdir(fh, offset) {
            Ok(entries) => {
                for (idx, (ino, kind, name)) in entries.iter().enumerate() {
                    if reply.add(INodeNo(*ino), offset + idx as u64 + 1, *kind, name) {
                        break;
                    }
                    added += 1;
                }
                reply.ok();
            }
            Err(err) => return reply.error(err),
        }
        self.end_listing(&mut inner, ino.0, added);
    }

    fn readdirplus(&self, _req: &Request, ino: INodeNo, fh: FileHandle, offset: u64, mut reply: ReplyDirectoryPlus) {
        let mut inner = self.0.lock();
        let fh = match self.listing(&mut inner, ino.0, fh.0, offset) {
            Ok(fh) => fh,
            Err(err) => return reply.error(err),
        };
        let mut added = 0;
        let result = inner.readdirplus(fh, offset, |ino, next, name, attr, ttl| {
            let full = reply.add(INodeNo(ino), next, name, &ttl, attr, Generation(0));
            added += usize::from(!full);
            full
        });
        match result {
            Ok(()) => reply.ok(),
            Err(err) => return reply.error(err),
        }
        self.end_listing(&mut inner, ino.0, added);
    }

    fn releasedir(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, _flags: OpenFlags, reply: ReplyEmpty) {
        self.0.lock().releasedir(fh.0);
        reply.ok();
    }

    /// `fsync` on a directory saves the whole workspace; the CLI uses it to make sure
    /// a fork or `changes` sees everything written so far.
    fn fsyncdir(&self, _req: &Request, _ino: INodeNo, _fh: FileHandle, _datasync: bool, reply: ReplyEmpty) {
        let result = self.0.save(true).map_err(|err| log_io(&err));
        self.reply_empty(reply, result);
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        // Files are written on the disk that holds the drafts, so report its room. A
        // mount without drafts writes into memory and reports plenty.
        // The workspace's state directory, which holds the drafts' directory and always
        // exists.
        let dir = self.0.lock().drafts.as_ref().and_then(|drafts| drafts.dir().parent().map(|dir| dir.to_path_buf()));
        let disk = dir.and_then(|dir| nix::sys::statvfs::statvfs(&dir).ok());
        match disk {
            Some(disk) => reply.statfs(
                disk.blocks(),
                disk.blocks_free(),
                disk.blocks_available(),
                disk.files(),
                disk.files_free(),
                u32::try_from(disk.block_size()).unwrap_or(4096),
                255,
                u32::try_from(disk.fragment_size()).unwrap_or(4096),
            ),
            None => {
                let blocks = 1 << 28;
                reply.statfs(blocks, blocks, blocks, 1 << 24, 1 << 24, 4096, 255, 4096);
            }
        }
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let exclusive = flags & libc::O_EXCL != 0;
        let truncate = flags & libc::O_TRUNC != 0;
        let result = self.0.lock().create(parent.0, name, mode, exclusive, truncate);
        self.0.changed();
        match result {
            // No FOPEN_KEEP_CACHE: an existing file emptied here drops its cached pages.
            Ok(attr) => reply.created(&TTL, &attr, Generation(0), FileHandle(0), FopenFlags::empty()),
            Err(err) => reply.error(err),
        }
    }
}

/// The file type and permission bits shown for an entry of `kind`.
fn file_type(kind: Kind) -> (FileType, u16) {
    match kind {
        Kind::Dir => (FileType::Directory, 0o755),
        Kind::File { executable: true } => (FileType::RegularFile, 0o755),
        Kind::File { executable: false } => (FileType::RegularFile, 0o644),
        Kind::Symlink => (FileType::Symlink, 0o777),
    }
}

#[cfg(fuse)]
fn log_io(err: &anyhow::Error) -> Errno {
    eprintln!("sandfs: saving changes failed: {err:#}");
    Errno::EIO
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use sandfs_core::MemorySource;

    use super::*;

    fn fs() -> Inner {
        let source =
            MemorySource::from_files([("README.md", "hello\n"), ("src/lib.rs", "fn one() {}\n"), ("src/util.rs", "")])
                .unwrap();
        let mut ws = Workspace::new();
        ws.mount("/mnt", Arc::new(source)).unwrap();
        let root = VPath::parse("/mnt").unwrap();
        Inner::new(ws, &root, BTreeMap::new(), BTreeSet::new(), SystemTime::UNIX_EPOCH, (1000, 1000))
    }

    fn name(s: &str) -> &OsStr {
        OsStr::new(s)
    }

    fn names(fs: &mut Inner, ino: u64) -> Vec<String> {
        let fh = fs.opendir(ino).unwrap();
        let names = fs.readdir(fh, 2).unwrap().iter().map(|(_, _, name)| name.clone()).collect();
        fs.releasedir(fh);
        names
    }

    #[test]
    fn lookups_and_attributes_come_from_the_workspace() {
        let mut fs = fs();
        let readme = fs.lookup(ROOT_INO, name("README.md")).unwrap();
        assert_eq!((readme.kind, readme.size, readme.perm), (FileType::RegularFile, 6, 0o644));
        let src = fs.lookup(ROOT_INO, name("src")).unwrap();
        assert_eq!(src.kind, FileType::Directory);
        assert_eq!(fs.lookup(ROOT_INO, name("README.md")).unwrap().ino, readme.ino, "inodes are stable");
        assert_eq!(fs.lookup(ROOT_INO, name("missing")).unwrap_err(), Errno::ENOENT);
        assert_eq!(names(&mut fs, ROOT_INO), ["README.md", "src"]);
    }

    /// Long enough that nothing written in a test has settled.
    const LATER: Duration = Duration::from_secs(3600);

    #[test]
    fn writes_are_committed_once_they_settle() {
        let mut fs = fs();
        let ino = fs.lookup(ROOT_INO, name("README.md")).unwrap().ino.0;
        fs.write(ino, 6, b"world\n").unwrap();
        assert!(fs.writing, "the saver has a buffer to come back for");
        assert_eq!(&fs.read(ino, 0, 100).unwrap()[..], b"hello\nworld\n", "reads see the buffer");
        assert_eq!(fs.attr(ino).unwrap().size, 12);
        fs.commit_written(LATER);
        assert_eq!(fs.ws.read_text("/mnt/README.md").unwrap(), "hello\n", "still being written");
        assert!(fs.writing && !fs.unsaved);

        fs.commit_written(Duration::ZERO);
        assert_eq!(fs.ws.read_text("/mnt/README.md").unwrap(), "hello\nworld\n");
        assert!(!fs.writing && fs.unsaved);
        fs.write(ino, 12, b"!\n").unwrap();
        assert_eq!(&fs.read(ino, 0, 100).unwrap()[..], b"hello\nworld\n!\n", "writes go on after a commit");
    }

    #[test]
    fn files_are_written_on_disk_where_there_are_drafts() {
        let state = tempfile::tempdir().unwrap();
        let mut saved = Saved::new(state.path());
        // Files big enough to keep an object of their own; small ones are packed.
        saved.pack_limit = None;
        let mut fs = fs();
        fs.drafts = Some(saved.drafts());
        let ino = fs.lookup(ROOT_INO, name("README.md")).unwrap().ino.0;
        fs.write(ino, 0, b"HE").unwrap();
        assert_eq!(&fs.read(ino, 0, 100).unwrap()[..], b"HEllo\n", "copied from the source, then written");
        fs.commit_written(Duration::ZERO);
        let stored = fs.ws.stored("/mnt/README.md").unwrap().expect("committed as the draft on disk");
        assert_eq!(std::fs::read(state.path().join("objects").join(&*stored.key)).unwrap(), b"HEllo\n");
        saved.save(&fs.snapshot(Duration::ZERO), &VPath::parse("/mnt").unwrap(), false).unwrap();
        assert!(saved.take_written().is_empty(), "the save writes nothing new");

        // A saved file written again goes to a new draft; the saved one is never changed.
        fs.write(ino, 2, b"LLO").unwrap();
        fs.commit_written(Duration::ZERO);
        assert_eq!(fs.ws.read_text("/mnt/README.md").unwrap(), "HELLO\n");
        assert_eq!(std::fs::read(state.path().join("objects").join(&*stored.key)).unwrap(), b"HEllo\n");

        // Sizes are the disk's to limit, not memory's.
        let huge = 5 << 30;
        assert_eq!(fs.setattr(ino, None, Some(huge), None).unwrap().size, huge);
        assert_eq!(&fs.read(ino, huge - 2, 10).unwrap()[..], b"\0\0");
        let mut in_memory = super::tests::fs();
        let ino = in_memory.lookup(ROOT_INO, name("README.md")).unwrap().ino.0;
        assert_eq!(in_memory.setattr(ino, None, Some(huge), None).unwrap_err(), Errno::EFBIG);

        // A first write that fails leaves nothing behind to commit.
        assert_eq!(in_memory.write(ino, huge, b"x").unwrap_err(), Errno::EFBIG);
        assert!(!in_memory.buffers.contains_key(&ino));
    }

    #[test]
    fn a_snapshot_leaves_out_files_still_being_written() {
        let mut fs = fs();
        let ino = fs.lookup(ROOT_INO, name("README.md")).unwrap().ino.0;
        fs.write(ino, 0, b"HELLO\n").unwrap();
        let snapshot = fs.snapshot(LATER);
        assert_eq!(snapshot.ws.read_text("/mnt/README.md").unwrap(), "hello\n");
        let snapshot = fs.snapshot(Duration::ZERO);
        assert_eq!(snapshot.ws.read_text("/mnt/README.md").unwrap(), "HELLO\n", "fsync and unmount take everything");
    }

    /// A source whose listings work but whose files can't be read.
    #[derive(Debug)]
    struct Unreadable(MemorySource);

    impl sandfs_core::Source for Unreadable {
        fn root(&self) -> sandfs_core::Result<sandfs_core::SourceKey> {
            self.0.root()
        }
        fn read_dir(&self, key: &str) -> sandfs_core::Result<sandfs_core::Listing> {
            self.0.read_dir(key)
        }
        fn read_file(&self, key: &str) -> sandfs_core::Result<Bytes> {
            Err(Error::Source(format!("{key} was read")))
        }
    }

    #[test]
    fn host_folder_files_are_read_in_ranges_and_never_kept() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notes.txt"), "before").unwrap();
        let mut ws = Workspace::new();
        ws.mount("/mnt", Arc::new(sandfs_core::DirSource::new(dir.path()).unwrap())).unwrap();
        let root = VPath::parse("/mnt").unwrap();
        let mut fs = Inner::new(ws, &root, BTreeMap::new(), BTreeSet::new(), SystemTime::UNIX_EPOCH, (1000, 1000));
        fs.ranged_reads = true;
        let ino = fs.lookup(ROOT_INO, name("notes.txt")).unwrap().ino.0;
        assert_eq!(&fs.read(ino, 2, 3).unwrap()[..], b"for");
        assert_eq!(&fs.read(ino, 0, 100).unwrap()[..], b"before");

        // The kernel caches what it read; the mount keeps no second copy, so a read the
        // kernel passes on goes back to the folder.
        std::fs::write(dir.path().join("notes.txt"), "after!").unwrap();
        assert_eq!(&fs.read(ino, 0, 100).unwrap()[..], b"after!");
    }

    /// A source that counts whole-file reads; a range reads the whole file, like git.
    #[derive(Debug)]
    struct Counting(MemorySource, std::sync::atomic::AtomicUsize);

    impl sandfs_core::Source for Counting {
        fn root(&self) -> sandfs_core::Result<sandfs_core::SourceKey> {
            self.0.root()
        }
        fn read_dir(&self, key: &str) -> sandfs_core::Result<sandfs_core::Listing> {
            self.0.read_dir(key)
        }
        fn read_file(&self, key: &str) -> sandfs_core::Result<Bytes> {
            self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.0.read_file(key)
        }
    }

    #[test]
    fn whole_file_sources_are_read_once_per_file_and_not_kept() {
        let files = (0..20).map(|i| (format!("f{i:02}"), b"0123456789".to_vec()));
        let source = Arc::new(Counting(MemorySource::from_files(files).unwrap(), Default::default()));
        let mut ws = Workspace::new();
        ws.mount("/mnt", source.clone()).unwrap();
        let root = VPath::parse("/mnt").unwrap();
        let mut fs = Inner::new(ws, &root, BTreeMap::new(), BTreeSet::new(), SystemTime::UNIX_EPOCH, (1000, 1000));
        let reads = || source.1.load(std::sync::atomic::Ordering::SeqCst);
        let inos: Vec<u64> = (0..20).map(|i| fs.lookup(ROOT_INO, name(&format!("f{i:02}"))).unwrap().ino.0).collect();

        // The kernel asks for a file in chunks: the source is read once for all of them.
        let chunks: Vec<u8> = (0..5).flat_map(|i| fs.read(inos[0], i * 2, 2).unwrap().to_vec()).collect();
        assert_eq!(chunks, b"0123456789");
        assert_eq!(reads(), 1);

        // Only the files read last are kept; the kernel caches the rest itself.
        for &ino in &inos {
            fs.read(ino, 0, 100).unwrap();
        }
        assert_eq!(reads(), 20);
        fs.read(inos[4], 0, 100).unwrap();
        assert_eq!(reads(), 20, "kept while it is among the last read");
        fs.read(inos[0], 0, 100).unwrap();
        assert_eq!(reads(), 21, "dropped once later files took its place");
        // Reading f04 again made it the latest read, so f05 went to make room instead.
        fs.read(inos[4], 0, 100).unwrap();
        assert_eq!(reads(), 21);
        fs.read(inos[5], 0, 100).unwrap();
        assert_eq!(reads(), 22);

        // A file written since it was kept is read as written.
        fs.write(inos[19], 0, b"new").unwrap();
        fs.commit_written(Duration::ZERO);
        assert_eq!(&fs.read(inos[19], 0, 100).unwrap()[..], b"new3456789");
    }

    #[test]
    fn emptying_a_file_does_not_read_it() {
        let source = Unreadable(MemorySource::from_files([("big.bin", vec![7u8; 1 << 20])]).unwrap());
        let mut ws = Workspace::new();
        ws.mount("/mnt", Arc::new(source)).unwrap();
        let root = VPath::parse("/mnt").unwrap();
        let mut fs = Inner::new(ws, &root, BTreeMap::new(), BTreeSet::new(), SystemTime::UNIX_EPOCH, (1000, 1000));
        let ino = fs.lookup(ROOT_INO, name("big.bin")).unwrap().ino.0;
        // O_TRUNC arrives as a truncate to zero.
        assert_eq!(fs.setattr(ino, None, Some(0), None).unwrap().size, 0);
        fs.commit_written(Duration::ZERO);
        assert_eq!(fs.ws.read("/mnt/big.bin").unwrap().len(), 0);
    }

    #[test]
    fn create_truncate_and_chmod() {
        let mut fs = fs();
        let attr = fs.create(ROOT_INO, name("run.sh"), 0o755, true, false).unwrap();
        assert_eq!(attr.perm, 0o755);
        fs.write(attr.ino.0, 0, b"#!/bin/sh\n").unwrap();
        assert_eq!(fs.create(ROOT_INO, name("run.sh"), 0o644, true, false).unwrap_err(), Errno::EEXIST);

        let attr = fs.setattr(attr.ino.0, Some(0o644), Some(2), None).unwrap();
        assert_eq!((attr.perm, attr.size), (0o644, 2));
        fs.commit_written(Duration::ZERO);
        assert_eq!(fs.ws.read_text("/mnt/run.sh").unwrap(), "#!");

        let attr = fs.create(ROOT_INO, name("run.sh"), 0o644, false, true).unwrap();
        assert_eq!(attr.size, 0, "an existing file opened with O_TRUNC");
        fs.commit_written(Duration::ZERO);
        assert_eq!(fs.ws.read_text("/mnt/run.sh").unwrap(), "");
    }

    #[test]
    fn copies_share_whole_files_and_write_pieces() {
        let mut fs = fs();
        let readme = fs.lookup(ROOT_INO, name("README.md")).unwrap().ino.0;
        let copy = fs.create(ROOT_INO, name("copy.md"), 0o644, true, false).unwrap().ino.0;
        assert_eq!(fs.copy_range(readme, 0, copy, 0, 1 << 30).unwrap(), 6);
        assert!(!fs.writing && fs.unsaved, "a whole copy needs no buffer");
        assert_eq!(fs.ws.read_text("/mnt/copy.md").unwrap(), "hello\n");
        assert_eq!(fs.attr(copy).unwrap().size, 6);

        // A piece goes through the buffer.
        assert_eq!(fs.copy_range(readme, 1, copy, 6, 3).unwrap(), 3);
        assert_eq!(&fs.read(copy, 0, 100).unwrap()[..], b"hello\nell");
        assert!(fs.writing);
        // A whole copy of a file still being written commits it and shares it.
        let again = fs.create(ROOT_INO, name("again.md"), 0o755, true, false).unwrap().ino.0;
        assert_eq!(fs.copy_range(copy, 0, again, 0, 100).unwrap(), 9);
        assert_eq!(fs.ws.read_text("/mnt/again.md").unwrap(), "hello\nell");
        assert_eq!(fs.ws.read_text("/mnt/copy.md").unwrap(), "hello\nell");
        assert_eq!(fs.attr(again).unwrap().perm, 0o755, "the destination keeps its mode");
        assert_eq!(fs.copy_range(copy, 100, again, 0, 100).unwrap(), 0, "past the end");
        assert_eq!(fs.ws.read_text("/mnt/README.md").unwrap(), "hello\n");
    }

    #[test]
    fn copies_of_saved_files_share_their_object() {
        let state = tempfile::tempdir().unwrap();
        let mut saved = Saved::new(state.path());
        // One object per file, so the files on disk count the objects.
        saved.pack_limit = None;
        let mut fs = fs();
        fs.drafts = Some(saved.drafts());
        let readme = fs.lookup(ROOT_INO, name("README.md")).unwrap().ino.0;
        fs.write(readme, 0, b"HE").unwrap();
        fs.commit_written(Duration::ZERO);
        saved.save(&fs.snapshot(Duration::ZERO), &VPath::parse("/mnt").unwrap(), false).unwrap();
        let stored = fs.ws.stored("/mnt/README.md").unwrap().unwrap();
        let objects = std::fs::read_dir(state.path().join("objects")).unwrap().count();

        let copy = fs.create(ROOT_INO, name("copy.md"), 0o644, true, false).unwrap().ino.0;
        assert_eq!(fs.copy_range(readme, 0, copy, 0, 1 << 30).unwrap(), 6);
        assert_eq!(fs.ws.stored("/mnt/copy.md").unwrap().unwrap().key, stored.key, "one object, two files");
        saved.save(&fs.snapshot(Duration::ZERO), &VPath::parse("/mnt").unwrap(), false).unwrap();
        assert_eq!(std::fs::read_dir(state.path().join("objects")).unwrap().count(), objects, "nothing new on disk");

        // Writing either one leaves the other as it was.
        fs.write(copy, 0, b"J").unwrap();
        fs.commit_written(Duration::ZERO);
        assert_eq!(fs.ws.read_text("/mnt/copy.md").unwrap(), "JEllo\n");
        assert_eq!(fs.ws.read_text("/mnt/README.md").unwrap(), "HEllo\n");
        saved.save(&fs.snapshot(Duration::ZERO), &VPath::parse("/mnt").unwrap(), false).unwrap();
        let mut restored = super::tests::fs().ws;
        Saved::new(state.path()).load(&mut restored, &VPath::parse("/mnt").unwrap()).unwrap();
        assert_eq!(restored.read_text("/mnt/README.md").unwrap(), "HEllo\n");
        assert_eq!(restored.read_text("/mnt/copy.md").unwrap(), "JEllo\n");
        // A file of the source goes to disk, in chunks, rather than being read whole at the next save.
        let src = fs.lookup(ROOT_INO, name("src")).unwrap().ino.0;
        let lib = fs.lookup(src, name("lib.rs")).unwrap().ino.0;
        let copied = fs.create(ROOT_INO, name("lib.rs"), 0o644, true, false).unwrap().ino.0;
        assert_eq!(fs.copy_range(lib, 0, copied, 0, 1 << 30).unwrap(), 12);
        assert!(fs.ws.stored("/mnt/lib.rs").unwrap().is_some());
        assert_eq!(fs.ws.read_text("/mnt/lib.rs").unwrap(), "fn one() {}\n");
    }

    #[test]
    fn an_fsync_with_nothing_changed_keeps_the_saved_index() {
        use std::os::unix::fs::MetadataExt;

        let state = tempfile::tempdir().unwrap();
        let mut saved = Saved::new(state.path());
        // One object per file, so the files on disk count the objects.
        saved.pack_limit = None;
        let mut inner = fs();
        inner.drafts = Some(saved.drafts());
        let shared = Shared::new(inner, saved, VPath::parse("/mnt").unwrap());
        let index = state.path().join("changes.json");
        let objects = || std::fs::read_dir(state.path().join("objects")).unwrap().count();
        let index_ino = || std::fs::metadata(&index).unwrap().ino();
        shared.save(true).unwrap();
        assert!(!index.exists(), "nothing to save yet");

        let readme = shared.lock().lookup(ROOT_INO, name("README.md")).unwrap().ino.0;
        shared.lock().write(readme, 0, b"HE").unwrap();
        shared.save(true).unwrap();
        let saved_ino = index_ino();
        // Each save puts a new index in place, so an unchanged inode means no save.
        shared.save(true).unwrap();
        shared.save(false).unwrap();
        assert_eq!(index_ino(), saved_ino, "the index is kept as it is");

        let new = shared.lock().create(ROOT_INO, name("new.md"), 0o644, true, false).unwrap().ino.0;
        shared.lock().write(new, 0, b"new\n").unwrap();
        shared.save(true).unwrap();
        assert_ne!(index_ino(), saved_ino, "a change is saved");

        // Writing a saved file again replaces its object, which a later save deletes,
        // even with nothing else changed by then.
        shared.lock().write(new, 0, b"NEW\n").unwrap();
        shared.save(true).unwrap();
        assert_eq!(objects(), 3, "the replaced object waits one save");
        shared.save(true).unwrap();
        assert_eq!(objects(), 2);
        let saved_ino = index_ino();
        shared.save(true).unwrap();
        assert_eq!(index_ino(), saved_ino, "and then nothing is left to do");

        // A draft no save named, such as one written to a removed file, is deleted too.
        let drafts = shared.lock().drafts.clone().unwrap();
        drop(drafts.empty().unwrap());
        assert_eq!(objects(), 3);
        shared.save(true).unwrap();
        assert_eq!(objects(), 2);
        let mut restored = fs().ws;
        Saved::new(state.path()).load(&mut restored, &VPath::parse("/mnt").unwrap()).unwrap();
        assert_eq!(restored.read_text("/mnt/README.md").unwrap(), "HEllo\n");
        assert_eq!(restored.read_text("/mnt/new.md").unwrap(), "NEW\n");
    }

    #[test]
    fn stopping_cuts_the_savers_settle_wait_short() {
        let state = tempfile::tempdir().unwrap();
        let saved = Saved::new(state.path());
        let mut inner = fs();
        inner.drafts = Some(saved.drafts());
        let shared = Shared::new(inner, saved, VPath::parse("/mnt").unwrap());
        let saver = shared.spawn_saver();
        let readme = shared.lock().lookup(ROOT_INO, name("README.md")).unwrap().ino.0;
        shared.lock().write(readme, 0, b"HE").unwrap();
        shared.changed();
        // Long enough for the saver to be waiting for the write to settle.
        std::thread::sleep(SETTLE / 5);
        let stopped = std::time::Instant::now();
        shared.stop();
        saver.join().unwrap();
        assert!(stopped.elapsed() < SETTLE / 2, "the saver took {:?} to stop", stopped.elapsed());
        // The final save, as at unmount, keeps the write.
        shared.save(true).unwrap();
        let mut restored = fs().ws;
        Saved::new(state.path()).load(&mut restored, &VPath::parse("/mnt").unwrap()).unwrap();
        assert_eq!(restored.read_text("/mnt/README.md").unwrap(), "HEllo\n");
    }

    #[test]
    fn a_failed_copy_keeps_the_destination_emptied() {
        let state = tempfile::tempdir().unwrap();
        let mut fs = fs();
        fs.drafts = Some(Saved::new(state.path()).drafts());
        fs.ws.write("/mnt/out.md", "old\n").unwrap();
        let out = fs.lookup(ROOT_INO, name("out.md")).unwrap().ino.0;
        // O_TRUNC, which only the buffer knows about so far.
        fs.setattr(out, None, Some(0), None).unwrap();
        // No room for drafts: the objects folder is a file.
        let objects = state.path().join("objects");
        let _ = std::fs::remove_dir_all(&objects);
        std::fs::write(&objects, "").unwrap();
        let readme = fs.lookup(ROOT_INO, name("README.md")).unwrap().ino.0;
        assert!(fs.copy_range(readme, 0, out, 0, 1 << 30).is_err());
        assert_eq!(fs.attr(out).unwrap().size, 0);
        assert_eq!(fs.read(out, 0, 100).unwrap().len(), 0, "the old bytes stay gone");
    }

    #[test]
    fn rename_replaces_and_moves_inodes() {
        let mut fs = fs();
        let src = fs.lookup(ROOT_INO, name("src")).unwrap().ino.0;
        let lib = fs.lookup(src, name("lib.rs")).unwrap().ino.0;
        fs.rename(src, name("lib.rs"), src, name("util.rs"), RenameFlags::empty()).unwrap();
        assert_eq!(names(&mut fs, src), ["util.rs"]);
        assert_eq!(fs.lookup(src, name("util.rs")).unwrap().ino.0, lib, "the inode follows the file");

        fs.rename(ROOT_INO, name("src"), ROOT_INO, name("code"), RenameFlags::empty()).unwrap();
        assert_eq!(fs.path(lib).unwrap().as_str(), "/mnt/code/util.rs");
        assert_eq!(fs.ws.read_text("/mnt/code/util.rs").unwrap(), "fn one() {}\n");
        assert!(fs.dirs.contains("/mnt/src") && fs.dirs.contains("/mnt/code"));

        let code = fs.lookup(ROOT_INO, name("code")).unwrap().ino.0;
        assert_eq!(
            fs.rename(ROOT_INO, name("code"), code, name("inside"), RenameFlags::empty()).unwrap_err(),
            Errno::EINVAL
        );
        assert_eq!(
            fs.rename(ROOT_INO, name("README.md"), ROOT_INO, name("code"), RenameFlags::empty()).unwrap_err(),
            Errno::EISDIR
        );
        #[cfg(target_os = "linux")]
        assert_eq!(
            fs.rename(
                code,
                name("util.rs"),
                ROOT_INO,
                name("README.md"),
                RenameFlags::from_bits_retain(RENAME_NOREPLACE)
            )
            .unwrap_err(),
            Errno::EEXIST
        );
    }

    #[test]
    fn removed_files_stay_readable_until_forgotten() {
        let mut fs = fs();
        let ino = fs.lookup(ROOT_INO, name("README.md")).unwrap().ino.0;
        fs.unlink(ROOT_INO, name("README.md")).unwrap();
        assert_eq!(fs.lookup(ROOT_INO, name("README.md")).unwrap_err(), Errno::ENOENT);
        // The kernel still holds the inode, so a handle may be open on it.
        assert_eq!(fs.attr(ino).unwrap().size, 6);
        assert_eq!(&fs.read(ino, 0, 100).unwrap()[..], b"hello\n");
        fs.write(ino, 0, b"HELLO\n").unwrap();
        assert_eq!(fs.setattr(ino, None, Some(2), None).unwrap().size, 2, "ftruncate on the open handle");
        assert_eq!(&fs.read(ino, 0, 100).unwrap()[..], b"HE");
        fs.commit_written(Duration::ZERO);
        assert!(!fs.ws.exists("/mnt/README.md"), "a removed file's writes go nowhere");
        assert!(!fs.writing, "and keep nothing waiting");
        assert_eq!(&fs.read(ino, 0, 100).unwrap()[..], b"HE");

        let link = fs.symlink(ROOT_INO, name("link"), OsStr::new("README.md")).unwrap().ino.0;
        fs.unlink(ROOT_INO, name("link")).unwrap();
        assert_eq!(fs.attr(link).unwrap().kind, FileType::Symlink, "a removed link is still a link");
        assert_eq!(fs.readlink(link).unwrap(), "README.md");

        fs.forget(ino, 1);
        assert!(!fs.nodes.contains_key(&ino) && !fs.buffers.contains_key(&ino), "closed once forgotten");
    }

    #[test]
    fn a_file_replaced_by_rename_stays_readable_until_forgotten() {
        let mut fs = fs();
        let src = fs.lookup(ROOT_INO, name("src")).unwrap().ino.0;
        let util = fs.lookup(src, name("util.rs")).unwrap().ino.0;
        fs.rename(src, name("lib.rs"), src, name("util.rs"), RenameFlags::empty()).unwrap();
        assert_eq!(&fs.read(util, 0, 100).unwrap()[..], b"", "the old util.rs");
        fs.forget(util, 1);
        assert!(!fs.nodes.contains_key(&util));
    }

    #[test]
    fn operations_that_would_follow_a_link_fail_with_eloop() {
        let mut ws = Workspace::new();
        ws.symlink("/link", "target").unwrap();
        assert_eq!(errno(&ws.write("/link", "x").unwrap_err()), Errno::ELOOP);
        assert_eq!(errno(&ws.read("/link").unwrap_err()), Errno::ELOOP);
        assert_eq!(errno(&ws.remove("/", true).unwrap_err()), Errno::EPERM, "other refusals stay EPERM");
    }

    #[test]
    fn a_link_has_no_size_to_set() {
        let mut fs = fs();
        let link = fs.symlink(ROOT_INO, name("link"), OsStr::new("README.md")).unwrap().ino.0;
        for size in [0, 3] {
            assert_eq!(fs.setattr(link, None, Some(size), None).unwrap_err(), Errno::EINVAL);
        }
        assert_eq!(fs.create(ROOT_INO, name("link"), 0o644, false, true).unwrap_err(), Errno::ELOOP);
        assert_eq!(fs.create(ROOT_INO, name("link"), 0o644, false, false).unwrap().ino.0, link);
        assert_eq!(fs.readlink(link).unwrap(), "README.md");
        assert!(fs.buffers.is_empty());

        // A link whose target can't be read now is still not a file to empty.
        #[derive(Debug)]
        struct Broken;
        impl sandfs_core::Source for Broken {
            fn root(&self) -> sandfs_core::Result<sandfs_core::SourceKey> {
                Ok("".into())
            }
            fn read_dir(&self, _key: &str) -> sandfs_core::Result<sandfs_core::Listing> {
                let entry =
                    sandfs_core::SourceEntry { kind: Kind::Symlink, key: "link".into(), size: Some(4), modified: None };
                Ok([("link".to_owned(), entry)].into())
            }
            fn read_file(&self, _key: &str) -> sandfs_core::Result<Bytes> {
                Err(Error::Source("gone".into()))
            }
        }
        let mut ws = Workspace::new();
        ws.mount("/mnt", Arc::new(Broken)).unwrap();
        let mut fs = Inner::new(
            ws,
            &VPath::parse("/mnt").unwrap(),
            BTreeMap::new(),
            BTreeSet::new(),
            SystemTime::UNIX_EPOCH,
            (1000, 1000),
        );
        let link = fs.lookup(ROOT_INO, name("link")).unwrap().ino.0;
        assert_eq!(fs.setattr(link, None, Some(0), None).unwrap_err(), Errno::EIO);
        assert!(fs.buffers.is_empty());
    }

    #[test]
    fn hard_links_are_one_inode_until_the_last_name_goes() {
        let mut fs = fs();
        let readme = fs.lookup(ROOT_INO, name("README.md")).unwrap().ino.0;
        let src = fs.lookup(ROOT_INO, name("src")).unwrap().ino.0;
        let attr = fs.link(readme, src, name("readme")).unwrap();
        assert_eq!((attr.ino.0, attr.nlink), (readme, 2));
        assert_eq!(attr.mtime, SystemTime::UNIX_EPOCH, "a new name leaves the file's time alone");
        assert_ne!(fs.attr(src).unwrap().mtime, SystemTime::UNIX_EPOCH, "its folder changed");
        assert_eq!(fs.lookup(src, name("readme")).unwrap().ino.0, readme);

        fs.write(readme, 0, b"HELLO").unwrap();
        fs.commit_written(Duration::ZERO);
        assert_eq!(fs.ws.read_text("/mnt/src/readme").unwrap(), "HELLO\n");
        let written = fs.times["/mnt/README.md"];
        assert_eq!(fs.times["/mnt/src/readme"], written, "every name has the time of the write");

        // Renaming one name onto the other changes nothing.
        fs.rename(ROOT_INO, name("README.md"), src, name("readme"), RenameFlags::empty()).unwrap();
        assert_eq!(fs.attr(readme).unwrap().nlink, 2);

        // The first name goes; the inode lives on under the second.
        fs.unlink(ROOT_INO, name("README.md")).unwrap();
        let attr = fs.attr(readme).unwrap();
        assert_eq!((attr.nlink, attr.size), (1, 6));
        assert_eq!(fs.read(readme, 0, 100).unwrap(), "HELLO\n");
        assert_eq!(fs.lookup(src, name("readme")).unwrap().ino.0, readme);
        assert!(matches!(fs.link(src, ROOT_INO, name("dir")), Err(Errno::EISDIR)));
    }

    #[test]
    fn directories_are_created_and_removed() {
        let mut fs = fs();
        let dir = fs.mkdir(ROOT_INO, name("new")).unwrap();
        assert_eq!(dir.kind, FileType::Directory);
        fs.mkdir(dir.ino.0, name("sub")).unwrap();
        assert_eq!(fs.rmdir(ROOT_INO, name("new")).unwrap_err(), Errno::ENOTEMPTY);
        fs.rmdir(dir.ino.0, name("sub")).unwrap();
        fs.rmdir(ROOT_INO, name("new")).unwrap();
        assert_eq!(fs.unlink(ROOT_INO, name("src")).unwrap_err(), Errno::EISDIR);
        assert_eq!(fs.rmdir(ROOT_INO, name("README.md")).unwrap_err(), Errno::ENOTDIR);
        assert!(fs.dirs.contains("/mnt/new"));
    }

    #[test]
    fn listings_with_attributes_count_lookups_and_resume() {
        let mut fs = fs();
        let fh = fs.opendir(ROOT_INO).unwrap();
        // Accept one entry past `.` and `..`, then report the reply full.
        let mut seen = Vec::new();
        fs.readdirplus(fh, 0, |_, next, name, _, _| {
            seen.push((next, name.to_owned()));
            seen.len() > 3
        })
        .unwrap();
        assert_eq!(seen.iter().map(|(_, name)| name.as_str()).collect::<Vec<_>>(), [".", "..", "README.md", "src"]);
        let resume = seen[2].0;
        let mut rest = Vec::new();
        fs.readdirplus(fh, resume, |_, _, name, attr, _| {
            rest.push((name.to_owned(), attr.kind));
            false
        })
        .unwrap();
        assert_eq!(rest, [("src".to_owned(), FileType::Directory)], "the refused entry comes again");
        fs.releasedir(fh);

        let readme = fs.lookup(ROOT_INO, name("README.md")).unwrap().ino.0;
        let src = fs.lookup(ROOT_INO, name("src")).unwrap().ino.0;
        assert_eq!(fs.nodes[&readme].lookups, 2, "one from the listing, one from lookup");
        assert_eq!(fs.nodes[&src].lookups, 2, "counted once, when accepted");
        assert_eq!(fs.nodes[&ROOT_INO].lookups, 0, ". and .. are not counted");
    }

    #[test]
    fn listings_with_attributes_leave_unknown_sizes_to_the_kernel() {
        /// A source that lists no sizes and counts the files it reads.
        #[derive(Debug)]
        struct Sizeless(MemorySource, std::sync::atomic::AtomicUsize);
        impl sandfs_core::Source for Sizeless {
            fn root(&self) -> sandfs_core::Result<sandfs_core::SourceKey> {
                self.0.root()
            }
            fn read_dir(&self, key: &str) -> sandfs_core::Result<sandfs_core::Listing> {
                let mut listing = self.0.read_dir(key)?;
                listing.values_mut().for_each(|entry| entry.size = None);
                Ok(listing)
            }
            fn read_file(&self, key: &str) -> sandfs_core::Result<bytes::Bytes> {
                self.1.fetch_add(1, Ordering::SeqCst);
                self.0.read_file(key)
            }
        }
        let source = Arc::new(Sizeless(
            MemorySource::from_files([("README.md", "hello\n"), ("src/lib.rs", "")]).unwrap(),
            Default::default(),
        ));
        let mut ws = Workspace::new();
        ws.mount("/mnt", source.clone()).unwrap();
        let root = VPath::parse("/mnt").unwrap();
        let mut fs = Inner::new(ws, &root, BTreeMap::new(), BTreeSet::new(), SystemTime::UNIX_EPOCH, (1000, 1000));
        fs.ws.hard_link("/mnt/README.md", "/mnt/README.txt").unwrap();
        let fh = fs.opendir(ROOT_INO).unwrap();
        let mut seen = Vec::new();
        fs.readdirplus(fh, 2, |ino, _, name, attr, ttl| {
            seen.push((name.to_owned(), ino, attr.size, ttl, attr.nlink));
            false
        })
        .unwrap();
        fs.releasedir(fh);
        let readme = seen.iter().find(|(name, ..)| name == "README.md").unwrap();
        assert_eq!((readme.2, readme.3), (0, Duration::ZERO), "no size yet, and nothing to keep");
        let other = seen.iter().find(|(name, ..)| name == "README.txt").unwrap();
        assert_eq!((readme.4, other.1, other.4), (2, readme.1, 2), "one inode with two names: {seen:?}");
        let src = seen.iter().find(|(name, ..)| name == "src").unwrap();
        assert_eq!(src.3, TTL);
        assert_eq!(source.1.load(Ordering::SeqCst), 0, "listing read no file");
        // The kernel asks again, and gets the real size.
        assert_eq!(fs.attr(readme.1).unwrap().size, 6);
        // Listing again must not shrink the file the kernel now knows to size 0 (a
        // program mapping it would crash), and finding its size again reads nothing.
        let fh = fs.opendir(ROOT_INO).unwrap();
        let mut again = Vec::new();
        fs.readdirplus(fh, 2, |_, _, name, attr, ttl| {
            again.push((name.to_owned(), attr.size, ttl));
            false
        })
        .unwrap();
        fs.releasedir(fh);
        assert!(again.contains(&("README.md".to_owned(), 6, TTL)), "{again:?}");
        assert_eq!(source.1.load(Ordering::SeqCst), 1, "only the first size read the file");
    }

    #[test]
    fn listings_with_attributes_never_publish_stale_names() {
        let mut fs = fs();
        let src = fs.lookup(ROOT_INO, name("src")).unwrap().ino.0;
        let fh = fs.opendir(ROOT_INO).unwrap();
        // After opendir, one child moves and the kernel forgets the other.
        fs.rename(ROOT_INO, name("README.md"), ROOT_INO, name("NOTES.md"), RenameFlags::empty()).unwrap();
        fs.forget(src, 1);
        let mut seen = Vec::new();
        fs.readdirplus(fh, 2, |ino, _, name, attr, _| {
            seen.push((name.to_owned(), ino, attr.kind));
            false
        })
        .unwrap();
        fs.releasedir(fh);
        assert_eq!(seen.len(), 1, "the moved name is left out: {seen:?}");
        let (name_seen, ino, kind) = &seen[0];
        assert_eq!((name_seen.as_str(), *kind), ("src", FileType::Directory), "the forgotten child is still listed");
        assert_eq!(fs.path(*ino).unwrap().as_str(), "/mnt/src");
        assert_eq!(fs.nodes[ino].lookups, 1);
    }

    #[test]
    fn unopened_listings_page_through_one_look_and_let_go_at_the_end() {
        let mut fs = fs();
        let fh = fs.listing(ROOT_INO, 0).unwrap();
        // A page that stops early keeps its handle for the next one, which sees the
        // listing as it was even if the folder changed in between.
        let first = fs.readdir(fh, 0).unwrap()[..3].to_vec();
        fs.create(ROOT_INO, name("late.txt"), 0o644, true, false).unwrap();
        assert_eq!(fs.listing(ROOT_INO, 3).unwrap(), fh);
        let rest: Vec<String> = fs.readdir(fh, 3).unwrap().iter().map(|(_, _, name)| name.clone()).collect();
        assert!(!rest.contains(&"late.txt".to_owned()), "{first:?} then {rest:?}");
        // Listing from the start takes a fresh look, and the old handle is let go.
        let again = fs.listing(ROOT_INO, 0).unwrap();
        assert_ne!(again, fh);
        assert!(!fs.handles.contains_key(&fh));
        assert!(fs.readdir(again, 0).unwrap().iter().any(|(_, _, name)| name == "late.txt"));
        fs.end_listing(ROOT_INO);
        assert!(fs.handles.is_empty() && fs.listings.is_empty() && !fs.open.contains_key(&ROOT_INO));
    }

    #[test]
    fn only_the_newest_unopened_listings_are_kept() {
        let mut fs = fs();
        let folders: Vec<u64> =
            (0..LISTINGS_KEPT + 6).map(|i| fs.mkdir(ROOT_INO, name(&format!("d{i}"))).unwrap().ino.0).collect();
        let first = fs.listing(folders[0], 0).unwrap();
        for &ino in &folders[1..] {
            fs.listing(ino, 0).unwrap();
        }
        assert_eq!((fs.listings.len(), fs.handles.len()), (LISTINGS_KEPT, LISTINGS_KEPT));
        assert!(!fs.handles.contains_key(&first), "the oldest listing was let go");
        // A reader going on with a listing that was let go gets a fresh one.
        assert_ne!(fs.listing(folders[0], 2).unwrap(), first);
    }

    #[test]
    fn an_open_folder_outlives_its_last_lookup() {
        let mut fs = fs();
        let src = fs.lookup(ROOT_INO, name("src")).unwrap().ino.0;
        let fh = fs.opendir(src).unwrap();
        fs.forget(src, 1);
        let mut names = Vec::new();
        fs.readdirplus(fh, 2, |_, _, name, _, _| {
            names.push(name.to_owned());
            false
        })
        .unwrap();
        assert!(!names.is_empty(), "the folder's entries are still listed");
        fs.releasedir(fh);
        assert!(!fs.nodes.contains_key(&src), "dropped once the handle is released");
    }

    #[test]
    fn a_forgotten_file_keeps_its_writes() {
        let mut fs = fs();
        let ino = fs.create(ROOT_INO, name("index.lock"), 0o644, true, false).unwrap().ino.0;
        fs.write(ino, 0, b"written").unwrap();
        // Renamed into place and forgotten before its writes settle, as git's index is
        // when the kernel is short of memory.
        fs.rename(ROOT_INO, name("index.lock"), ROOT_INO, name("index"), RenameFlags::empty()).unwrap();
        fs.forget(ino, 1);
        assert!(!fs.nodes.contains_key(&ino));
        assert_eq!(fs.ws.read_text("/mnt/index").unwrap(), "written");
        assert!(fs.unsaved, "a save picks it up");
    }

    #[test]
    fn forgotten_inodes_are_dropped() {
        let mut fs = fs();
        let ino = fs.lookup(ROOT_INO, name("README.md")).unwrap().ino.0;
        fs.lookup(ROOT_INO, name("README.md")).unwrap();
        fs.forget(ino, 1);
        assert!(fs.nodes.contains_key(&ino));
        fs.forget(ino, 1);
        assert!(!fs.nodes.contains_key(&ino) && !fs.inos.contains_key("/mnt/README.md"));
    }
}
