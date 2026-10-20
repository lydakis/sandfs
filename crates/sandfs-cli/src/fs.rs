//! The mounted filesystem: kernel requests turned into workspace calls.
//!
//! The kernel addresses files by inode number and the workspace by path, so [`Inner`]
//! keeps a table between the two and moves entries when something is renamed. Files
//! are never opened or closed here: the kernel is told it needn't ask (no-open mode),
//! which saves two round trips per file, so reads and writes name only the inode.
//! Writes land in a per-inode buffer that reaches the workspace once the file has gone
//! [`SETTLE`] without a write, or on `fsync`, so a file written in many small chunks is
//! copied once, not once per chunk. [`Shared`] adds a background thread that does that
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
use fuser::{Errno, FileAttr, FileType, INodeNo, RenameFlags, TimeOrNow};
#[cfg(fuse)]
use fuser::{
    FileHandle, Filesystem, FopenFlags, Generation, InitFlags, KernelConfig, LockOwner, OpenFlags, ReplyAttr,
    ReplyCreate, ReplyData, ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs,
    ReplyWrite, Request, WriteFlags,
};
use sandfs_core::{ChangeKind, Error, Kind, VPath, Workspace};

use crate::persist::{Saved, Snapshot};
use crate::store::subtree_keys;

/// How long the kernel may cache attributes and names. Every change goes through this
/// filesystem, so the kernel already sees its own updates and nothing goes stale;
/// a short TTL would only make it ask again (a round trip per path, per second).
#[cfg(any(fuse, test))]
const TTL: Duration = Duration::from_secs(60);
pub const ROOT_INO: u64 = 1;
/// Files live in memory, so refuse sizes that could only be a mistake.
pub const MAX_FILE_SIZE: usize = 4 << 30;
/// How many whole files, and bytes of them, [`Inner::read`] keeps for sources that
/// can't read a range on its own. The kernel asks for a file in chunks and caches
/// what it gets, so only the few files being read right now are worth keeping.
const RECENT_FILES: usize = 16;
const RECENT_BYTES: usize = 256 << 20;
/// How long a file goes without a write before its buffer is committed and saved.
/// Nothing tells this filesystem when a file is closed, so a quiet spell stands in.
pub const SETTLE: Duration = Duration::from_millis(250);

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
    #[cfg_attr(not(any(fuse, test)), allow(dead_code))]
    sizeless: HashMap<String, bool>,
}

/// A file's contents with writes the workspace hasn't seen yet.
struct Buffer {
    data: Vec<u8>,
    written: Instant,
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
    /// Open handles per directory.
    open: HashMap<u64, u32>,
    /// File contents written but not yet committed to the workspace.
    buffers: HashMap<u64, Buffer>,
    /// Modification times of touched paths; others report `base_time`.
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
    /// show up among the changes. See [`is_bookkeeping`].
    pub skip_bookkeeping: bool,
}

/// Files macOS creates next to the ones people write: `._name` holds `name`'s extended
/// attributes on filesystems without them (NFS), `.DS_Store` holds Finder's view
/// settings, and `.nfs.*` is a removed file that is still open.
pub fn is_bookkeeping(name: &str) -> bool {
    name.starts_with("._") || name == ".DS_Store" || name.starts_with(".nfs.")
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
            ws,
            nodes: HashMap::new(),
            inos: BTreeMap::new(),
            next_ino: ROOT_INO + 1,
            handles: HashMap::new(),
            next_fh: 1,
            open: HashMap::new(),
            buffers: HashMap::new(),
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
        let mut ws = self.ws.fork();
        if self.skip_bookkeeping {
            for change in ws.changes().unwrap_or_default() {
                if change.kind == ChangeKind::Added && change.path.name().is_some_and(is_bookkeeping) {
                    let _ = ws.remove(change.path.as_str(), true);
                }
            }
        }
        Snapshot { ws, times: self.times.clone(), dirs: self.dirs.clone() }
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
        let ino = self.next_ino;
        self.next_ino += 1;
        self.inos.insert(path.to_string(), ino);
        self.nodes.insert(
            ino,
            Node { path: path.clone(), lookups: 0, linked: true, sized: Cell::new(false), removed_from: None },
        );
        ino
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
            // A removed file the kernel may still have open.
            let size = match (self.buffers.get(&ino), &node.removed_from) {
                (Some(buffer), _) => buffer.data.len() as u64,
                (None, Some(before)) => before.stat(node.path.as_str()).fs()?.size,
                (None, None) => return Err(Errno::ENOENT),
            };
            node.sized.set(true);
            return Ok(self.make_attr(ino, FileType::RegularFile, 0o644, size, self.base_time));
        }
        let path = node.path.as_str();
        let meta = self.ws.stat(path).fs()?;
        node.sized.set(true);
        let size = match self.buffers.get(&ino) {
            Some(buffer) => buffer.data.len() as u64,
            None if meta.kind.is_dir() => 0,
            None => meta.size,
        };
        let (kind, perm) = match meta.kind {
            Kind::Dir => (FileType::Directory, 0o755),
            Kind::File { executable: true } => (FileType::RegularFile, 0o755),
            Kind::File { executable: false } => (FileType::RegularFile, 0o644),
            Kind::Symlink => (FileType::Symlink, 0o777),
        };
        let mtime = self.times.get(path).copied().unwrap_or(self.base_time);
        Ok(self.make_attr(ino, kind, perm, size, mtime))
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
        Ok(self.make_attr(ino, FileType::RegularFile, if executable { 0o755 } else { 0o644 }, 0, mtime))
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
            if let Some(node) = self.nodes.remove(&ino)
                && self.inos.get(node.path.as_str()) == Some(&ino)
            {
                self.inos.remove(node.path.as_str());
            }
            self.buffers.remove(&ino);
        }
    }

    /// Mark the inode at `path` as removed. A removed file keeps `before`, the
    /// workspace from before the removal, until the kernel forgets it, so reads and
    /// writes through handles still open on it keep working. Forking is O(1).
    fn unlink_path(&mut self, path: &VPath, before: Option<Workspace>) {
        let Some(ino) = self.inos.remove(path.as_str()) else { return };
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
        if let Some(size) = size {
            let size = usize::try_from(size).ok().filter(|size| *size <= MAX_FILE_SIZE).ok_or(Errno::EFBIG)?;
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
                self.buffers.insert(ino, Buffer { data: Vec::new(), written: Instant::now() });
                self.writing = true;
            } else {
                self.buffer(ino)?.resize(size, 0);
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
                // Only the executable bit is kept; other mode bits are not stored.
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
            self.times.insert(path.to_string(), time);
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
        let path = self.path(ino)?;
        self.ws.read_link(path.as_str()).map_err(|err| match err {
            Error::Unsupported(..) => Errno::EINVAL,
            err => errno(&err),
        })
    }

    pub fn unlink(&mut self, parent: u64, name: &OsStr) -> FsResult<()> {
        let path = self.child(parent, name)?;
        if self.ws.stat(path.as_str()).fs()?.kind.is_dir() {
            return Err(Errno::EISDIR);
        }
        let before = self.ws.fork();
        self.ws.remove(path.as_str(), false).fs()?;
        self.unlink_path(&path, Some(before));
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
        self.unlink_path(&path, None);
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
        if replacing {
            next.remove(to.as_str(), true).fs()?;
        }
        next.rename(from.as_str(), to.as_str()).fs()?;
        if replacing {
            let before = (!moving_dir).then(|| self.ws.fork());
            self.unlink_path(&to, before);
            self.times.remove(to.as_str());
        }
        self.ws = next;

        self.move_subtree(&from, &to);
        if moving_dir {
            // Empty directories inside the moved one are invisible to `changes`.
            let mut stack = vec![to.clone()];
            while let Some(dir) = stack.pop() {
                for entry in self.ws.list(dir.as_str()).fs()? {
                    if entry.kind.is_dir() {
                        stack.push(dir.join(&entry.name).map_err(|_| Errno::EINVAL)?);
                    }
                }
                self.dirs.insert(dir.to_string());
            }
            self.dirs.insert(from.to_string());
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
                    let ino = self.ino_for(&path);
                    self.buffers.insert(ino, Buffer { data: Vec::new(), written: Instant::now() });
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

    pub fn read(&self, ino: u64, offset: u64, size: u32) -> FsResult<Bytes> {
        let range = |len: usize| {
            let start = usize::try_from(offset).unwrap_or(usize::MAX).min(len);
            start..start.saturating_add(size as usize).min(len)
        };
        if let Some(buffer) = self.buffers.get(&ino) {
            return Ok(Bytes::copy_from_slice(&buffer.data[range(buffer.data.len())]));
        }
        let (ws, path) = self.contents(ino)?;
        // Saved files are on disk, where reading a range is cheap.
        if self.ranged_reads || ws.stored(path.as_str()).is_ok_and(|stored| stored.is_some()) {
            return ws.read_range(path.as_str(), offset, size.into()).fs();
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
        Ok(data.slice(range(data.len())))
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

    /// The write buffer for `ino`, filled from the workspace on first use.
    fn buffer(&mut self, ino: u64) -> FsResult<&mut Vec<u8>> {
        if !self.buffers.contains_key(&ino) {
            let (ws, path) = self.contents(ino)?;
            // Check before reading: a large source file would otherwise be read whole.
            let size = ws.stat(path.as_str()).fs()?.size;
            if size > MAX_FILE_SIZE as u64 {
                return Err(Errno::EFBIG);
            }
            // A ranged read keeps no copy of a base file in the workspace's cache.
            let data = ws.read_range(path.as_str(), 0, size).fs()?.to_vec();
            self.buffers.insert(ino, Buffer { data, written: Instant::now() });
        }
        self.writing = true;
        let buffer = self.buffers.get_mut(&ino).ok_or(Errno::EIO)?;
        buffer.written = Instant::now();
        Ok(&mut buffer.data)
    }

    pub fn write(&mut self, ino: u64, offset: u64, data: &[u8]) -> FsResult<u32> {
        let start = usize::try_from(offset).map_err(|_| Errno::EFBIG)?;
        let end = start.checked_add(data.len()).filter(|end| *end <= MAX_FILE_SIZE).ok_or(Errno::EFBIG)?;
        let buffer = self.buffer(ino)?;
        if buffer.len() < end {
            buffer.resize(end, 0);
        }
        buffer[start..end].copy_from_slice(data);
        if let Ok(path) = self.path(ino) {
            self.times.insert(path.to_string(), SystemTime::now());
        }
        Ok(u32::try_from(data.len()).unwrap_or(u32::MAX))
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
            let _ = self.ws.write(path.as_str(), Bytes::from(buffer.data)).fs();
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
        let snapshot = self.lock().snapshot(settle);
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
                std::thread::sleep(SETTLE);
                let changed = {
                    let mut inner = shared.lock();
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
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        match self.0.lock().lookup(parent.0, name) {
            Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
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
        match self.0.lock().read(ino.0, offset, size) {
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

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        match self.0.lock().opendir(ino.0) {
            Ok(fh) => reply.opened(FileHandle(fh), FopenFlags::empty()),
            Err(err) => reply.error(err),
        }
    }

    fn readdir(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, offset: u64, mut reply: ReplyDirectory) {
        let inner = self.0.lock();
        match inner.readdir(fh.0, offset) {
            Ok(entries) => {
                for (idx, (ino, kind, name)) in entries.iter().enumerate() {
                    if reply.add(INodeNo(*ino), offset + idx as u64 + 1, *kind, name) {
                        break;
                    }
                }
                reply.ok();
            }
            Err(err) => reply.error(err),
        }
    }

    fn readdirplus(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, offset: u64, mut reply: ReplyDirectoryPlus) {
        let result = self.0.lock().readdirplus(fh.0, offset, |ino, next, name, attr, ttl| {
            reply.add(INodeNo(ino), next, name, &ttl, attr, Generation(0))
        });
        match result {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(err),
        }
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
        // Writes live in memory, so report plenty of room rather than a real disk.
        let blocks = 1 << 28;
        reply.statfs(blocks, blocks, blocks, 1 << 24, 1 << 24, 4096, 255, 4096);
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
                let entry = sandfs_core::SourceEntry { kind: Kind::Symlink, key: "link".into(), size: Some(4) };
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
        let fh = fs.opendir(ROOT_INO).unwrap();
        let mut seen = Vec::new();
        fs.readdirplus(fh, 2, |ino, _, name, attr, ttl| {
            seen.push((name.to_owned(), ino, attr.size, ttl));
            false
        })
        .unwrap();
        fs.releasedir(fh);
        let readme = seen.iter().find(|(name, ..)| name == "README.md").unwrap();
        assert_eq!((readme.2, readme.3), (0, Duration::ZERO), "no size yet, and nothing to keep");
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
