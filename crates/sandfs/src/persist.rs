//! Saving a mounted workspace's changes to disk and replaying them on the next mount.
//!
//! The base of a workspace never needs saving: it is a pinned git commit or a host
//! directory. What is saved is the overlay, as one entry per changed path in
//! `changes.json` plus one file per saved file body under `objects/`. A save rewrites
//! the small index but writes an object only for files whose version changed since the
//! last save, so saving costs the index plus whatever was written in between.
//!
//! The entries come from [`Workspace::changes`] (files) plus the directories the mount
//! created or removed, which `changes` does not report. Only the first save of a
//! process goes over every change: each later one starts from what the save before it
//! saw and looks again only at the files changed since ([`Workspace::changes_since`]),
//! so after a build a save costs what the next edit changed, not the whole build
//! output.
//!
//! Saved files stay on disk. A loaded workspace reads them from their objects (see
//! [`Workspace::write_stored`]), and once a save has written a file's object the
//! serving process reads it from there too ([`Workspace::store`]), so what a mount
//! has saved costs it no memory; the operating system caches the objects in use.
//!
//! A mount writes a file straight into a new object, a [`Draft`], instead of into
//! memory, and hands it to the workspace as a stored file once the writes settle. The
//! next save names that object as it is. From then on the object is never written
//! again: a later write copies it into a new draft, which the disk can often do
//! without copying any bytes.
//!
//! Small files are the exception: a save writes them, drafts included, into one pack,
//! an object holding many files back to back, and names each by its range of the
//! pack. A fork then links one file per pack rather than one per small file, which
//! after `npm install` or a build is the difference between thousands of links and a
//! handful. A pack goes once no file reads from it, and a save writes the files of a
//! mostly unused pack, or of many small packs, into its own pack again.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use sandfs_core::{ChangeKind, Error, Kind, Listing, Source, SourceKey, Stored, VPath, Workspace};
use serde::{Deserialize, Serialize};

/// Files macOS creates next to the ones people write: `._name` holds `name`'s extended
/// attributes on filesystems without them (NFS), `.DS_Store` holds Finder's view
/// settings, and `.nfs.*` is a removed file that is still open.
pub fn is_bookkeeping(name: &str) -> bool {
    name.starts_with("._") || name == ".DS_Store" || name.starts_with(".nfs.")
}

/// A point in time as `(seconds, nanoseconds)` since the Unix epoch.
type Time = (u64, u32);

/// The saved state of one changed path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "lowercase")]
enum Entry {
    /// A regular file whose bytes are in `objects/<object>`.
    File {
        executable: bool,
        object: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mtime: Option<Time>,
        /// Another name of the same file (a hard link), earlier in the index or
        /// unchanged in the source. Loading links the two instead of reading the object.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        link: Option<String>,
    },
    Dir {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mtime: Option<Time>,
    },
    /// A symbolic link, its target kept in the index since it is short.
    Symlink {
        target: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mtime: Option<Time>,
        /// Another name of the same link, as for a file.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        link: Option<String>,
    },
    /// Removed: a deleted base file or directory.
    Absent,
}

/// What a saved path is: a file, a directory, or removed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathState {
    File,
    Dir,
    Absent,
}

/// `changes.json`: entries keyed by path relative to the mount root, so parents sort
/// before their children and replay can go in order.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Changes {
    entries: BTreeMap<String, Entry>,
    /// The next object name. Names are never reused, even for objects that have since
    /// been deleted, so a reader holding an older index finds an object gone rather
    /// than replaced by unrelated bytes.
    #[serde(default)]
    next: u64,
}

/// What a save needs, taken under the filesystem lock. The workspace is a fork, so
/// taking it is O(1) and the save can run while the mount keeps changing.
pub struct Snapshot {
    pub ws: Workspace,
    /// Modification times of touched paths.
    pub times: BTreeMap<String, SystemTime>,
    /// Directories created or removed through the mount.
    pub dirs: BTreeSet<String>,
}

/// What [`Saved::load`] restores besides the workspace contents.
#[derive(Debug, Default)]
pub struct Restored {
    pub times: BTreeMap<String, SystemTime>,
    pub dirs: BTreeSet<String>,
}

/// A saved object the index names has been deleted: a newer save replaced it while
/// the index was being loaded.
#[derive(Debug)]
pub struct ObjectGone(String);

impl std::fmt::Display for ObjectGone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "saved object {} {GONE}", self.0)
    }
}

impl std::error::Error for ObjectGone {}

const GONE: &str = "is gone; a newer save replaced it";

/// Files a save wrote objects for, by workspace path, with the version saved and where
/// its bytes are now, so the serving process can read them from there (see
/// [`Workspace::store`]) instead of keeping them in memory.
pub type Written = Vec<(VPath, u64, Stored)>;

/// The objects of one workspace, as the source its saved files are read from. It is
/// never mounted: files point into it by object name.
#[derive(Debug)]
struct Objects {
    dir: PathBuf,
    /// The next object name, shared by saves and drafts.
    next: AtomicU64,
    /// Drafts made since the last save, which that save starts tracking.
    drafts: Mutex<Vec<SourceKey>>,
}

impl Objects {
    /// A name no object of this workspace has had.
    fn new_name(&self) -> SourceKey {
        self.next.fetch_add(1, Ordering::SeqCst).to_string().into()
    }

    /// The file holding `key` and the range of it that is `key`'s bytes, if not all.
    fn open(&self, key: &str) -> sandfs_core::Result<(std::fs::File, Option<(u64, u64)>)> {
        let place = place(key).map_err(|err| Error::Source(err.to_string()))?;
        let file = std::fs::File::open(self.dir.join(place.file)).map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => Error::Source(ObjectGone(key.to_owned()).to_string()),
            _ => Error::Source(format!("reading saved object {key}: {err}")),
        })?;
        Ok((file, place.range))
    }
}

impl Source for Objects {
    fn root(&self) -> sandfs_core::Result<SourceKey> {
        Ok("".into())
    }

    fn read_dir(&self, _key: &str) -> sandfs_core::Result<Listing> {
        Err(Error::Source("saved objects are read by name, never listed".into()))
    }

    fn read_file(&self, key: &str) -> sandfs_core::Result<Bytes> {
        self.read_range(key, 0, u64::MAX)
    }

    fn read_range(&self, key: &str, offset: u64, len: u64) -> sandfs_core::Result<Bytes> {
        let (mut file, range) = self.open(key)?;
        // A packed object is a range of its pack, read through the page cache like
        // any other file.
        let (start, len) = match range {
            None => (offset, len),
            Some((at, size)) => (at.saturating_add(offset.min(size)), len.min(size.saturating_sub(offset))),
        };
        let mut data = Vec::new();
        file.seek(SeekFrom::Start(start))
            .and_then(|_| file.take(len).read_to_end(&mut data))
            .map_err(|err| Error::Source(err.to_string()))?;
        Ok(data.into())
    }
}

/// Where a mount writes files: each into a new object of the workspace, a [`Draft`],
/// unless it is small enough that a save packs it (see [`Drafts::in_memory`]).
#[derive(Clone, Debug)]
pub struct Drafts(Arc<Objects>, Option<u64>);

/// Bytes read into a draft at a time, from a file that cannot be copied on disk.
const CHUNK: u64 = 8 << 20;

impl Drafts {
    /// Whether a file of `size` bytes is written in memory instead of into a draft: a
    /// save packs it, reading it back, so a draft of its own would only cost making and
    /// deleting a file on disk for each one written.
    pub fn in_memory(&self, size: u64) -> bool {
        self.1.is_some_and(|limit| size <= limit)
    }

    /// A new, empty draft.
    pub fn empty(&self) -> Result<Draft> {
        let key = self.0.new_name();
        std::fs::create_dir_all(&self.0.dir).context("creating the objects directory")?;
        let file = std::fs::File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(self.0.dir.join(&*key))
            .context("creating a file to write into")?;
        Ok(self.track(file, key))
    }

    /// A new draft holding what `path` in `ws` holds now. A saved file of its own is
    /// copied on disk, which shares its blocks where the disk can (APFS, btrfs, XFS);
    /// a small one in a pack is read like a source file. Any other
    /// file is read in chunks when its source reads ranges cheaply (`ranged`), so its
    /// size never sits in memory; otherwise it is read once, whole, since each chunk
    /// would read the whole file again.
    pub fn copy(&self, ws: &Workspace, path: &str, ranged: bool) -> Result<Draft> {
        if let Some(stored) = ws.stored(path)?
            && std::ptr::addr_eq(Arc::as_ptr(&stored.source), Arc::as_ptr(&self.0))
            && !stored.key.contains('@')
        {
            let key = self.0.new_name();
            let to = self.0.dir.join(&*key);
            match std::fs::copy(self.0.dir.join(&*stored.key), &to) {
                Ok(_) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    let _ = std::fs::remove_file(&to);
                    return Err(ObjectGone(stored.key.to_string()).into());
                }
                Err(err) => {
                    let _ = std::fs::remove_file(&to);
                    return Err(err).context("copying a saved file to write into");
                }
            }
            let file = std::fs::File::options().read(true).write(true).open(&to)?;
            return Ok(self.track(file, key));
        }
        let size = ws.stat(path)?.size;
        let chunk = if ranged { CHUNK } else { size };
        let mut draft = self.empty()?;
        let fill = |draft: &mut Draft| -> Result<()> {
            let mut offset = 0;
            while offset < size {
                let data = ws.read_range(path, offset, chunk.min(size - offset))?;
                if data.is_empty() {
                    break;
                }
                // Zeros are left as a hole, so a sparse file stays sparse on disk.
                if data.iter().any(|byte| *byte != 0) {
                    draft.write_all_at(offset, &data)?;
                }
                offset += data.len() as u64;
            }
            Ok(draft.set_len(offset)?)
        };
        // A copy cut short (a full disk) is deleted now: no save may come to collect
        // it, and it would hold the room a retry needs.
        if let Err(err) = fill(&mut draft) {
            let _ = std::fs::remove_file(self.0.dir.join(&*draft.key));
            return Err(err);
        }
        Ok(draft)
    }

    /// The directory drafts are written in.
    #[cfg(fuse)]
    pub fn dir(&self) -> &Path {
        &self.0.dir
    }

    fn track(&self, file: std::fs::File, key: SourceKey) -> Draft {
        self.0.drafts.lock().unwrap_or_else(PoisonError::into_inner).push(key.clone());
        Draft { file, key, store: self.0.clone() }
    }
}

/// A file being written, as a new object on disk. [`Draft::finish`] turns it into
/// a stored file for the workspace; a draft dropped unfinished is deleted by a later
/// save.
#[derive(Debug)]
pub struct Draft {
    file: std::fs::File,
    key: SourceKey,
    store: Arc<Objects>,
}

impl Draft {
    /// Write `data` at `offset`, growing the file if it ends past it, and return how
    /// many bytes were written. An error after some were written (a disk filling up)
    /// is a short write, as `write(2)` reports it, so the caller knows the file
    /// changed.
    pub fn write_at(&mut self, offset: u64, data: &[u8]) -> std::io::Result<usize> {
        self.file.seek(SeekFrom::Start(offset))?;
        let mut written = 0;
        while written < data.len() {
            match self.file.write(&data[written..]) {
                Ok(0) => break,
                Ok(n) => written += n,
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) if written > 0 => break,
                Err(err) => return Err(err),
            }
        }
        if written == 0 && !data.is_empty() {
            return Err(std::io::ErrorKind::WriteZero.into());
        }
        Ok(written)
    }

    /// Write all of `data` at `offset`, or fail.
    fn write_all_at(&mut self, offset: u64, data: &[u8]) -> std::io::Result<()> {
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(data)
    }

    /// Up to `len` bytes from `offset`; fewer at the end of the file.
    pub fn read_at(&self, offset: u64, len: u64) -> std::io::Result<Bytes> {
        let (mut file, mut data) = (&self.file, Vec::new());
        file.seek(SeekFrom::Start(offset))?;
        file.take(len).read_to_end(&mut data)?;
        Ok(data.into())
    }

    /// Cut or extend the file to `size`; an extension reads as zeros.
    pub fn set_len(&self, size: u64) -> std::io::Result<()> {
        self.file.set_len(size)
    }

    pub fn len(&self) -> std::io::Result<u64> {
        Ok(self.file.metadata()?.len())
    }

    /// The finished file, for [`Workspace::write_stored`]. Its object is never written
    /// again.
    pub fn finish(self) -> std::io::Result<Stored> {
        let size = self.len()?;
        Ok(Stored { source: self.store, key: self.key, size })
    }
}

/// Files a durable save flushes one by one; more are flushed with one call for the
/// whole disk.
#[cfg(target_os = "linux")]
const SYNC_ONE_BY_ONE: usize = 64;

/// Flush the whole disk holding `dir` in one call when `files` files need flushing
/// and one call each would cost more, and say whether it did. After `npm install`
/// wrote 6,200 files, flushing them one by one made a fork take 0.8 s; one call
/// makes it 0.35 s. The call also flushes what other programs wrote to that disk.
#[cfg(target_os = "linux")]
fn sync_disk(dir: &Path, files: usize) -> Result<bool> {
    if files <= SYNC_ONE_BY_ONE || !syncfs_reports_errors() {
        return Ok(false);
    }
    let dir = std::fs::File::open(dir).context("flushing saved files")?;
    nix::unistd::syncfs(&dir).context("flushing saved files")?;
    Ok(true)
}

/// Whether `syncfs` reports a file that failed to reach the disk, as it does from
/// Linux 5.8. Before that it succeeds anyway, so a save flushes file by file.
#[cfg(target_os = "linux")]
fn syncfs_reports_errors() -> bool {
    static REPORTS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *REPORTS.get_or_init(|| {
        let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
        kernel_at_least(&release, (5, 8))
    })
}

/// Whether a kernel release such as `6.1.0-13-amd64` is `want` or newer.
#[cfg(target_os = "linux")]
fn kernel_at_least(release: &str, want: (u32, u32)) -> bool {
    let mut parts = release.trim().split(['.', '-']).map(|part| part.parse::<u32>().ok());
    match (parts.next().flatten(), parts.next().flatten()) {
        (Some(major), Some(minor)) => (major, minor) >= want,
        _ => false,
    }
}

#[cfg(not(target_os = "linux"))]
fn sync_disk(_dir: &Path, _files: usize) -> Result<bool> {
    Ok(false)
}

/// What is at `path` before `entry` is replayed there.
fn current_kind(ws: &Workspace, path: &VPath, entry: &Entry) -> Result<Option<Kind>> {
    match ws.stat(path.as_str()) {
        Ok(meta) => Ok(Some(meta.kind)),
        // Below a file that replaced a directory: the file already hides it.
        Err(Error::NotFound(_) | Error::NotADirectory(_)) if *entry == Entry::Absent => Ok(None),
        Err(Error::NotFound(_)) => Ok(None),
        Err(err) => Err(err.into()),
    }
}

/// Whether `err` is a saved object found missing, by a load or a read of a saved file,
/// because a newer save replaced it: reading again from the newer index works.
pub fn is_object_gone(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause.downcast_ref::<ObjectGone>().is_some()
            || cause.downcast_ref::<Error>().is_some_and(|err| err.to_string().contains(GONE))
    })
}

/// A save wrote its new index, so it is what the next load sees, but could not flush
/// it to disk: a crash could still lose it.
#[derive(Debug)]
pub struct NotDurable;

impl std::fmt::Display for NotDurable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the changes were saved but could not be flushed to disk")
    }
}

/// Files of at most this size are saved into the save's pack, one file holding many,
/// so that a fork links one file per pack instead of one per file. Bigger files keep
/// a file each, which a write into them can clone.
const PACK_LIMIT: u64 = 64 << 10;

/// A pack: one file in `objects/` holding the bytes of many small saved files.
#[derive(Debug, Default)]
struct Pack {
    /// Its size on disk.
    size: u64,
    /// Every object name handed out for a range of it. Files reading from the pack
    /// share these, so their counts say whether any still does.
    objects: Vec<SourceKey>,
    /// No index named it at the last save. It is deleted at the next save that finds
    /// nothing reading from it, so a reader of the index before still finds it.
    unnamed: bool,
}

/// Packs at most this big are small: a save repacks small packs once there are more
/// than [`SMALL_PACKS`], so their number stays bounded however many saves there are.
const SMALL_PACK: u64 = 1 << 20;
const SMALL_PACKS: usize = 16;

/// The pack a save is writing, opened at its first packed file.
struct PackWriter {
    file: std::io::BufWriter<std::fs::File>,
    name: SourceKey,
    size: u64,
    /// The object names handed out for it.
    objects: Vec<SourceKey>,
    unfinished: Unfinished,
}

/// A file a failed save leaves behind, deleted when dropped unless the save finished it.
struct Unfinished(Option<PathBuf>);

impl Unfinished {
    fn finished(mut self) {
        self.0 = None;
    }
}

impl Drop for Unfinished {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// What a save has done so far.
struct Saving {
    /// Each saved file's version and object.
    kept: HashMap<String, (u64, SourceKey)>,
    written: Written,
    pack: Option<PackWriter>,
    /// The objects the last index named.
    before: HashSet<String>,
    /// Packs whose files this save writes again; see [`Saved::packs_to_repack`].
    repack: HashSet<String>,
    /// Objects this save wrote again into its pack, with their new names.
    packed: HashMap<String, SourceKey>,
    /// See [`Saved::pack_limit`].
    pack_limit: Option<u64>,
}

impl Saving {
    /// Whether this save writes the stored file `stored` into its pack: a small draft
    /// no index has named yet, or a file of a pack being repacked.
    fn packs(&self, stored: &Stored) -> bool {
        match stored.key.contains('@') {
            true => self.repack.contains(file_of(&stored.key)),
            false => self.small(stored.size) && !self.before.contains(&*stored.key),
        }
    }

    /// Whether a file of `size` bytes is packed.
    fn small(&self, size: u64) -> bool {
        self.pack_limit.is_some_and(|limit| size <= limit)
    }
}

/// The saved changes of one workspace, in its state directory.
pub struct Saved {
    dir: PathBuf,
    /// The objects, as the source saved files are read from.
    store: Arc<Objects>,
    /// For each saved file: the version it had when saved, and its object. An object's
    /// name is shared with every file reading from it, so its count says whether any
    /// workspace still does.
    objects: HashMap<String, (u64, SourceKey)>,
    /// Objects the previous saves replaced. Each is deleted one save later, so a reader
    /// that loaded the index just before that save still finds it, and only once no
    /// file here reads from it, such as a removed file a program still has open.
    doomed: Vec<SourceKey>,
    /// Objects not packed that the saved state this one took over from handed out
    /// (see [`Saved::take_over`]). Files left open from before may still read them, so
    /// none goes while one does; one that nothing here names goes once none does.
    earlier: Vec<SourceKey>,
    /// Objects written since the last durable save that names them, not yet flushed to
    /// disk, by file: a pack is flushed once for all the objects in it.
    unsynced: HashSet<String>,
    /// The packs, by file, with the objects handed out from each.
    packs: HashMap<String, Pack>,
    /// Files of at most this size are packed: [`PACK_LIMIT`], or none in tests of
    /// files saved one per object.
    pub(crate) pack_limit: Option<u64>,
    /// See [`Saved::take_written`].
    written: Written,
    /// The objects being deleted in the background, if any; see [`Saved::delete`].
    deleting: Option<std::thread::JoinHandle<()>>,
    /// What the last save saw, which the next one starts from.
    last: Option<Last>,
    /// Leave the files macOS makes for its own bookkeeping ([`is_bookkeeping`]) out.
    pub skip_bookkeeping: bool,
}

/// What a save saw: the snapshot it saved, each change with its index key, and the
/// entries it wrote. A later save takes the entries of files unchanged since as they are.
struct Last {
    ws: Workspace,
    root: VPath,
    changes: BTreeMap<VPath, (ChangeKind, String)>,
    entries: BTreeMap<String, Entry>,
    times: BTreeMap<String, SystemTime>,
}

impl Drop for Saved {
    /// Finish deleting first, so no delete outlives the workspace: one recreated
    /// under the same name reuses the same object names.
    fn drop(&mut self) {
        if let Some(deleting) = self.deleting.take() {
            let _ = deleting.join();
        }
    }
}

impl Saved {
    /// The saved changes in the state directory `dir`.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        let store =
            Arc::new(Objects { dir: dir.join("objects"), next: AtomicU64::new(1), drafts: Mutex::new(Vec::new()) });
        Saved {
            dir,
            store,
            objects: HashMap::new(),
            doomed: Vec::new(),
            earlier: Vec::new(),
            unsynced: HashSet::new(),
            packs: HashMap::new(),
            pack_limit: Some(PACK_LIMIT),
            written: Written::new(),
            deleting: None,
            last: None,
            skip_bookkeeping: false,
        }
    }

    /// Take over from `old`, the saved state of this workspace before it was moved onto
    /// a newer source while served: files left open from before read `old`'s objects,
    /// whose names this one has its own counts for, so it keeps `old`'s as well.
    pub fn take_over(&mut self, mut old: Saved) {
        let mut seen = HashSet::new();
        let unpacked = old.objects.values().map(|(_, object)| object).chain(&old.doomed).chain(&old.earlier);
        for object in unpacked.filter(|object| !object.contains('@')) {
            if seen.insert(Arc::as_ptr(object).cast::<u8>()) {
                self.earlier.push(object.clone());
            }
        }
        for (file, pack) in std::mem::take(&mut old.packs) {
            let into = self.packs.entry(file).or_insert(Pack { size: pack.size, objects: Vec::new(), unnamed: false });
            into.objects.extend(pack.objects);
        }
    }

    /// Where the bytes of `object`, `size` long, are.
    fn stored(&self, object: SourceKey, size: u64) -> Stored {
        Stored { source: self.store.clone(), key: object, size }
    }

    /// Where the serving process writes files, as drafts of new objects.
    pub fn drafts(&self) -> Drafts {
        Drafts(self.store.clone(), self.pack_limit)
    }

    /// Whether `stored` is one of this workspace's objects.
    fn is_ours(&self, stored: &Stored) -> bool {
        std::ptr::addr_eq(Arc::as_ptr(&stored.source), Arc::as_ptr(&self.store))
    }

    fn index_path(&self) -> PathBuf {
        self.dir.join("changes.json")
    }

    fn objects_dir(&self) -> PathBuf {
        self.dir.join("objects")
    }

    /// The index's bytes. On Windows, opening it while a save renames the new index
    /// over it is refused with "access denied" for a moment; that is retried.
    fn read_index_bytes(&self) -> std::io::Result<Vec<u8>> {
        let mut tries = 0;
        loop {
            match std::fs::read(self.index_path()) {
                Err(err) if cfg!(windows) && err.kind() == std::io::ErrorKind::PermissionDenied && tries < 100 => {
                    tries += 1;
                    std::thread::sleep(Duration::from_millis(1));
                }
                read => return read,
            }
        }
    }

    fn read_index(&self) -> Result<Changes> {
        match self.read_index_bytes() {
            Ok(data) => serde_json::from_slice(&data).context("reading changes.json"),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Changes::default()),
            Err(err) => Err(err).context("reading changes.json"),
        }
    }

    /// Every changed path in the last save, relative to the mount root, without
    /// loading the workspace.
    pub fn paths(&self) -> Result<Vec<(String, PathState)>> {
        let changes = self.read_index()?;
        let state = |entry: &Entry| match entry {
            Entry::File { .. } | Entry::Symlink { .. } => PathState::File,
            Entry::Dir { .. } => PathState::Dir,
            Entry::Absent => PathState::Absent,
        };
        Ok(changes.entries.iter().map(|(rel, entry)| (rel.clone(), state(entry))).collect())
    }

    /// The saved paths that are other names of another path (hard links), with that
    /// path: an earlier saved one, or one unchanged in the source.
    pub fn links(&self) -> Result<Vec<(String, String)>> {
        let changes = self.read_index()?;
        let link = |entry: &Entry| match entry {
            Entry::File { link, .. } | Entry::Symlink { link, .. } => link.clone(),
            _ => None,
        };
        Ok(changes.entries.iter().filter_map(|(rel, entry)| Some((rel.clone(), link(entry)?))).collect())
    }

    /// Forget the saved changes of the paths `drop` names, so they read as the source
    /// does again. Their objects go once nothing uses them, as replaced ones do.
    ///
    /// Only the holder of the workspace's writer lock may do this, with nothing serving it.
    pub fn forget(&mut self, drop: &BTreeSet<String>) -> Result<()> {
        let mut changes = self.read_index()?;
        changes.entries.retain(|rel, _| !drop.contains(rel));
        let tmp = self.dir.join("changes.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(&changes)?).context("writing changes.json")?;
        sync_path(&tmp)?;
        std::fs::rename(&tmp, self.index_path()).context("writing changes.json")?;
        self.last = None;
        sync_path(&self.dir).map_err(|err| err.context(NotDurable))
    }

    /// Replay the saved changes onto `ws`, whose source is mounted at `root`.
    ///
    /// Loading only reads, so any process may load a workspace, even while another
    /// one serves it.
    pub fn load(&mut self, ws: &mut Workspace, root: &VPath) -> Result<Restored> {
        let changes = self.read_index()?;
        self.last = None;
        let mut restored = Restored::default();
        // Files saved as one object share its name.
        let mut names: HashMap<&str, SourceKey> = HashMap::new();
        // The size of each file in `objects/` looked at so far: one look per pack.
        let mut files: HashMap<String, u64> = HashMap::new();
        // Directories this load made where nothing was. Entries are sorted, so one comes
        // before everything below it, and nothing is there yet to look at.
        let mut made: HashSet<VPath> = HashSet::new();
        for (rel, entry) in &changes.entries {
            let path = under(root, rel)?;
            let object = match entry {
                Entry::File { object, .. } => {
                    let size = self.object_size(object, &mut files).with_context(|| format!("restoring {rel}"))?;
                    let key = names.entry(object).or_insert_with(|| {
                        let key: SourceKey = object.as_str().into();
                        if object.contains('@') {
                            self.packs.entry(file_of(object).to_owned()).or_default().objects.push(key.clone());
                        }
                        key
                    });
                    Some((key.clone(), size))
                }
                _ => None,
            };
            let current = match path.parent() {
                Some(parent) if made.contains(&parent) => None,
                _ => current_kind(ws, &path, entry).with_context(|| format!("restoring {rel}"))?,
            };
            let version = self
                .apply(ws, root, &path, entry, current, object.clone())
                .with_context(|| format!("restoring {rel}"))?;
            if current.is_none() && matches!(entry, Entry::Dir { .. }) {
                made.insert(path.clone());
            }
            let object = object.map(|(object, _)| object);
            let mtime = match entry {
                Entry::File { mtime, .. } | Entry::Dir { mtime } | Entry::Symlink { mtime, .. } => *mtime,
                Entry::Absent => None,
            };
            if let Some(mtime) = mtime {
                restored.times.insert(path.to_string(), from_time(mtime));
            }
            match (entry, object) {
                (Entry::File { .. }, Some(object)) => {
                    let version = match version {
                        Some(version) => version,
                        None => ws.stat(path.as_str())?.version,
                    };
                    // A save that wasn't durable may have left it unflushed before the
                    // process that wrote it went away, so the next durable save flushes it.
                    self.unsynced.insert(file_of(&object).to_owned());
                    self.objects.insert(rel.clone(), (version, object));
                }
                (Entry::Symlink { .. }, _) => {}
                _ => {
                    restored.dirs.insert(path.to_string());
                }
            }
        }
        // New objects must never reuse the name of one the index still names, nor of any
        // object an earlier save used.
        let last = self.objects.values().filter_map(|(_, object)| file_of(object).parse::<u64>().ok()).max();
        self.store.next.fetch_max((last.unwrap_or(0) + 1).max(changes.next), Ordering::SeqCst);
        for (file, size) in files {
            if let Some(pack) = self.packs.get_mut(&file) {
                pack.size = size;
            }
        }
        Ok(restored)
    }

    /// The size of the saved object `object`. Only its file's size is read, once per
    /// file in `files`; a missing file means a newer save replaced the object while
    /// this index was being loaded.
    fn object_size(&self, object: &str, files: &mut HashMap<String, u64>) -> Result<u64> {
        let place = place(object)?;
        let size = match files.get(place.file) {
            Some(size) => *size,
            None => {
                let size = match std::fs::metadata(self.objects_dir().join(place.file)) {
                    Ok(meta) => meta.len(),
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                        return Err(ObjectGone(object.to_owned()).into());
                    }
                    Err(err) => return Err(err).with_context(|| format!("reading saved object {object}")),
                };
                files.insert(place.file.to_owned(), size);
                size
            }
        };
        match place.range {
            None => Ok(size),
            Some((offset, len)) if offset.checked_add(len).is_some_and(|end| end <= size) => Ok(len),
            Some(_) => bail!("saved object {object} reaches past the end of its pack"),
        }
    }

    /// Delete objects the loaded index does not use, left by a crash or by the last
    /// session's delayed deletes. Only the process that serves the workspace, holding
    /// its writer lock, may do this: it alone writes objects.
    pub fn collect_garbage(&mut self) {
        let live: HashSet<&str> = self.objects.values().map(|(_, object)| file_of(object)).collect();
        let mut last = 0;
        if let Ok(files) = std::fs::read_dir(self.objects_dir()) {
            for file in files.flatten() {
                let name = file.file_name();
                let name = name.to_str().unwrap_or_default();
                if live.contains(name) {
                    last = last.max(name.parse().unwrap_or(0));
                } else {
                    let _ = std::fs::remove_file(file.path());
                }
            }
        }
        self.store.next.fetch_max(last + 1, Ordering::SeqCst);
    }

    /// Replay one entry over `current`, what is at its path now; a file reads from
    /// `object`, its object's shared name, and size, unless it is another name of a file
    /// already restored. Returns the version of a file it wrote.
    fn apply(
        &self,
        ws: &mut Workspace,
        root: &VPath,
        path: &VPath,
        entry: &Entry,
        current: Option<Kind>,
        object: Option<(SourceKey, u64)>,
    ) -> Result<Option<u64>> {
        let p = path.as_str();
        match entry {
            Entry::Absent => {
                if current.is_some() {
                    ws.remove(p, true)?;
                }
            }
            Entry::Dir { .. } => match current {
                Some(Kind::Dir) => {}
                Some(_) => {
                    ws.remove(p, false)?;
                    ws.mkdir(p, true)?;
                }
                None => ws.mkdir(p, true)?,
            },
            Entry::File { link: Some(other), .. } | Entry::Symlink { link: Some(other), .. }
                if self.relink(ws, root, path, other, current)? => {}
            Entry::Symlink { target, .. } => {
                if current.is_some() {
                    ws.remove(p, true)?;
                }
                ws.symlink(p, target)?;
            }
            Entry::File { executable, .. } => {
                // The bytes stay on disk; only the size was read.
                let (object, size) = object.context("a saved file without its object")?;
                // A directory or a link in the way goes first: a write never follows a link.
                if matches!(current, Some(Kind::Dir | Kind::Symlink)) {
                    ws.remove(p, true)?;
                }
                let version = ws.write_stored(p, self.stored(object, size))?;
                // A write keeps the mode of the file it replaces, and a new file starts
                // without the executable bit, so the bit is set only where it differs.
                let was_executable = current == Some(Kind::File { executable: true });
                if *executable != was_executable {
                    return Ok(Some(ws.set_executable(p, *executable)?));
                }
                return Ok(Some(version));
            }
        }
        Ok(None)
    }

    /// Make `path` another name of the file at `other` (relative to `root`), if that
    /// is still a file. False when it isn't, so the file is read from its object.
    fn relink(
        &self,
        ws: &mut Workspace,
        root: &VPath,
        path: &VPath,
        other: &str,
        current: Option<Kind>,
    ) -> Result<bool> {
        let other = under(root, other)?;
        match ws.stat(other.as_str()) {
            Ok(meta) if !meta.kind.is_dir() => {}
            _ => return Ok(false),
        }
        if current.is_some() {
            ws.remove(path.as_str(), true)?;
        }
        ws.hard_link(other.as_str(), path.as_str())?;
        Ok(true)
    }

    /// Save `snap`, whose source is mounted at `root`. A `durable` save, which answers
    /// `fsync`, also flushes the index and every object it names to disk before returning.
    /// [`Saved::take_written`] then says which files it wrote objects for.
    pub fn save(&mut self, snap: &Snapshot, root: &VPath, durable: bool) -> Result<()> {
        // Drafts made since the last save: each is either named by this save or, once
        // nothing writes or reads it, deleted like a replaced object.
        let drafts = std::mem::take(&mut *self.store.drafts.lock().unwrap_or_else(PoisonError::into_inner));
        self.unsynced.extend(drafts.iter().map(|draft| draft.to_string()));
        self.doomed.extend(drafts);
        let mut entries = BTreeMap::new();
        let mut saving = Saving {
            kept: HashMap::new(),
            written: Written::new(),
            pack: None,
            before: self.objects.values().map(|(_, object)| object.to_string()).collect(),
            repack: self.packs_to_repack(),
            packed: HashMap::new(),
            pack_limit: self.pack_limit,
        };
        // Taken now: if this save fails, the next one goes over everything again.
        let last = self.last.take().filter(|last| last.root == *root);
        let (changes, touched) = changes(snap, root, last.as_ref())?;
        // A hard link ties a file's entry to its other names', so then every entry is made again.
        let reuse = last.as_ref().zip(touched.as_ref()).filter(|_| !snap.ws.has_hard_links());
        let skip_bookkeeping = self.skip_bookkeeping;
        let skip = |path: &VPath, kind: ChangeKind| {
            skip_bookkeeping && kind == ChangeKind::Added && path.name().is_some_and(is_bookkeeping)
        };
        let changed: HashSet<&VPath> =
            changes.iter().filter(|(path, (kind, _))| !skip(path, *kind)).map(|(path, _)| path).collect();
        for (path, (kind, rel)) in &changes {
            if skip(path, *kind) {
                continue;
            }
            let unchanged = reuse.and_then(|since| self.unchanged_entry(snap, since, path, *kind, rel, &mut saving));
            if let Some(entry) = unchanged {
                entries.insert(rel.clone(), entry);
                continue;
            }
            let entry = match kind {
                ChangeKind::Deleted => Entry::Absent,
                ChangeKind::Added | ChangeKind::Modified => {
                    let link = linked_name(snap, root, &changed, path)?;
                    self.entry(snap, path, rel, link, &mut saving)?
                }
            };
            entries.insert(rel.clone(), entry);
        }
        for dir in &snap.dirs {
            let path = VPath::parse(dir)?;
            let rel = relative(root, &path)?;
            let entry = match snap.ws.stat(dir) {
                // A directory that replaced a deleted base file overrides its deletion.
                Ok(meta) if meta.kind.is_dir() => Entry::Dir { mtime: snap.times.get(dir).map(|t| to_time(*t)) },
                _ if entries.contains_key(&rel) => continue,
                Ok(_) => {
                    let link = linked_name(snap, root, &changed, &path)?;
                    self.entry(snap, &path, &rel, link, &mut saving)?
                }
                // Gone, or below a file that replaced one of its parents.
                Err(Error::NotFound(_) | Error::NotADirectory(_)) => Entry::Absent,
                Err(err) => return Err(err.into()),
            };
            entries.insert(rel, entry);
        }

        // The snapshot before this one reads from objects that may be deleted below.
        drop(last);
        let Saving { kept, written, pack, .. } = saving;
        // The pack stays the save's to delete until the index naming it is in place.
        let pack = match pack {
            Some(PackWriter { file, name, size, objects, unfinished }) => {
                file.into_inner().map_err(|err| err.into_error()).context("writing a saved file")?;
                self.unsynced.insert(name.to_string());
                Some((name, Pack { size, objects, unnamed: false }, unfinished))
            }
            None => None,
        };
        let index = Changes { entries, next: self.store.next.load(Ordering::SeqCst) };
        let data = serde_json::to_vec(&index)?;
        let tmp = self.dir.join("changes.json.tmp");
        if durable {
            // Objects reach the disk before the index that names them.
            self.sync_objects(&kept)?;
        }
        std::fs::write(&tmp, data).context("writing changes.json")?;
        if durable {
            sync_path(&tmp)?;
        }
        std::fs::rename(&tmp, self.index_path()).context("writing changes.json")?;
        self.last = Some(Last {
            ws: snap.ws.fork(),
            root: root.clone(),
            changes,
            entries: index.entries,
            times: snap.times.clone(),
        });
        if let Some((name, pack, unfinished)) = pack {
            unfinished.finished();
            self.packs.insert(name.to_string(), pack);
        }

        // The new index is in place: objects replaced by the save before this one are
        // now two indexes old. One that a file here still reads from waits until none does.
        let live: HashSet<&str> = kept.values().map(|(_, object)| &**object).collect();
        let mut waiting = Vec::new();
        let mut deleted = Vec::new();
        // What files left open from before a take-over still read, and what nothing
        // reads any more and nothing names either: that goes.
        let read_earlier: HashSet<String> = self
            .earlier
            .iter()
            .filter(|object| Arc::strong_count(object) > 1)
            .map(|object| object.to_string())
            .collect();
        let named: HashSet<&str> =
            self.objects.values().map(|(_, object)| &**object).chain(self.doomed.iter().map(|o| &**o)).collect();
        for object in std::mem::take(&mut self.earlier) {
            if Arc::strong_count(&object) > 1 {
                self.earlier.push(object);
            } else if !live.contains(&*object) && !named.contains(&*object) && !read_earlier.contains(&*object) {
                self.unsynced.remove(&*object);
                deleted.push(object.to_string());
            }
        }
        for object in std::mem::take(&mut self.doomed) {
            if live.contains(&*object) {
                continue;
            }
            if Arc::strong_count(&object) == 1 && !read_earlier.contains(&*object) {
                self.unsynced.remove(&*object);
                deleted.push(object.to_string());
            } else {
                waiting.push(object);
            }
        }
        // Files saved as one object (copies) name it once each, but it waits once: a
        // second entry here would count as a file still reading from it. A packed
        // object goes with its pack, below.
        let mut replaced = HashSet::new();
        for object in self.objects.values().map(|(_, object)| object) {
            if !live.contains(&**object) && !object.contains('@') && replaced.insert(object.clone()) {
                waiting.push(object.clone());
            }
        }
        self.doomed = waiting;
        self.objects = kept;
        // The index names these files' objects now, even if flushing it fails below.
        self.written = written;
        deleted.extend(self.collect_packs());
        self.delete(deleted);
        if durable {
            sync_path(&self.dir).map_err(|err| err.context(NotDurable))?;
        }
        Ok(())
    }

    /// Whether a replaced object or pack, or a draft no save has named yet (such as
    /// one written to a removed file), is waiting for a later save to delete it.
    pub fn deleting_later(&self) -> bool {
        let drafts = !self.store.drafts.lock().unwrap_or_else(PoisonError::into_inner).is_empty();
        drafts || !self.doomed.is_empty() || self.packs.values().any(|pack| pack.unnamed)
    }

    /// Flush to disk what the last save left unflushed: the objects its index names, and
    /// the index. This is all an `fsync` has to do when nothing changed since that save
    /// and nothing waits for it to be deleted ([`Saved::deleting_later`]).
    pub fn flush(&mut self) -> Result<()> {
        let objects = std::mem::take(&mut self.objects);
        let synced = self.sync_objects(&objects);
        self.objects = objects;
        synced?;
        let index = self.index_path();
        // Nothing saved yet, so nothing to flush.
        if !index.exists() {
            return Ok(());
        }
        sync_path(&index)?;
        sync_path(&self.dir).map_err(|err| err.context(NotDurable))
    }

    /// Flush the files holding `objects` that were written since a durable save last
    /// named them.
    fn sync_objects(&mut self, objects: &HashMap<String, (u64, SourceKey)>) -> Result<()> {
        let live: HashSet<&str> = objects.values().map(|(_, object)| file_of(object)).collect();
        let named: Vec<String> = self.unsynced.iter().filter(|o| live.contains(o.as_str())).cloned().collect();
        if !sync_disk(&self.objects_dir(), named.len())? {
            for object in &named {
                sync_path(&self.objects_dir().join(object))?;
            }
        }
        if !named.is_empty() {
            sync_path(&self.objects_dir())?;
        }
        // A draft still being written is flushed by the durable save that names it.
        for object in named {
            self.unsynced.remove(&object);
        }
        Ok(())
    }

    /// The files the last save to put its index in place wrote objects for, by workspace
    /// path, with the version saved and where its bytes are now: the serving process
    /// reads them from there from now on (see [`Workspace::store`]). They are kept until
    /// taken, even when that save then failed to flush the index to disk.
    pub fn take_written(&mut self) -> Written {
        std::mem::take(&mut self.written)
    }

    /// The entry the last save wrote for `path`, if it still holds: the file is the
    /// same as then (not `touched` since `last`), and so are its time and object.
    fn unchanged_entry(
        &self,
        snap: &Snapshot,
        (last, touched): (&Last, &HashSet<VPath>),
        path: &VPath,
        kind: ChangeKind,
        rel: &str,
        saving: &mut Saving,
    ) -> Option<Entry> {
        if touched.contains(path) || snap.times.get(path.as_str()) != last.times.get(path.as_str()) {
            return None;
        }
        let entry = last.entries.get(rel)?;
        match (kind, entry) {
            (ChangeKind::Added | ChangeKind::Modified, Entry::File { object, link: None, .. }) => {
                let (version, saved) = self.objects.get(rel)?;
                if **saved != *object || saving.repack.contains(file_of(saved)) {
                    return None;
                }
                saving.kept.insert(rel.to_owned(), (*version, saved.clone()));
            }
            (ChangeKind::Added | ChangeKind::Modified, Entry::Symlink { link: None, .. })
            | (ChangeKind::Deleted, Entry::Absent) => {}
            // A directory there now, or another name of the same file.
            _ => return None,
        }
        Some(entry.clone())
    }

    /// The entry for a file or directory, writing its bytes to a new object unless the
    /// last save already holds this version. A small file goes into the save's pack.
    fn entry(
        &mut self,
        snap: &Snapshot,
        path: &VPath,
        rel: &str,
        link: Option<String>,
        saving: &mut Saving,
    ) -> Result<Entry> {
        let p = path.as_str();
        let meta = snap.ws.stat(p)?;
        let mtime = snap.times.get(p).map(|t| to_time(*t));
        let executable = match meta.kind {
            Kind::Dir => return Ok(Entry::Dir { mtime }),
            Kind::Symlink => return Ok(Entry::Symlink { target: snap.ws.read_link(p)?, mtime, link }),
            Kind::File { executable } => executable,
        };
        // A hard-linked file shares the object its first name got in this save, even a
        // new range when that name's pack is written again.
        let shared = link.as_ref().and_then(|first| saving.kept.get(first)).filter(|(v, _)| *v == meta.version);
        let shared = shared.map(|(_, object)| object.clone());
        let saved = match self.objects.get(rel) {
            Some((version, object)) if *version == meta.version && !saving.repack.contains(file_of(object)) => {
                Some(object.clone())
            }
            _ => None,
        };
        let object = match (shared, saved) {
            (Some(object), _) => {
                // If this save wrote it, every name may move it out of memory, so
                // removing the first name meanwhile doesn't keep it there.
                let written = saving.written.iter().rev().find(|(.., stored)| stored.key == object);
                if let Some((_, _, stored)) = written {
                    let stored = stored.clone();
                    saving.written.push((path.clone(), meta.version, stored));
                }
                object
            }
            (None, Some(object)) => object,
            (None, None) => {
                let stored = snap.ws.stored(p)?.filter(|stored| self.is_ours(stored));
                match stored {
                    // A saved file moved, copied or made executable keeps its object.
                    Some(stored) if !saving.packs(&stored) => stored.key,
                    _ => {
                        let earlier = stored.as_ref().and_then(|stored| saving.packed.get(&*stored.key)).cloned();
                        let object = match earlier {
                            // A copy of a file this save already packed.
                            Some(object) => object,
                            None => {
                                // A ranged read, which keeps no copy of a base file in memory.
                                let data = snap.ws.read_range(p, 0, meta.size)?;
                                let object = match saving.small(data.len() as u64) {
                                    true => self.pack_object(&data, &mut saving.pack)?,
                                    false => self.write_object(&data)?,
                                };
                                if let Some(stored) = &stored {
                                    saving.packed.insert(stored.key.to_string(), object.clone());
                                }
                                object
                            }
                        };
                        let size = place(&object).ok().and_then(|place| place.range).map_or(meta.size, |(_, len)| len);
                        saving.written.push((path.clone(), meta.version, self.stored(object.clone(), size)));
                        object
                    }
                }
            }
        };
        saving.kept.insert(rel.to_owned(), (meta.version, object.clone()));
        Ok(Entry::File { executable, object: object.to_string(), mtime, link })
    }

    /// Append `data` to the pack this save writes, opening it first if need be, and
    /// return the object name of its range.
    fn pack_object(&mut self, data: &[u8], pack: &mut Option<PackWriter>) -> Result<SourceKey> {
        let pack = match pack {
            Some(pack) => pack,
            None => {
                let dir = self.objects_dir();
                std::fs::create_dir_all(&dir).context("creating the objects directory")?;
                let name = self.store.new_name();
                let path = dir.join(&*name);
                let file = std::fs::File::options()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                    .context("writing a saved file")?;
                let file = std::io::BufWriter::with_capacity(1 << 20, file);
                let unfinished = Unfinished(Some(path));
                pack.insert(PackWriter { file, name, size: 0, objects: Vec::new(), unfinished })
            }
        };
        pack.file.write_all(data).context("writing a saved file")?;
        let object: SourceKey = format!("{}@{}+{}", pack.name, pack.size, data.len()).into();
        pack.size += data.len() as u64;
        pack.objects.push(object.clone());
        Ok(object)
    }

    /// The packs whose files the next save writes again into its own pack: those mostly
    /// holding files the last index no longer names, and every small one once there
    /// are many, so that neither wasted space nor the number of packs keeps growing.
    fn packs_to_repack(&self) -> HashSet<String> {
        // Copies of a file share its range, which holds its bytes once.
        let objects: HashSet<&str> = self.objects.values().map(|(_, object)| &**object).collect();
        let mut named: HashMap<&str, u64> = HashMap::new();
        for object in objects {
            if let Ok(Place { file, range: Some((_, len)) }) = place(object) {
                *named.entry(file).or_default() += len;
            }
        }
        let mut repack = HashSet::new();
        let mut small = Vec::new();
        for (file, bytes) in named {
            let Some(pack) = self.packs.get(file) else { continue };
            if bytes * 2 < pack.size {
                repack.insert(file.to_owned());
            } else if pack.size <= SMALL_PACK {
                small.push(file.to_owned());
            }
        }
        if small.len() > SMALL_PACKS {
            repack.extend(small);
        }
        repack
    }

    /// Forget the objects of each pack that nothing reads from any more, and return the
    /// packs to delete: those the last two indexes name none of the objects of, and
    /// that nothing reads from.
    fn collect_packs(&mut self) -> Vec<String> {
        let named: HashSet<&str> = self.objects.values().map(|(_, object)| file_of(object)).collect();
        let mut deleted = Vec::new();
        let unsynced = &mut self.unsynced;
        self.packs.retain(|file, pack| {
            pack.objects.retain(|object| Arc::strong_count(object) > 1);
            if named.contains(file.as_str()) {
                pack.unnamed = false;
                return true;
            }
            if pack.unnamed && pack.objects.is_empty() {
                unsynced.remove(file);
                deleted.push(file.clone());
                return false;
            }
            pack.unnamed = true;
            true
        });
        deleted
    }

    fn write_object(&mut self, data: &Bytes) -> Result<SourceKey> {
        let dir = self.objects_dir();
        std::fs::create_dir_all(&dir).context("creating the objects directory")?;
        let object = self.store.new_name();
        std::fs::write(dir.join(&*object), data).context("writing a saved file")?;
        self.unsynced.insert(object.to_string());
        Ok(object)
    }

    /// Copy the saved changes into `to`, the state directory of a fork. Objects are
    /// never modified once written, so they are hard-linked where the disk allows.
    ///
    /// The workspace may be mounted and saving meanwhile; if an object the index
    /// names has already been deleted, the copy starts over from the newer index.
    pub fn copy_to(&self, to: &Path) -> Result<()> {
        let objects = to.join("objects");
        for _ in 0..10 {
            let index = match self.read_index_bytes() {
                Ok(index) => index,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(err) => return Err(err).context("reading changes.json"),
            };
            let changes: Changes = serde_json::from_slice(&index).context("reading changes.json")?;
            std::fs::create_dir_all(&objects)?;
            let mut complete = true;
            let mut files = HashSet::new();
            for entry in changes.entries.values() {
                let Entry::File { object, .. } = entry else { continue };
                // A pack is linked once for all the files in it.
                let file = place(object)?.file;
                if !files.insert(file) {
                    continue;
                }
                let (from, target) = (self.objects_dir().join(file), objects.join(file));
                // Already linked by an earlier attempt, or for another file saved as it.
                let linked = match std::fs::hard_link(&from, &target) {
                    Ok(()) => true,
                    Err(err) => err.kind() == std::io::ErrorKind::AlreadyExists,
                };
                if !linked && std::fs::copy(&from, &target).is_err() {
                    // A failed copy may leave part of the file, which a retry must not reuse.
                    let _ = std::fs::remove_file(&target);
                    complete = false;
                    break;
                }
            }
            if complete {
                std::fs::write(to.join("changes.json"), index)?;
                return Ok(());
            }
        }
        bail!("the workspace kept changing while it was being copied; try again")
    }
}

/// Objects a save deletes itself; more are deleted in the background, so that a save
/// after many files were packed doesn't wait for one delete each.
const DELETE_IN_PLACE: usize = 64;

impl Saved {
    /// Delete the objects `files`. This workspace never uses their names again, so a
    /// delete finishing late changes nothing, and one the process ending cuts short is
    /// finished by the next mount's [`Saved::collect_garbage`].
    fn delete(&mut self, files: Vec<String>) {
        let many = files.len() > DELETE_IN_PLACE;
        let dir = self.objects_dir();
        let remove = move || {
            for file in files {
                let _ = std::fs::remove_file(dir.join(file));
            }
        };
        if !many {
            return remove();
        }
        // One batch at a time, so they don't pile up.
        if let Some(deleting) = self.deleting.take() {
            let _ = deleting.join();
        }
        self.deleting = Some(std::thread::spawn(remove));
    }
}

/// Where an object's bytes are: a whole file in `objects/`, named by a number, or a
/// range of a pack, a file holding many small objects, named `FILE@OFFSET+LEN`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Place<'a> {
    file: &'a str,
    range: Option<(u64, u64)>,
}

/// The place an object name points at. Names are numbers, or a pack range; refuse
/// anything else rather than follow it on disk.
fn place(object: &str) -> Result<Place<'_>> {
    let number = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    let (file, range) = match object.split_once('@') {
        None => (object, None),
        Some((file, range)) => {
            let parsed = range.split_once('+').and_then(|(offset, len)| {
                (number(offset) && number(len)).then(|| Some((offset.parse().ok()?, len.parse().ok()?)))?
            });
            (file, Some(parsed.with_context(|| format!("invalid object name {object:?} in changes.json"))?))
        }
    };
    if !number(file) {
        bail!("invalid object name {object:?} in changes.json");
    }
    Ok(Place { file, range })
}

/// The file in `objects/` holding `object`'s bytes. Only for names [`place`] accepted.
fn file_of(object: &str) -> &str {
    object.split_once('@').map_or(object, |(file, _)| file)
}

/// Every change in `snap`, with its index key, and the files changed since `last` if
/// they came from there. With no `last`, or a change since that doesn't follow from
/// it, they are worked out from the source, going over every change.
///
/// A file changed since `last` keeps what it was then: a file `last` added is still
/// added, one it found modified or deleted still differs from the source. A file
/// changed back to what the source has counts as modified, which saves its bytes again.
#[allow(clippy::type_complexity)]
fn changes(
    snap: &Snapshot,
    root: &VPath,
    last: Option<&Last>,
) -> Result<(BTreeMap<VPath, (ChangeKind, String)>, Option<HashSet<VPath>>)> {
    if let Some(last) = last {
        let since = snap.ws.changes_since(&last.ws)?;
        let mut changes = last.changes.clone();
        let mut follows = true;
        for change in &since {
            use ChangeKind::*;
            let before = changes.get(&change.path).map(|(kind, _)| *kind);
            let now = match (before, change.kind) {
                (None, kind) => Some(kind),
                (Some(Added), Modified) => Some(Added),
                (Some(Added), Deleted) => None,
                (Some(Modified), Modified | Deleted) => Some(change.kind),
                (Some(Deleted), Added) => Some(Modified),
                _ => {
                    follows = false;
                    break;
                }
            };
            match now {
                Some(kind) => {
                    let rel = relative(root, &change.path)?;
                    changes.insert(change.path.clone(), (kind, rel));
                }
                None => {
                    changes.remove(&change.path);
                }
            }
        }
        if follows {
            return Ok((changes, Some(since.into_iter().map(|change| change.path).collect())));
        }
    }
    let mut changes = BTreeMap::new();
    for change in snap.ws.changes()? {
        let rel = relative(root, &change.path)?;
        changes.insert(change.path, (change.kind, rel));
    }
    Ok((changes, None))
}

/// `path` relative to `root`, as stored in the index.
/// For a file with other names (hard links), the one loading restores first, relative
/// to `root`: a name saved before `path`, or one unchanged in the source.
fn linked_name(snap: &Snapshot, root: &VPath, changed: &HashSet<&VPath>, path: &VPath) -> Result<Option<String>> {
    if !snap.ws.has_hard_links() {
        return Ok(None);
    }
    let names = snap.ws.hard_links(path.as_str())?;
    // A name saved earlier comes first: an unchanged one may be gone from a host folder
    // by the next load, and then only the first name saved relinks to it.
    let saved = names.iter().find(|other| *other < path && changed.contains(other));
    let first = saved.or_else(|| names.iter().find(|other| *other != path && !changed.contains(other)));
    Ok(first.and_then(|first| relative(root, first).ok()))
}

fn relative(root: &VPath, path: &VPath) -> Result<String> {
    match path.strip_prefix(root) {
        Some(parts) if !parts.is_empty() => Ok(parts.join("/")),
        _ => bail!("{path} is not inside the mount"),
    }
}

/// The workspace path of an index key, refusing keys that climb out of the mount.
fn under(root: &VPath, rel: &str) -> Result<VPath> {
    let path = VPath::parse(&format!("{root}/{rel}"))?;
    if path == *root || !path.starts_with(root) {
        bail!("saved path {rel:?} is outside the mount");
    }
    Ok(path)
}

fn to_time(time: SystemTime) -> Time {
    let since = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    (since.as_secs(), since.subsec_nanos())
}

fn from_time((secs, nanos): Time) -> SystemTime {
    UNIX_EPOCH + Duration::new(secs, nanos)
}

/// Flush a file or directory to disk (`fsync`).
fn sync_path(path: &Path) -> Result<()> {
    // Windows flushes only files opened for writing, and has no directory flush: NTFS
    // journals the renames and new names a directory's flush would cover.
    if cfg!(windows) && path.is_dir() {
        return Ok(());
    }
    std::fs::File::options()
        .read(true)
        .write(cfg!(windows))
        .open(path)
        .and_then(|file| file.sync_all())
        .with_context(|| format!("syncing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use sandfs_core::MemorySource;

    use super::*;

    fn base() -> Workspace {
        let source = MemorySource::from_files([("a.txt", "a"), ("dir/b.txt", "b"), ("dir/sub/c.txt", "c")]).unwrap();
        let mut ws = Workspace::new();
        ws.mount("/mnt", Arc::new(source)).unwrap();
        ws
    }

    fn root() -> VPath {
        VPath::parse("/mnt").unwrap()
    }

    fn listing(ws: &Workspace, path: &str) -> Vec<String> {
        ws.list(path).unwrap().into_iter().map(|e| e.name).collect()
    }

    #[test]
    fn changes_survive_a_save_and_load() {
        let state = tempfile::tempdir().unwrap();
        let mut ws = base();
        ws.write("/mnt/a.txt", "changed").unwrap();
        ws.write("/mnt/new/run.sh", "#!/bin/sh").unwrap();
        ws.set_executable("/mnt/new/run.sh", true).unwrap();
        ws.symlink("/mnt/new/link", "run.sh").unwrap();
        ws.remove("/mnt/dir/b.txt", false).unwrap();
        ws.symlink("/mnt/dir/b.txt", "../a.txt").unwrap();
        ws.remove("/mnt/dir/sub", true).unwrap();
        ws.mkdir("/mnt/empty", false).unwrap();
        let dirs = ["/mnt/dir/sub", "/mnt/empty", "/mnt/new"].map(str::to_owned).into();
        let mtime = UNIX_EPOCH + Duration::new(1_700_000_000, 5);
        let times = [("/mnt/a.txt".to_owned(), mtime)].into();

        let mut saved = Saved::new(state.path());
        saved.save(&Snapshot { ws: ws.fork(), times, dirs }, &root(), true).unwrap();
        assert!(saved.unsynced.is_empty(), "a durable save flushes every object it wrote");

        let mut loaded = base();
        let mut reloaded = Saved::new(state.path());
        let restored = reloaded.load(&mut loaded, &root()).unwrap();
        assert_eq!(reloaded.unsynced.len(), 1, "a new process flushes what it loaded at its first durable save");
        assert_eq!(loaded.read_text("/mnt/a.txt").unwrap(), "changed");
        assert_eq!(loaded.stat("/mnt/new/run.sh").unwrap().kind, Kind::File { executable: true });
        assert_eq!(listing(&loaded, "/mnt/dir"), ["b.txt"], "the removed directory stays removed");
        assert_eq!(loaded.read_link("/mnt/new/link").unwrap(), "run.sh");
        assert_eq!(loaded.read_link("/mnt/dir/b.txt").unwrap(), "../a.txt", "a link replaces a base file");
        assert_eq!(saved.objects.len(), 2, "links need no saved object");
        assert!(loaded.list("/mnt/empty").unwrap().is_empty(), "empty directories are kept");
        assert_eq!(loaded.changes().unwrap(), ws.changes().unwrap());
        assert_eq!(restored.times["/mnt/a.txt"], mtime);
        assert!(restored.dirs.contains("/mnt/empty"));
    }

    #[test]
    fn hard_links_survive_a_save_and_load() {
        let state = tempfile::tempdir().unwrap();
        let mut ws = base();
        ws.write("/mnt/out/lib.rlib", "built").unwrap();
        ws.hard_link("/mnt/out/lib.rlib", "/mnt/cache/lib.rlib").unwrap();
        ws.hard_link("/mnt/out/lib.rlib", "/mnt/out/lib2.rlib").unwrap();
        ws.hard_link("/mnt/a.txt", "/mnt/a-link.txt").unwrap();
        ws.hard_link("/mnt/a.txt", "/mnt/z-link.txt").unwrap();
        ws.symlink("/mnt/sym", "a.txt").unwrap();
        ws.hard_link("/mnt/sym", "/mnt/sym2").unwrap();
        let dirs = ["/mnt/cache", "/mnt/out"].map(str::to_owned).into();

        let mut saved = Saved::new(state.path());
        saved.save(&Snapshot { ws: ws.fork(), times: BTreeMap::new(), dirs }, &root(), false).unwrap();
        assert_eq!(saved.objects.len(), 5);
        let objects: HashSet<&SourceKey> = saved.objects.values().map(|(_, object)| object).collect();
        // The second new name of a.txt links to the first, which a load can relink even
        // if a.txt is gone from the source by then.
        assert_eq!(objects.len(), 2, "the names of one file share an object");
        assert!(objects.iter().all(|object| object.contains('@')), "small files are packed: {objects:?}");
        let mut written: Vec<String> = saved.take_written().iter().map(|(path, ..)| path.to_string()).collect();
        written.sort();
        assert_eq!(
            written,
            ["/mnt/a-link.txt", "/mnt/cache/lib.rlib", "/mnt/out/lib.rlib", "/mnt/out/lib2.rlib", "/mnt/z-link.txt"],
            "any name can move the file out of memory"
        );

        let mut loaded = base();
        Saved::new(state.path()).load(&mut loaded, &root()).unwrap();
        let names = |path: &str| loaded.hard_links(path).unwrap().iter().map(ToString::to_string).collect::<Vec<_>>();
        assert_eq!(names("/mnt/out/lib.rlib"), ["/mnt/cache/lib.rlib", "/mnt/out/lib.rlib", "/mnt/out/lib2.rlib"]);
        assert_eq!(names("/mnt/a-link.txt"), ["/mnt/a-link.txt", "/mnt/a.txt", "/mnt/z-link.txt"]);
        assert_eq!(names("/mnt/sym2"), ["/mnt/sym", "/mnt/sym2"], "a link's names stay one link");
        assert_eq!(loaded.changes().unwrap(), ws.changes().unwrap());
        loaded.write("/mnt/cache/lib.rlib", "rebuilt").unwrap();
        assert_eq!(loaded.read_text("/mnt/out/lib2.rlib").unwrap(), "rebuilt");
    }

    #[test]
    fn hard_links_share_a_new_range_when_their_pack_is_written_again() {
        let state = tempfile::tempdir().unwrap();
        let mut ws = base();
        for i in 0..10 {
            ws.write(&format!("/mnt/{i}.txt"), "0123456789").unwrap();
        }
        ws.hard_link("/mnt/0.txt", "/mnt/link.txt").unwrap();
        let mut saved = Saved::new(state.path());
        save_and_store(&mut saved, &mut ws);
        // Most of the pack replaced: the next save writes 0.txt again, into its own pack.
        for i in 1..10 {
            ws.write(&format!("/mnt/{i}.txt"), "changed").unwrap();
        }
        save_and_store(&mut saved, &mut ws);
        save_and_store(&mut saved, &mut ws);
        let (first, link) = (&saved.objects["0.txt"].1, &saved.objects["link.txt"].1);
        assert_eq!(first, link, "both names name the new range");
        assert_eq!(file_of(first), "3");

        let mut loaded = base();
        Saved::new(state.path()).load(&mut loaded, &root()).unwrap();
        assert_eq!(loaded.hard_links("/mnt/link.txt").unwrap().len(), 2);
        assert_eq!(loaded.read_text("/mnt/link.txt").unwrap(), "0123456789");
        assert_eq!(loaded.changes().unwrap(), ws.changes().unwrap());
    }

    #[test]
    fn saves_write_only_files_that_changed_and_drop_stale_objects() {
        let state = tempfile::tempdir().unwrap();
        let mut ws = base();
        ws.write("/mnt/one.txt", "1").unwrap();
        ws.write("/mnt/two.txt", "2").unwrap();
        let mut saved = Saved::new(state.path());
        saved.pack_limit = None;
        let snap = |ws: &Workspace| Snapshot { ws: ws.fork(), times: BTreeMap::new(), dirs: BTreeSet::new() };
        saved.save(&snap(&ws), &root(), false).unwrap();
        let objects = |dir: &Path| {
            let mut names: Vec<String> = std::fs::read_dir(dir.join("objects"))
                .unwrap()
                .map(|f| f.unwrap().file_name().into_string().unwrap())
                .collect();
            names.sort();
            names
        };
        assert_eq!(objects(state.path()), ["1", "2"]);

        ws.write("/mnt/two.txt", "2 again").unwrap();
        saved.save(&snap(&ws), &root(), false).unwrap();
        assert_eq!(objects(state.path()), ["1", "2", "3"], "one.txt kept its object; two.txt's old one waits a save");

        ws.remove("/mnt/one.txt", false).unwrap();
        saved.save(&snap(&ws), &root(), false).unwrap();
        assert_eq!(objects(state.path()), ["1", "3"], "2 is gone now; 1 goes after the next save");

        let mut reopened = Saved::new(state.path());
        reopened.load(&mut base(), &root()).unwrap();
        reopened.collect_garbage();
        assert_eq!(objects(state.path()), ["3"], "the serving process clears what the index no longer uses");
        assert_eq!(reopened.store.next.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn object_names_are_never_reused() {
        let state = tempfile::tempdir().unwrap();
        let snap = |ws: &Workspace| Snapshot { ws: ws.fork(), times: BTreeMap::new(), dirs: BTreeSet::new() };
        let mut ws = base();
        ws.write("/mnt/one.txt", "1").unwrap();
        ws.write("/mnt/two.txt", "2").unwrap();
        let mut saved = Saved::new(state.path());
        saved.pack_limit = None;
        saved.save(&snap(&ws), &root(), false).unwrap();
        ws.remove("/mnt/two.txt", false).unwrap();
        saved.save(&snap(&ws), &root(), false).unwrap();

        // The next session deletes object 2, which a reader of the older index may
        // still be about to open, so it must never name a new object 2.
        let mut next = base();
        let mut saved = Saved::new(state.path());
        saved.pack_limit = None;
        saved.load(&mut next, &root()).unwrap();
        saved.collect_garbage();
        assert!(!state.path().join("objects/2").exists());
        next.write("/mnt/three.txt", "3").unwrap();
        saved.save(&snap(&next), &root(), false).unwrap();
        assert!(!state.path().join("objects/2").exists());
        assert_eq!(std::fs::read_to_string(state.path().join("objects/3")).unwrap(), "3");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn syncfs_is_trusted_from_linux_5_8() {
        assert!(kernel_at_least("5.8.0", (5, 8)));
        assert!(kernel_at_least("6.1.0-13-amd64\n", (5, 8)));
        assert!(kernel_at_least("5.15.0-1050-azure", (5, 8)));
        assert!(!kernel_at_least("5.4.0-150-generic", (5, 8)));
        assert!(!kernel_at_least("4.19.0", (5, 8)));
        assert!(!kernel_at_least("", (5, 8)));
    }

    #[test]
    fn a_durable_save_of_many_files_flushes_them_all() {
        let state = tempfile::tempdir().unwrap();
        let snap = |ws: &Workspace| Snapshot { ws: ws.fork(), times: BTreeMap::new(), dirs: BTreeSet::new() };
        let mut ws = base();
        // More than are flushed one by one on Linux.
        for i in 0..100 {
            ws.write(&format!("/mnt/many/{i}.txt"), i.to_string()).unwrap();
        }
        let mut saved = Saved::new(state.path());
        // One object per file, so there are many to flush.
        saved.pack_limit = None;
        saved.save(&snap(&ws), &root(), true).unwrap();
        assert!(saved.unsynced.is_empty());

        let mut reloaded = base();
        Saved::new(state.path()).load(&mut reloaded, &root()).unwrap();
        assert_eq!(reloaded.read_text("/mnt/many/99.txt").unwrap(), "99");
    }

    #[test]
    fn a_save_cut_short_leaves_the_last_one_intact() {
        let state = tempfile::tempdir().unwrap();
        let snap = |ws: &Workspace| Snapshot { ws: ws.fork(), times: BTreeMap::new(), dirs: BTreeSet::new() };
        let mut ws = base();
        ws.write("/mnt/a.txt", "saved").unwrap();
        Saved::new(state.path()).save(&snap(&ws), &root(), true).unwrap();

        // A process that died mid-save: objects written, the new index half written,
        // never renamed over the old one.
        std::fs::write(state.path().join("objects/2"), "unsaved").unwrap();
        std::fs::write(state.path().join("objects/3"), "unsaved").unwrap();
        std::fs::write(state.path().join("changes.json.tmp"), "{\"entries\": {\"a.t").unwrap();

        let mut restarted = base();
        let mut saved = Saved::new(state.path());
        saved.load(&mut restarted, &root()).unwrap();
        assert_eq!(restarted.read_text("/mnt/a.txt").unwrap(), "saved");
        saved.collect_garbage();
        assert!(!state.path().join("objects/2").exists() && !state.path().join("objects/3").exists());

        // Saving again works, and a save that fails returns its error and leaves the
        // previous save in place.
        restarted.write("/mnt/b.txt", "next").unwrap();
        saved.save(&snap(&restarted), &root(), true).unwrap();
        std::fs::create_dir(state.path().join("changes.json.tmp")).unwrap();
        restarted.write("/mnt/a.txt", "not saved").unwrap();
        assert!(saved.save(&snap(&restarted), &root(), true).is_err());
        std::fs::remove_dir(state.path().join("changes.json.tmp")).unwrap();

        let mut loaded = base();
        Saved::new(state.path()).load(&mut loaded, &root()).unwrap();
        assert_eq!(loaded.read_text("/mnt/a.txt").unwrap(), "saved");
        assert_eq!(loaded.read_text("/mnt/b.txt").unwrap(), "next");

        // The failed save is retried by the next one.
        saved.save(&snap(&restarted), &root(), true).unwrap();
        let mut loaded = base();
        Saved::new(state.path()).load(&mut loaded, &root()).unwrap();
        assert_eq!(loaded.read_text("/mnt/a.txt").unwrap(), "not saved");
    }

    #[test]
    fn a_file_that_replaced_a_directory_is_restored() {
        let state = tempfile::tempdir().unwrap();
        let mut ws = base();
        ws.remove("/mnt/dir", true).unwrap();
        ws.write("/mnt/dir", "now a file").unwrap();
        let snap = Snapshot { ws: ws.fork(), times: BTreeMap::new(), dirs: ["/mnt/dir".to_owned()].into() };
        Saved::new(state.path()).save(&snap, &root(), false).unwrap();

        let mut loaded = base();
        Saved::new(state.path()).load(&mut loaded, &root()).unwrap();
        assert_eq!(loaded.read_text("/mnt/dir").unwrap(), "now a file");
        assert_eq!(loaded.changes().unwrap(), ws.changes().unwrap());
    }

    #[test]
    fn replacements_between_files_and_directories_are_saved() {
        let state = tempfile::tempdir().unwrap();
        let mut ws = base();
        // dir/sub is tracked as removed, then dir itself becomes a file.
        ws.remove("/mnt/dir", true).unwrap();
        ws.write("/mnt/dir", "a file").unwrap();
        // a.txt becomes an empty directory.
        ws.remove("/mnt/a.txt", false).unwrap();
        ws.mkdir("/mnt/a.txt", false).unwrap();
        let dirs = ["/mnt/dir", "/mnt/dir/sub", "/mnt/a.txt"].map(str::to_owned).into();
        Saved::new(state.path())
            .save(&Snapshot { ws: ws.fork(), times: BTreeMap::new(), dirs }, &root(), false)
            .unwrap();

        let mut loaded = base();
        Saved::new(state.path()).load(&mut loaded, &root()).unwrap();
        assert_eq!(loaded.read_text("/mnt/dir").unwrap(), "a file");
        assert!(loaded.stat("/mnt/a.txt").unwrap().kind.is_dir(), "the directory that replaced a file is kept");
    }

    fn objects(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir.join("objects"))
            .map(|files| files.map(|f| f.unwrap().file_name().into_string().unwrap()).collect())
            .unwrap_or_default();
        names.sort();
        names
    }

    fn snap(ws: &Workspace) -> Snapshot {
        Snapshot { ws: ws.fork(), times: BTreeMap::new(), dirs: BTreeSet::new() }
    }

    /// Save `ws` and read its saved files from disk from now on, as a mount does.
    fn save_and_store(saved: &mut Saved, ws: &mut Workspace) {
        saved.save(&snap(ws), &root(), false).unwrap();
        for (path, version, stored) in saved.take_written() {
            assert!(ws.store(path.as_str(), version, stored).unwrap());
        }
    }

    /// Every path below `path` with what it holds, for comparing two workspaces.
    fn tree(ws: &Workspace, path: &str, out: &mut Vec<(String, String)>) {
        for entry in ws.list(path).unwrap() {
            let child = format!("{path}/{}", entry.name);
            let held = match entry.kind {
                Kind::Dir => "dir".to_owned(),
                Kind::Symlink => format!("-> {}", ws.read_link(&child).unwrap()),
                Kind::File { executable } => {
                    let data = ws.read(&child).unwrap();
                    format!("{executable} {} {:?}", data.len(), &data[..data.len().min(8)])
                }
            };
            out.push((child.clone(), held));
            if entry.kind.is_dir() {
                tree(ws, &child, out);
            }
        }
    }

    #[test]
    fn saves_that_start_from_the_last_one_load_back_what_was_saved() {
        let state = tempfile::tempdir().unwrap();
        let mut ws = base();
        let mut saved = Saved::new(state.path());
        let mut times: BTreeMap<String, SystemTime> = BTreeMap::new();
        let mut dirs: BTreeSet<String> = BTreeSet::new();
        let paths = ["a.txt", "dir/b.txt", "dir/sub/c.txt", "n0", "dir/n1", "dir/sub/n2", "new/n3", "new/deep/n4"];
        let mut seed: u64 = 7;
        let mut next = |n: usize| {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) as usize % n
        };
        for step in 0..400 {
            let path = format!("/mnt/{}", paths[next(paths.len())]);
            let other = format!("/mnt/{}", paths[next(paths.len())]);
            let parent = VPath::parse(&path).unwrap().parent().unwrap().to_string();
            // As a mount does, every directory made is kept track of.
            if parent.matches('/').count() > 1 {
                dirs.insert(parent.clone());
                dirs.insert(VPath::parse(&parent).unwrap().parent().unwrap().to_string());
                dirs.remove("/mnt");
            }
            // Each step may fail (a missing file, a file where a folder should be): what
            // the workspace then holds is what has to be saved.
            let _ = match next(10) {
                0 | 1 => ws.mkdir(&parent, true).and_then(|_| ws.write(&path, format!("{step}")).map(|_| ())),
                // Back to what the source has.
                2 => ws
                    .write(
                        &path,
                        path.trim_start_matches("/mnt/dir/sub/")
                            .trim_start_matches("/mnt/dir/")
                            .trim_start_matches("/mnt/")
                            .trim_end_matches(".txt")
                            .to_owned(),
                    )
                    .map(|_| ()),
                3 => ws.remove(&path, true),
                4 => ws.mkdir(&parent, true).and_then(|_| ws.rename(&other, &path)),
                5 => ws.set_executable(&path, next(2) == 0).map(|_| ()),
                6 => ws.mkdir(&parent, true).and_then(|_| ws.symlink(&path, "a.txt").map(|_| ())),
                7 if next(2) == 0 => {
                    dirs.insert(path.clone());
                    ws.mkdir(&path, true)
                }
                7 if parent != "/mnt" => {
                    dirs.insert(parent.clone());
                    ws.remove(&parent, true)
                }
                7 => Ok(()),
                8 => {
                    times.insert(path.clone(), UNIX_EPOCH + Duration::from_secs(step));
                    Ok(())
                }
                9 if step > 300 => ws.mkdir(&parent, true).and_then(|_| ws.hard_link(&other, &path)),
                _ => ws.write(&path, vec![step as u8; PACK_LIMIT as usize + 1]).map(|_| ()),
            };
            let snapshot = Snapshot { ws: ws.fork(), times: times.clone(), dirs: dirs.clone() };
            saved.save(&snapshot, &root(), false).unwrap();
            for (path, version, stored) in saved.take_written() {
                let _ = ws.store(path.as_str(), version, stored);
            }

            let mut loaded = base();
            let restored = Saved::new(state.path()).load(&mut loaded, &root()).unwrap();
            let (mut want, mut got) = (Vec::new(), Vec::new());
            tree(&ws, "/mnt", &mut want);
            tree(&loaded, "/mnt", &mut got);
            let differ: Vec<_> =
                want.iter().filter(|w| !got.contains(w)).chain(got.iter().filter(|g| !want.contains(g))).collect();
            assert_eq!(got, want, "after step {step}: {differ:?}");
            for change in ws.changes().unwrap() {
                let p = change.path.as_str();
                if change.kind != ChangeKind::Deleted && !ws.stat(p).unwrap().kind.is_dir() {
                    assert_eq!(restored.times.get(p), times.get(p), "time of {p} after step {step}");
                }
            }
        }
    }

    #[test]
    fn saved_files_are_read_from_disk_not_memory() {
        let state = tempfile::tempdir().unwrap();
        let mut ws = base();
        ws.write("/mnt/a.txt", "changed").unwrap();
        ws.write("/mnt/big.bin", vec![7u8; 1 << 20]).unwrap();
        let mut saved = Saved::new(state.path());
        saved.pack_limit = None;
        saved.save(&snap(&ws), &root(), false).unwrap();
        let written = saved.take_written();
        let paths: Vec<&str> = written.iter().map(|(path, _, _)| path.as_str()).collect();
        assert_eq!(paths, ["/mnt/a.txt", "/mnt/big.bin"], "the save says which files it wrote");

        // A workspace loaded from the save reads each file's object when it is read.
        let mut loaded = base();
        Saved::new(state.path()).load(&mut loaded, &root()).unwrap();
        assert_eq!(&*loaded.stored("/mnt/a.txt").unwrap().unwrap().key, "1");
        assert_eq!(loaded.stat("/mnt/big.bin").unwrap().size, 1 << 20, "sizes come from the objects");
        std::fs::write(state.path().join("objects/1"), "on disk").unwrap();
        assert_eq!(loaded.read_text("/mnt/a.txt").unwrap(), "on disk", "nothing was read at load");
    }

    #[test]
    fn moving_a_saved_file_writes_nothing_new() {
        let state = tempfile::tempdir().unwrap();
        let mut ws = base();
        ws.write("/mnt/out/app.bin", "built").unwrap();
        let mut saved = Saved::new(state.path());
        save_and_store(&mut saved, &mut ws);
        assert_eq!(objects(state.path()), ["1"]);

        // A program that builds into a folder and renames it, then copies and marks a file.
        ws.rename("/mnt/out", "/mnt/release").unwrap();
        ws.copy("/mnt/release/app.bin", "/mnt/app", false).unwrap();
        ws.set_executable("/mnt/app", true).unwrap();
        saved.save(&snap(&ws), &root(), false).unwrap();
        assert!(saved.take_written().is_empty(), "every file kept its object");
        assert_eq!(objects(state.path()), ["1"]);

        let mut loaded = base();
        Saved::new(state.path()).load(&mut loaded, &root()).unwrap();
        assert_eq!(loaded.read_text("/mnt/release/app.bin").unwrap(), "built");
        assert_eq!(loaded.stat("/mnt/app").unwrap().kind, Kind::File { executable: true });
        assert_eq!(loaded.changes().unwrap(), ws.changes().unwrap());
    }

    #[test]
    fn an_object_a_file_still_reads_from_is_kept() {
        let state = tempfile::tempdir().unwrap();
        let mut ws = base();
        ws.write("/mnt/log.txt", "first").unwrap();
        let mut saved = Saved::new(state.path());
        save_and_store(&mut saved, &mut ws);
        // A program still has the file open after it was replaced: the mount keeps a
        // fork from before, which reads from object 1.
        let still_open = ws.fork();
        ws.write("/mnt/log.txt", "second").unwrap();
        save_and_store(&mut saved, &mut ws);
        save_and_store(&mut saved, &mut ws);
        save_and_store(&mut saved, &mut ws);
        assert_eq!(objects(state.path()), ["1", "2"], "object 1 waits for the open file");
        assert_eq!(still_open.read_text("/mnt/log.txt").unwrap(), "first");

        drop(still_open);
        save_and_store(&mut saved, &mut ws);
        assert_eq!(objects(state.path()), ["2"], "and goes once nothing reads from it");
        assert_eq!(ws.read_text("/mnt/log.txt").unwrap(), "second");
    }

    #[test]
    fn an_object_a_file_open_from_before_a_take_over_reads_from_is_kept() {
        let state = tempfile::tempdir().unwrap();
        let mut ws = base();
        ws.write("/mnt/log.txt", "first").unwrap();
        let mut saved = Saved::new(state.path());
        saved.pack_limit = None;
        save_and_store(&mut saved, &mut ws);
        let still_open = ws.fork();
        drop(ws);
        // The workspace is loaded again, as an update in place does, with names of its
        // own for the same objects.
        let mut next = base();
        let mut fresh = Saved::new(state.path());
        fresh.pack_limit = None;
        fresh.load(&mut next, &root()).unwrap();
        fresh.take_over(saved);
        next.write("/mnt/log.txt", "second").unwrap();
        for _ in 0..3 {
            save_and_store(&mut fresh, &mut next);
        }
        assert_eq!(objects(state.path()), ["1", "2"], "object 1 waits for the file open from before");
        assert_eq!(still_open.read_text("/mnt/log.txt").unwrap(), "first");

        drop(still_open);
        save_and_store(&mut fresh, &mut next);
        assert_eq!(objects(state.path()), ["2"], "and goes once nothing reads from it");
    }

    #[test]
    fn an_object_two_replaced_copies_read_from_still_goes() {
        let state = tempfile::tempdir().unwrap();
        let mut ws = base();
        ws.write("/mnt/a.txt", "shared").unwrap();
        let mut saved = Saved::new(state.path());
        saved.pack_limit = None;
        save_and_store(&mut saved, &mut ws);
        // The copy reuses a.txt's object, so both files name object 1.
        ws.copy("/mnt/a.txt", "/mnt/b.txt", false).unwrap();
        save_and_store(&mut saved, &mut ws);
        assert_eq!(objects(state.path()), ["1"]);

        ws.write("/mnt/a.txt", "new a").unwrap();
        ws.write("/mnt/b.txt", "new b").unwrap();
        save_and_store(&mut saved, &mut ws);
        save_and_store(&mut saved, &mut ws);
        assert_eq!(objects(state.path()), ["2", "3"], "object 1 goes once neither file reads it");
    }

    #[test]
    fn a_file_written_as_a_draft_is_saved_as_it_is() {
        let state = tempfile::tempdir().unwrap();
        let mut ws = base();
        let mut saved = Saved::new(state.path());
        saved.pack_limit = None;
        let mut draft = saved.drafts().empty().unwrap();
        draft.write_at(0, b"hello world").unwrap();
        draft.write_at(6, b"there").unwrap();
        assert_eq!(draft.read_at(6, 100).unwrap(), "there");
        ws.write_stored("/mnt/new.txt", draft.finish().unwrap()).unwrap();
        saved.save(&snap(&ws), &root(), true).unwrap();
        assert!(saved.take_written().is_empty(), "the draft is the saved object");
        assert_eq!(objects(state.path()), ["1"]);
        assert!(saved.unsynced.is_empty(), "a durable save flushes the drafts it names");

        let mut loaded = base();
        Saved::new(state.path()).load(&mut loaded, &root()).unwrap();
        assert_eq!(loaded.read_text("/mnt/new.txt").unwrap(), "hello there");
    }

    #[test]
    fn a_draft_is_deleted_once_nothing_writes_or_reads_it() {
        let state = tempfile::tempdir().unwrap();
        let mut ws = base();
        let mut saved = Saved::new(state.path());
        let drafts = saved.drafts();
        let open = drafts.empty().unwrap();
        drop(drafts.empty().unwrap());
        save_and_store(&mut saved, &mut ws);
        assert_eq!(objects(state.path()), ["1"], "the dropped draft goes, the open one stays");
        assert!(saved.unsynced.contains("1"), "flushed once a durable save names it");

        // Finished, then replaced before any save named it.
        ws.write_stored("/mnt/a.txt", open.finish().unwrap()).unwrap();
        ws.write("/mnt/a.txt", "in memory").unwrap();
        save_and_store(&mut saved, &mut ws);
        assert_eq!(objects(state.path()), ["3"], "only the saved copy of a.txt is left");
        assert!(saved.unsynced.iter().all(|o| o == "3"));
    }

    #[test]
    fn a_draft_copied_from_a_file_leaves_the_file_as_it_was() {
        let state = tempfile::tempdir().unwrap();
        let mut ws = base();
        ws.write("/mnt/saved.txt", "saved bytes").unwrap();
        let mut saved = Saved::new(state.path());
        saved.pack_limit = None;
        save_and_store(&mut saved, &mut ws);
        let drafts = saved.drafts();

        // A saved file is copied from its object.
        let mut draft = drafts.copy(&ws, "/mnt/saved.txt", true).unwrap();
        draft.write_at(0, b"SAVED").unwrap();
        assert_eq!(ws.read_text("/mnt/saved.txt").unwrap(), "saved bytes");
        ws.write_stored("/mnt/saved.txt", draft.finish().unwrap()).unwrap();
        assert_eq!(ws.read_text("/mnt/saved.txt").unwrap(), "SAVED bytes");

        // A file from the source is read into it.
        let draft = drafts.copy(&ws, "/mnt/dir/b.txt", false).unwrap();
        draft.set_len(3).unwrap();
        assert_eq!(draft.read_at(0, 10).unwrap(), &b"b\0\0"[..]);
        assert_eq!(ws.read_text("/mnt/dir/b.txt").unwrap(), "b");
        save_and_store(&mut saved, &mut ws);
        save_and_store(&mut saved, &mut ws);
        assert_eq!(objects(state.path()), ["2", "3"], "saved.txt's new object and the draft still open");
    }

    /// A source that can only read whole files, as git's can, counting the reads.
    #[derive(Debug)]
    struct WholeFiles(MemorySource, AtomicU64);

    impl Source for WholeFiles {
        fn root(&self) -> sandfs_core::Result<SourceKey> {
            self.0.root()
        }

        fn read_dir(&self, key: &str) -> sandfs_core::Result<Listing> {
            self.0.read_dir(key)
        }

        fn read_file(&self, key: &str) -> sandfs_core::Result<Bytes> {
            self.1.fetch_add(1, Ordering::SeqCst);
            self.0.read_file(key)
        }
    }

    #[test]
    fn a_draft_reads_a_file_whole_once_from_a_source_without_ranges() {
        let state = tempfile::tempdir().unwrap();
        let big = vec![7u8; 2 * CHUNK as usize + 1];
        let source = Arc::new(WholeFiles(MemorySource::from_files([("big", big.clone())]).unwrap(), AtomicU64::new(0)));
        let mut ws = Workspace::new();
        ws.mount("/mnt", source.clone()).unwrap();
        let drafts = Saved::new(state.path()).drafts();

        let draft = drafts.copy(&ws, "/mnt/big", false).unwrap();
        assert_eq!(source.1.load(Ordering::SeqCst), 1);
        assert_eq!(draft.read_at(0, big.len() as u64 + 1).unwrap(), &big[..]);

        // In chunks, every chunk would read the whole file again.
        drafts.copy(&ws, "/mnt/big", true).unwrap();
        assert_eq!(source.1.load(Ordering::SeqCst), 4);
    }

    #[cfg(unix)]
    #[test]
    fn a_draft_of_a_sparse_file_stays_sparse() {
        use std::os::unix::fs::MetadataExt;

        let (state, data) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let sparse = std::fs::File::create(data.path().join("sparse")).unwrap();
        sparse.set_len(4 * CHUNK).unwrap();
        drop(sparse);
        let mut ws = Workspace::new();
        ws.mount("/mnt", Arc::new(sandfs_core::DirSource::new(data.path()).unwrap())).unwrap();
        let drafts = Saved::new(state.path()).drafts();

        let draft = drafts.copy(&ws, "/mnt/sparse", true).unwrap();
        let meta = draft.file.metadata().unwrap();
        assert_eq!(meta.len(), 4 * CHUNK);
        assert!(meta.blocks() * 512 < CHUNK, "{} bytes on disk", meta.blocks() * 512);
    }

    #[test]
    fn small_files_are_saved_into_one_pack_and_forked_as_one_link() {
        let (state, fork) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let mut ws = base();
        for i in 0..100 {
            ws.write(&format!("/mnt/many/{i}.txt"), format!("file {i}")).unwrap();
        }
        ws.write("/mnt/big.bin", vec![7u8; PACK_LIMIT as usize + 1]).unwrap();
        let mut saved = Saved::new(state.path());
        save_and_store(&mut saved, &mut ws);
        assert_eq!(objects(state.path()), ["1", "2"], "one object for the big file, one pack for the small ones");
        let key = ws.stored("/mnt/many/7.txt").unwrap().unwrap().key;
        assert!(key.starts_with("2@") && key.ends_with("+6"), "{key}");
        assert_eq!(ws.read_text("/mnt/many/42.txt").unwrap(), "file 42");
        assert_eq!(ws.read_range("/mnt/many/42.txt", 5, 100).unwrap(), "42", "ranges stop at the file's end");

        // A load reads the pack's size once and each file's bytes when it is read.
        let mut loaded = base();
        let mut reloaded = Saved::new(state.path());
        reloaded.load(&mut loaded, &root()).unwrap();
        assert_eq!(loaded.read_text("/mnt/many/99.txt").unwrap(), "file 99");
        assert_eq!(loaded.stat("/mnt/big.bin").unwrap().size, PACK_LIMIT + 1);
        assert_eq!(reloaded.packs["2"].objects.len(), 100);

        saved.copy_to(fork.path()).unwrap();
        assert_eq!(objects(fork.path()), ["1", "2"], "a fork links the pack once");
        let mut forked = base();
        Saved::new(fork.path()).load(&mut forked, &root()).unwrap();
        assert_eq!(forked.read_text("/mnt/many/0.txt").unwrap(), "file 0");
        assert_eq!(forked.changes().unwrap(), ws.changes().unwrap());
    }

    #[test]
    fn a_small_draft_is_packed_and_a_big_one_kept_as_it_is() {
        let state = tempfile::tempdir().unwrap();
        let mut ws = base();
        let mut saved = Saved::new(state.path());
        let drafts = saved.drafts();
        let mut small = drafts.empty().unwrap();
        small.write_at(0, b"small").unwrap();
        ws.write_stored("/mnt/small.txt", small.finish().unwrap()).unwrap();
        let big = drafts.empty().unwrap();
        big.set_len(PACK_LIMIT + 1).unwrap();
        ws.write_stored("/mnt/big.bin", big.finish().unwrap()).unwrap();
        // A copy shares the draft, and the save packs it once.
        ws.copy("/mnt/small.txt", "/mnt/copy.txt", false).unwrap();
        save_and_store(&mut saved, &mut ws);
        assert_eq!(&*ws.stored("/mnt/small.txt").unwrap().unwrap().key, "3@0+5");
        assert_eq!(objects(state.path()), ["1", "2", "3"], "the small draft waits a save");
        assert_eq!(&*ws.stored("/mnt/big.bin").unwrap().unwrap().key, "2", "the big draft is the saved object");
        assert_eq!(ws.read_text("/mnt/copy.txt").unwrap(), "small");

        save_and_store(&mut saved, &mut ws);
        assert_eq!(objects(state.path()), ["2", "3"], "then goes, since nothing reads it any more");
        assert_eq!(saved.packs["3"].objects.len(), 1, "the copy shares the small file's object");

        // Writing into a packed file copies its range into a new draft.
        let mut draft = drafts.copy(&ws, "/mnt/small.txt", true).unwrap();
        draft.write_at(5, b"er").unwrap();
        assert_eq!(draft.read_at(0, 100).unwrap(), "smaller");
    }

    #[test]
    fn a_pack_goes_once_nothing_reads_it() {
        let state = tempfile::tempdir().unwrap();
        let mut ws = base();
        ws.write("/mnt/a.txt", "a").unwrap();
        ws.write("/mnt/b.txt", "b").unwrap();
        let mut saved = Saved::new(state.path());
        save_and_store(&mut saved, &mut ws);
        assert_eq!(objects(state.path()), ["1"]);

        let still_open = ws.fork();
        ws.write("/mnt/a.txt", "a again").unwrap();
        ws.write("/mnt/b.txt", "b again").unwrap();
        save_and_store(&mut saved, &mut ws);
        save_and_store(&mut saved, &mut ws);
        save_and_store(&mut saved, &mut ws);
        assert_eq!(objects(state.path()), ["1", "2"], "pack 1 waits for the open files");
        assert_eq!(still_open.read_text("/mnt/a.txt").unwrap(), "a");

        drop(still_open);
        save_and_store(&mut saved, &mut ws);
        assert_eq!(objects(state.path()), ["2"]);
        assert!(!saved.packs.contains_key("1"));
    }

    #[test]
    fn packs_mostly_replaced_or_many_and_small_are_written_again() {
        let state = tempfile::tempdir().unwrap();
        let mut ws = base();
        for i in 0..10 {
            ws.write(&format!("/mnt/{i}.txt"), "0123456789").unwrap();
        }
        let mut saved = Saved::new(state.path());
        save_and_store(&mut saved, &mut ws);
        // Six of the ten files replaced: pack 1 is mostly unused, so the next save
        // writes its four other files again, into its own pack.
        for i in 0..6 {
            ws.write(&format!("/mnt/{i}.txt"), "changed").unwrap();
        }
        save_and_store(&mut saved, &mut ws);
        save_and_store(&mut saved, &mut ws);
        assert_eq!(&*ws.stored("/mnt/9.txt").unwrap().unwrap().key, "3@30+10", "after 6, 7 and 8");
        save_and_store(&mut saved, &mut ws);
        assert_eq!(objects(state.path()), ["2", "3"]);

        // Each save of one small file makes a small pack; once there are more than
        // SMALL_PACKS, a save writes them all into one.
        for i in 0..2 * SMALL_PACKS {
            ws.write(&format!("/mnt/one/{i}.txt"), "1").unwrap();
            save_and_store(&mut saved, &mut ws);
        }
        save_and_store(&mut saved, &mut ws);
        assert!(saved.packs.len() <= SMALL_PACKS + 2, "{} packs", saved.packs.len());
        assert_eq!(ws.read_text("/mnt/one/0.txt").unwrap(), "1");
        assert_eq!(ws.read_text("/mnt/9.txt").unwrap(), "0123456789");
        let mut loaded = base();
        Saved::new(state.path()).load(&mut loaded, &root()).unwrap();
        assert_eq!(loaded.changes().unwrap(), ws.changes().unwrap());
    }

    #[test]
    fn copies_of_a_packed_file_count_its_bytes_once() {
        let state = tempfile::tempdir().unwrap();
        let mut ws = base();
        for i in 0..10 {
            ws.write(&format!("/mnt/{i}.txt"), "0123456789").unwrap();
        }
        let mut saved = Saved::new(state.path());
        save_and_store(&mut saved, &mut ws);
        // Ten copies of one file and the nine others replaced: the pack holds 10 live
        // bytes of 100, however many paths name them.
        for i in 0..10 {
            ws.copy("/mnt/0.txt", &format!("/mnt/copy{i}.txt"), false).unwrap();
        }
        for i in 1..10 {
            ws.write(&format!("/mnt/{i}.txt"), "changed").unwrap();
        }
        save_and_store(&mut saved, &mut ws);
        assert_eq!(saved.packs_to_repack(), HashSet::from(["1".to_owned()]));
    }

    #[test]
    fn a_failed_save_leaves_no_pack_behind() {
        let state = tempfile::tempdir().unwrap();
        let mut ws = base();
        ws.write("/mnt/a.txt", "packed first").unwrap();
        // Saved after a.txt, and unreadable: its source holds no such file.
        let gone = Stored { source: Arc::new(MemorySource::default()), key: "gone".into(), size: 4 };
        ws.write_stored("/mnt/b.txt", gone).unwrap();
        let mut saved = Saved::new(state.path());
        assert!(saved.save(&snap(&ws), &root(), false).is_err());
        assert!(objects(state.path()).is_empty(), "{:?}", objects(state.path()));

        // Nor one whose index could not be written.
        ws.remove("/mnt/b.txt", false).unwrap();
        std::fs::create_dir(state.path().join("changes.json.tmp")).unwrap();
        assert!(saved.save(&snap(&ws), &root(), false).is_err());
        assert!(objects(state.path()).is_empty(), "{:?}", objects(state.path()));
        std::fs::remove_dir(state.path().join("changes.json.tmp")).unwrap();
        saved.save(&snap(&ws), &root(), false).unwrap();
        assert_eq!(objects(state.path()).len(), 1);
    }

    #[test]
    fn deletes_in_the_background_are_done_once_the_workspace_is_let_go() {
        let state = tempfile::tempdir().unwrap();
        let mut ws = base();
        let mut saved = Saved::new(state.path());
        saved.pack_limit = None;
        for round in 0..3 {
            // More files replaced than are deleted in place.
            for i in 0..2 * DELETE_IN_PLACE {
                ws.write(&format!("/mnt/{i}.txt"), round.to_string()).unwrap();
            }
            save_and_store(&mut saved, &mut ws);
        }
        drop(saved);
        assert_eq!(objects(state.path()).len(), 2 * DELETE_IN_PLACE * 2, "the last two saves' objects");
    }

    #[test]
    fn object_names_outside_the_objects_folder_are_refused() {
        assert_eq!(place("12").unwrap(), Place { file: "12", range: None });
        assert_eq!(place("12@4096+5").unwrap(), Place { file: "12", range: Some((4096, 5)) });
        for name in ["", "../x", "1@", "1@2", "1@+3", "1@2+", "a@1+2", "1@2+3+4", "1@99999999999999999999+1"] {
            assert!(place(name).is_err(), "{name:?}");
        }
    }

    #[test]
    fn index_paths_cannot_leave_the_mount() {
        assert!(under(&root(), "../etc/passwd").is_err());
        assert!(under(&root(), "a/../../x").is_err());
        assert_eq!(under(&root(), "a/b").unwrap().as_str(), "/mnt/a/b");
    }
}
