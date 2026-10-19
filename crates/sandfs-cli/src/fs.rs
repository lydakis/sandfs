//! The FUSE filesystem: kernel requests turned into workspace calls.
//!
//! The kernel addresses files by inode number and the workspace by path, so [`Inner`]
//! keeps a table between the two and moves entries when something is renamed. Writes
//! land in a per-inode buffer and reach the workspace when the file is flushed (on
//! every `close`), so a file written in many small chunks is copied once, not once per
//! chunk. [`Shared`] adds a background thread that saves the workspace's changes
//! shortly after they happen, and the [`fuser::Filesystem`] impl at the bottom only
//! locks, calls into [`Inner`] and replies.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use fuser::{
    Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo, InitFlags, KernelConfig,
    LockOwner, OpenAccMode, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
    ReplyDirectoryPlus, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request, TimeOrNow, WriteFlags,
};
use sandfs_core::{Error, Kind, VPath, Workspace};

use crate::persist::{Saved, Snapshot};
use crate::store::subtree_keys;

/// How long the kernel may cache attributes and names. Every change goes through this
/// filesystem, so the kernel already sees its own updates and nothing goes stale;
/// a short TTL would only make it ask again (a round trip per path, per second).
const TTL: Duration = Duration::from_secs(60);
const ROOT_INO: u64 = 1;
/// Files live in memory, so refuse sizes that could only be a mistake.
const MAX_FILE_SIZE: usize = 4 << 30;
/// Base files larger than this are read in ranges rather than whole.
const RANGED_READ_SIZE: u64 = 64 << 20;

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
        Error::Unsupported(_, reason) if reason.contains("symbolic links") => Errno::ELOOP,
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
}

enum Handle {
    File { ino: u64 },
    Dir { ino: u64, entries: Vec<(u64, FileType, String)> },
}

/// The filesystem state, behind one lock.
pub struct Inner {
    ws: Workspace,
    nodes: HashMap<u64, Node>,
    /// Path to inode, ordered so a renamed directory's entries are one range.
    inos: BTreeMap<String, u64>,
    next_ino: u64,
    handles: HashMap<u64, Handle>,
    next_fh: u64,
    /// Open handles per inode.
    open: HashMap<u64, u32>,
    /// File contents written but not yet flushed to the workspace.
    buffers: HashMap<u64, Vec<u8>>,
    /// The file version each inode had when it was last opened. A file opened again
    /// at the same version keeps the kernel's page cache instead of reading it again.
    opened_versions: HashMap<u64, u64>,
    /// Modification times of touched paths; others report `base_time`.
    times: BTreeMap<String, SystemTime>,
    /// Directories created or removed here, which a save must record.
    dirs: BTreeSet<String>,
    base_time: SystemTime,
    uid: u32,
    gid: u32,
    /// Set by every change, cleared when a save takes a snapshot.
    unsaved: bool,
    /// Read large base files in ranges. Only worth it for sources that can seek (a
    /// host folder): git reads a whole blob for every range.
    pub ranged_reads: bool,
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
            opened_versions: HashMap::new(),
            times,
            dirs,
            base_time,
            uid,
            gid,
            unsaved: false,
            ranged_reads: false,
        };
        inner.inos.insert(root.to_string(), ROOT_INO);
        inner.nodes.insert(ROOT_INO, Node { path: root.clone(), lookups: 0, linked: true });
        inner
    }

    /// A copy of what a save needs; the workspace part is an O(1) fork.
    pub fn snapshot(&mut self) -> Snapshot {
        self.unsaved = false;
        Snapshot { ws: self.ws.fork(), times: self.times.clone(), dirs: self.dirs.clone() }
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
        self.nodes.insert(ino, Node { path: path.clone(), lookups: 0, linked: true });
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
            // A removed file that is still open lives on in its buffer.
            let size = self.buffers.get(&ino).ok_or(Errno::ENOENT)?.len() as u64;
            return Ok(self.make_attr(ino, FileType::RegularFile, 0o644, size, self.base_time));
        }
        let path = node.path.as_str();
        let meta = self.ws.stat(path).fs()?;
        let size = match self.buffers.get(&ino) {
            Some(buffer) => buffer.len() as u64,
            None if meta.kind.is_dir() => 0,
            None => meta.size,
        };
        let (kind, perm) = match meta.kind {
            Kind::Dir => (FileType::Directory, 0o755),
            Kind::File { executable: true } => (FileType::RegularFile, 0o755),
            Kind::File { executable: false } => (FileType::RegularFile, 0o644),
        };
        let mtime = self.times.get(path).copied().unwrap_or(self.base_time);
        Ok(self.make_attr(ino, kind, perm, size, mtime))
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

    /// Drop an inode the kernel no longer holds and nothing has open.
    fn maybe_drop(&mut self, ino: u64) {
        let unused = self.nodes.get(&ino).is_some_and(|node| node.lookups == 0) && !self.is_open(ino);
        if unused && ino != ROOT_INO {
            if let Some(node) = self.nodes.remove(&ino)
                && self.inos.get(node.path.as_str()) == Some(&ino)
            {
                self.inos.remove(node.path.as_str());
            }
            self.buffers.remove(&ino);
            self.opened_versions.remove(&ino);
        }
    }

    /// Mark the inode at `path` as removed. If it is open, its contents move into a
    /// buffer first, so reads and writes through open handles keep working.
    fn unlink_path(&mut self, path: &VPath) {
        let Some(ino) = self.inos.remove(path.as_str()) else { return };
        if self.is_open(ino)
            && !self.buffers.contains_key(&ino)
            && let Ok(data) = self.ws.read(path.as_str())
        {
            self.buffers.insert(ino, data.to_vec());
        }
        if let Some(node) = self.nodes.get_mut(&ino) {
            node.linked = false;
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
            self.buffer(ino)?.resize(size, 0);
            // ftruncate on a removed file that is still open changes only its buffer.
            if !self.nodes.get(&ino).is_some_and(|node| node.linked) {
                return self.attr(ino);
            }
            let path = self.path(ino)?;
            self.touch(&path);
            // truncate(2) on a file nobody has open: there will be no flush to wait for.
            if !self.is_open(ino) {
                self.commit(ino)?;
            }
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

    pub fn unlink(&mut self, parent: u64, name: &OsStr) -> FsResult<()> {
        let path = self.child(parent, name)?;
        if self.ws.stat(path.as_str()).fs()?.kind.is_dir() {
            return Err(Errno::EISDIR);
        }
        // Keep open contents before the workspace forgets them.
        let ino = self.inos.get(path.as_str()).copied();
        if let Some(ino) = ino.filter(|ino| self.is_open(*ino) && !self.buffers.contains_key(ino)) {
            let data = self.ws.read(path.as_str()).fs()?;
            self.buffers.insert(ino, data.to_vec());
        }
        self.ws.remove(path.as_str(), false).fs()?;
        self.unlink_path(&path);
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
        self.unlink_path(&path);
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
        if flags.contains(RenameFlags::RENAME_EXCHANGE) || flags.contains(RenameFlags::RENAME_WHITEOUT) {
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
                if flags.contains(RenameFlags::RENAME_NOREPLACE) {
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
            self.unlink_path(&to);
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

    /// Create and open a file. With `exclusive` an existing file is an error; otherwise
    /// it is opened, and emptied when `truncate` is set.
    pub fn create(
        &mut self,
        parent: u64,
        name: &OsStr,
        mode: u32,
        exclusive: bool,
        truncate: bool,
    ) -> FsResult<(FileAttr, u64)> {
        let path = self.child(parent, name)?;
        match self.ws.stat(path.as_str()) {
            Ok(_) if exclusive => return Err(Errno::EEXIST),
            Ok(meta) if meta.kind.is_dir() => return Err(Errno::EISDIR),
            Ok(_) => {
                if truncate {
                    let ino = self.ino_for(&path);
                    self.buffers.insert(ino, Vec::new());
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
        let attr = self.entry(&path)?;
        let (fh, _) = self.open(attr.ino.0, false)?;
        Ok((attr, fh))
    }

    /// Open a file. The flag says whether the kernel may keep the pages it cached
    /// for this inode: only when the file's version is the one it had at the last
    /// open, since every change in between gets a new version. Files read in ranges
    /// go back to the host folder on every open instead, so changes made there show.
    pub fn open(&mut self, ino: u64, truncate: bool) -> FsResult<(u64, bool)> {
        let path = self.path(ino)?;
        if truncate {
            self.buffers.insert(ino, Vec::new());
            self.touch(&path);
        }
        let meta = self.ws.stat(path.as_str()).fs()?;
        let unchanged = self.opened_versions.insert(ino, meta.version) == Some(meta.version);
        let ranged = self.ranged_reads && meta.size > RANGED_READ_SIZE;
        let keep_cache = unchanged && !ranged && !self.buffers.contains_key(&ino);
        let fh = self.next_fh;
        self.next_fh += 1;
        self.handles.insert(fh, Handle::File { ino });
        *self.open.entry(ino).or_default() += 1;
        Ok((fh, keep_cache))
    }

    pub fn read(&self, ino: u64, offset: u64, size: u32) -> FsResult<Bytes> {
        let range = |len: usize| {
            let start = usize::try_from(offset).unwrap_or(usize::MAX).min(len);
            start..start.saturating_add(size as usize).min(len)
        };
        match self.buffers.get(&ino) {
            Some(buffer) => Ok(Bytes::copy_from_slice(&buffer[range(buffer.len())])),
            None => {
                let path = self.path(ino)?;
                if self.ranged_reads && self.ws.stat(path.as_str()).fs()?.size > RANGED_READ_SIZE {
                    return self.ws.read_range(path.as_str(), offset, size.into()).fs();
                }
                // Other files are read whole once and served from the workspace's cache.
                let data = self.ws.read(path.as_str()).fs()?;
                Ok(data.slice(range(data.len())))
            }
        }
    }

    /// The write buffer for `ino`, filled from the workspace on first use.
    fn buffer(&mut self, ino: u64) -> FsResult<&mut Vec<u8>> {
        if !self.buffers.contains_key(&ino) {
            let path = self.path(ino)?;
            // Check before reading: a large source file would otherwise be read whole.
            if self.ws.stat(path.as_str()).fs()?.size > MAX_FILE_SIZE as u64 {
                return Err(Errno::EFBIG);
            }
            let data = self.ws.read(path.as_str()).fs()?;
            self.buffers.insert(ino, data.to_vec());
        }
        self.buffers.get_mut(&ino).ok_or(Errno::EIO)
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

    /// Move a file's write buffer into the workspace. A removed file's buffer stays
    /// where it is: it belongs to nobody but the handles still open on it.
    /// Commit every open file's buffer, so a save includes writes not yet flushed.
    pub fn commit_all(&mut self) -> FsResult<()> {
        let inos: Vec<u64> = self.buffers.keys().copied().collect();
        inos.into_iter().try_for_each(|ino| self.commit(ino))
    }

    pub fn commit(&mut self, ino: u64) -> FsResult<()> {
        let Ok(path) = self.path(ino) else { return Ok(()) };
        let Some(buffer) = self.buffers.remove(&ino) else { return Ok(()) };
        self.ws.write(path.as_str(), Bytes::from(buffer)).fs()?;
        self.unsaved = true;
        Ok(())
    }

    pub fn release(&mut self, fh: u64) -> FsResult<()> {
        let Some(Handle::File { ino }) = self.handles.remove(&fh) else { return Ok(()) };
        let result = self.commit(ino);
        if let Some(count) = self.open.get_mut(&ino) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.open.remove(&ino);
                self.buffers.remove(&ino);
            }
        }
        self.maybe_drop(ino);
        result
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
        for entry in listing {
            let child = path.join(&entry.name).map_err(|_| Errno::EIO)?;
            let kind = match entry.kind {
                Kind::Dir => FileType::Directory,
                Kind::File { .. } => FileType::RegularFile,
            };
            entries.push((self.ino_for(&child), kind, entry.name));
        }
        let fh = self.next_fh;
        self.next_fh += 1;
        self.handles.insert(fh, Handle::Dir { ino, entries });
        // Like a file handle, the listing keeps its folder's inode until releasedir,
        // since readdirplus resolves names through it.
        *self.open.entry(ino).or_default() += 1;
        Ok(fh)
    }

    /// Directory entries from position `offset` on; each entry's offset is its
    /// position plus one, which is where the next call resumes.
    pub fn readdir(&self, fh: u64, offset: u64) -> FsResult<&[(u64, FileType, String)]> {
        match self.handles.get(&fh) {
            Some(Handle::Dir { entries, .. }) => {
                Ok(entries.get(usize::try_from(offset).unwrap_or(usize::MAX)..).unwrap_or_default())
            }
            _ => Err(Errno::EBADF),
        }
    }

    /// Like [`Inner::readdir`], with each entry's attributes, handed to `add` until it
    /// reports the reply full. The kernel counts every entry it accepts as a lookup,
    /// except `.` and `..`, so those entries count here too.
    pub fn readdirplus(
        &mut self,
        fh: u64,
        offset: u64,
        mut add: impl FnMut(u64, u64, &str, &FileAttr) -> bool,
    ) -> FsResult<()> {
        let mut position = usize::try_from(offset).unwrap_or(usize::MAX);
        loop {
            let Some(Handle::Dir { ino: dir, entries }) = self.handles.get(&fh) else { return Err(Errno::EBADF) };
            let dir = *dir;
            let Some((saved, _, name)) = entries.get(position).cloned() else { return Ok(()) };
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
            let Ok(attr) = self.attr(ino) else {
                self.maybe_drop(ino);
                continue;
            };
            if add(ino, position as u64, &name, &attr) {
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
        let Some(Handle::Dir { ino, .. }) = self.handles.remove(&fh) else { return };
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
}

impl Shared {
    pub fn new(inner: Inner, saved: Saved, root: VPath) -> Arc<Self> {
        Arc::new(Shared {
            inner: Mutex::new(inner),
            saved: Mutex::new(saved),
            root,
            wake: Condvar::new(),
            stop: AtomicBool::new(false),
        })
    }

    pub fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Save the workspace's changes now; `durable` also flushes them to disk, as `fsync` promises.
    pub fn save(&self, durable: bool) -> anyhow::Result<()> {
        let mut saved = self.saved.lock().unwrap_or_else(PoisonError::into_inner);
        let snapshot = self.lock().snapshot();
        let result = saved.save(&snapshot, &self.root, durable);
        if result.is_err() {
            self.lock().unsaved = true;
        }
        result
    }

    /// Start a thread that saves shortly after each burst of changes, until [`Shared::stop`].
    pub fn spawn_saver(self: &Arc<Self>) -> JoinHandle<()> {
        let shared = self.clone();
        std::thread::spawn(move || {
            loop {
                {
                    let mut inner = shared.lock();
                    while !inner.unsaved && !shared.stop.load(Ordering::SeqCst) {
                        inner = shared.wake.wait(inner).unwrap_or_else(PoisonError::into_inner);
                    }
                }
                if shared.stop.load(Ordering::SeqCst) {
                    return;
                }
                // Let a burst of writes settle so it costs one save, not one per write.
                std::thread::sleep(Duration::from_millis(250));
                if let Err(err) = shared.save(false) {
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
    /// (a close after a read) change nothing, and waking a thread costs more than the
    /// request itself.
    fn changed(&self) {
        if self.lock().unsaved {
            self.wake.notify_one();
        }
    }
}

/// The [`Filesystem`] handed to fuser.
pub struct SandFs(pub Arc<Shared>);

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

impl Filesystem for SandFs {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> std::io::Result<()> {
        // Listings carry attributes, so a walk that stats every entry (`ls -l`, `find`)
        // costs a round trip per page of entries instead of one per entry. Kernels
        // without it fall back to plain listings.
        let _ = config.add_capabilities(InitFlags::FUSE_DO_READDIRPLUS);
        let _ = config.add_capabilities(InitFlags::FUSE_READDIRPLUS_AUTO);
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

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let writable = flags.acc_mode() != OpenAccMode::O_RDONLY;
        match self.0.lock().open(ino.0, writable && flags.0 & libc::O_TRUNC != 0) {
            Ok((fh, keep_cache)) => {
                let mut reply_flags = FopenFlags::empty();
                reply_flags.set(FopenFlags::FOPEN_KEEP_CACHE, keep_cache);
                // A read-only handle has nothing to commit when it is closed, so the
                // kernel can skip the flush round trip.
                reply_flags.set(FopenFlags::FOPEN_NOFLUSH, !writable);
                reply.opened(FileHandle(fh), reply_flags)
            }
            Err(err) => reply.error(err),
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
        match self.0.lock().write(ino.0, offset, data) {
            Ok(written) => reply.written(written),
            Err(err) => reply.error(err),
        }
    }

    fn flush(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, _lock_owner: LockOwner, reply: ReplyEmpty) {
        let result = self.0.lock().commit(ino.0);
        self.reply_empty(reply, result);
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let result = self.0.lock().release(fh.0);
        self.reply_empty(reply, result);
    }

    fn fsync(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, _datasync: bool, reply: ReplyEmpty) {
        let result = self.0.lock().commit(ino.0);
        let result = result.and_then(|()| self.0.save(true).map_err(|err| log_io(&err)));
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
        let result = self.0.lock().readdirplus(fh.0, offset, |ino, next, name, attr| {
            reply.add(INodeNo(ino), next, name, &TTL, attr, Generation(0))
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
        let committed = self.0.lock().commit_all();
        let result = committed.and_then(|()| self.0.save(true).map_err(|err| log_io(&err)));
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
            Ok((attr, fh)) => reply.created(&TTL, &attr, Generation(0), FileHandle(fh), FopenFlags::empty()),
            Err(err) => reply.error(err),
        }
    }
}

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

    #[test]
    fn writes_are_buffered_until_flush() {
        let mut fs = fs();
        let ino = fs.lookup(ROOT_INO, name("README.md")).unwrap().ino.0;
        let (fh, _) = fs.open(ino, false).unwrap();
        fs.write(ino, 6, b"world\n").unwrap();
        assert_eq!(fs.ws.read_text("/mnt/README.md").unwrap(), "hello\n", "not flushed yet");
        assert_eq!(&fs.read(ino, 0, 100).unwrap()[..], b"hello\nworld\n", "reads see the buffer");
        assert_eq!(fs.attr(ino).unwrap().size, 12);
        fs.release(fh).unwrap();
        assert_eq!(fs.ws.read_text("/mnt/README.md").unwrap(), "hello\nworld\n");
        assert!(fs.unsaved);
    }

    #[test]
    fn the_kernel_keeps_its_cache_only_for_unchanged_files() {
        let mut fs = fs();
        let ino = fs.lookup(ROOT_INO, name("README.md")).unwrap().ino.0;
        let reopen = |fs: &mut Inner| {
            let (fh, keep) = fs.open(ino, false).unwrap();
            fs.release(fh).unwrap();
            keep
        };
        assert!(!reopen(&mut fs), "nothing is cached on the first open");
        assert!(reopen(&mut fs), "same version");
        let (fh, _) = fs.open(ino, false).unwrap();
        fs.write(ino, 0, b"HELLO\n").unwrap();
        fs.release(fh).unwrap();
        assert!(!reopen(&mut fs), "written since the last open");
        assert!(reopen(&mut fs));
        let (fh, keep) = fs.open(ino, true).unwrap();
        assert!(!keep, "truncated");
        fs.release(fh).unwrap();
    }

    #[test]
    fn syncing_the_directory_commits_open_files() {
        let mut fs = fs();
        let ino = fs.lookup(ROOT_INO, name("README.md")).unwrap().ino.0;
        let (fh, _) = fs.open(ino, false).unwrap();
        fs.write(ino, 6, b"world\n").unwrap();
        fs.commit_all().unwrap();
        assert_eq!(fs.ws.read_text("/mnt/README.md").unwrap(), "hello\nworld\n");
        fs.write(ino, 12, b"!\n").unwrap();
        assert_eq!(&fs.read(ino, 0, 100).unwrap()[..], b"hello\nworld\n!\n", "writes go on after a commit");
        fs.release(fh).unwrap();
        assert_eq!(fs.ws.read_text("/mnt/README.md").unwrap(), "hello\nworld\n!\n");
    }

    #[test]
    fn create_truncate_and_chmod() {
        let mut fs = fs();
        let (attr, fh) = fs.create(ROOT_INO, name("run.sh"), 0o755, true, false).unwrap();
        assert_eq!(attr.perm, 0o755);
        fs.write(attr.ino.0, 0, b"#!/bin/sh\n").unwrap();
        fs.release(fh).unwrap();
        assert_eq!(fs.create(ROOT_INO, name("run.sh"), 0o644, true, false).unwrap_err(), Errno::EEXIST);

        let attr = fs.setattr(attr.ino.0, Some(0o644), Some(2), None).unwrap();
        assert_eq!((attr.perm, attr.size), (0o644, 2));
        assert_eq!(fs.ws.read_text("/mnt/run.sh").unwrap(), "#!", "truncate without an open file commits");
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
        assert_eq!(
            fs.rename(code, name("util.rs"), ROOT_INO, name("README.md"), RenameFlags::RENAME_NOREPLACE).unwrap_err(),
            Errno::EEXIST
        );
    }

    #[test]
    fn removed_files_stay_readable_while_open() {
        let mut fs = fs();
        let ino = fs.lookup(ROOT_INO, name("README.md")).unwrap().ino.0;
        let (fh, _) = fs.open(ino, false).unwrap();
        fs.unlink(ROOT_INO, name("README.md")).unwrap();
        assert_eq!(fs.lookup(ROOT_INO, name("README.md")).unwrap_err(), Errno::ENOENT);
        assert_eq!(&fs.read(ino, 0, 100).unwrap()[..], b"hello\n");
        fs.write(ino, 0, b"HELLO\n").unwrap();
        assert_eq!(fs.setattr(ino, None, Some(2), None).unwrap().size, 2, "ftruncate on the open handle");
        assert_eq!(&fs.read(ino, 0, 100).unwrap()[..], b"HE");
        fs.release(fh).unwrap();
        assert!(!fs.ws.exists("/mnt/README.md"), "a removed file's writes go nowhere");
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
        fs.readdirplus(fh, 0, |_, next, name, _| {
            seen.push((next, name.to_owned()));
            seen.len() > 3
        })
        .unwrap();
        assert_eq!(seen.iter().map(|(_, name)| name.as_str()).collect::<Vec<_>>(), [".", "..", "README.md", "src"]);
        let resume = seen[2].0;
        let mut rest = Vec::new();
        fs.readdirplus(fh, resume, |_, _, name, attr| {
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
    fn listings_with_attributes_never_publish_stale_names() {
        let mut fs = fs();
        let src = fs.lookup(ROOT_INO, name("src")).unwrap().ino.0;
        let fh = fs.opendir(ROOT_INO).unwrap();
        // After opendir, one child moves and the kernel forgets the other.
        fs.rename(ROOT_INO, name("README.md"), ROOT_INO, name("NOTES.md"), RenameFlags::empty()).unwrap();
        fs.forget(src, 1);
        let mut seen = Vec::new();
        fs.readdirplus(fh, 2, |ino, _, name, attr| {
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
        fs.readdirplus(fh, 2, |_, _, name, _| {
            names.push(name.to_owned());
            false
        })
        .unwrap();
        assert!(!names.is_empty(), "the folder's entries are still listed");
        fs.releasedir(fh);
        assert!(!fs.nodes.contains_key(&src), "dropped once the handle is released");
    }

    #[test]
    fn only_unchanged_whole_read_files_keep_their_pages() {
        let big = vec![7u8; RANGED_READ_SIZE as usize + 1];
        let source = MemorySource::from_files([("small.txt", b"hi".to_vec()), ("big.bin", big)]).unwrap();
        let mut ws = Workspace::new();
        ws.mount("/mnt", Arc::new(source)).unwrap();
        let root = VPath::parse("/mnt").unwrap();
        let mut fs = Inner::new(ws, &root, BTreeMap::new(), BTreeSet::new(), SystemTime::UNIX_EPOCH, (1000, 1000));
        fs.ranged_reads = true;
        let mut reopen = |file: &str| {
            let ino = fs.lookup(ROOT_INO, name(file)).unwrap().ino.0;
            let keep: Vec<bool> = (0..2)
                .map(|_| {
                    let (fh, keep) = fs.open(ino, false).unwrap();
                    fs.release(fh).unwrap();
                    keep
                })
                .collect();
            keep
        };
        assert_eq!(reopen("small.txt"), [false, true]);
        // Ranged reads go back to the host folder on every open.
        assert_eq!(reopen("big.bin"), [false, false]);
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
