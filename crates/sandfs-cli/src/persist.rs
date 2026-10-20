//! Saving a mounted workspace's changes to disk and replaying them on the next mount.
//!
//! The base of a workspace never needs saving: it is a pinned git commit or a host
//! directory. What is saved is the overlay, as one entry per changed path in
//! `changes.json` plus one file per saved file body under `objects/`. A save rewrites
//! the small index but writes an object only for files whose version changed since the
//! last save, so saving costs the index plus whatever was written in between.
//!
//! The entries come from [`Workspace::changes`] (files) plus the directories the mount
//! created or removed, which `changes` does not report.
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
    },
    Dir {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mtime: Option<Time>,
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

    fn open(&self, key: &str) -> sandfs_core::Result<std::fs::File> {
        check_object(key).map_err(|err| Error::Source(err.to_string()))?;
        std::fs::File::open(self.dir.join(key)).map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => Error::Source(ObjectGone(key.to_owned()).to_string()),
            _ => Error::Source(format!("reading saved object {key}: {err}")),
        })
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
        let mut data = Vec::new();
        self.open(key)?.read_to_end(&mut data).map_err(|err| Error::Source(err.to_string()))?;
        Ok(data.into())
    }

    fn read_range(&self, key: &str, offset: u64, len: u64) -> sandfs_core::Result<Bytes> {
        let mut file = self.open(key)?;
        let mut data = Vec::new();
        file.seek(SeekFrom::Start(offset))
            .and_then(|_| file.take(len).read_to_end(&mut data))
            .map_err(|err| Error::Source(err.to_string()))?;
        Ok(data.into())
    }
}

/// Where a mount writes files: each into a new object of the workspace, a [`Draft`].
#[derive(Clone, Debug)]
pub struct Drafts(Arc<Objects>);

/// Bytes read into a draft at a time, from a file that cannot be copied on disk.
const CHUNK: u64 = 8 << 20;

impl Drafts {
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

    /// A new draft holding what `path` in `ws` holds now. A saved file is copied on
    /// disk, which shares its blocks where the disk can (APFS, btrfs, XFS). Any other
    /// file is read in chunks when its source reads ranges cheaply (`ranged`), so its
    /// size never sits in memory; otherwise it is read once, whole, since each chunk
    /// would read the whole file again.
    pub fn copy(&self, ws: &Workspace, path: &str, ranged: bool) -> Result<Draft> {
        if let Some(stored) = ws.stored(path)?
            && std::ptr::addr_eq(Arc::as_ptr(&stored.source), Arc::as_ptr(&self.0))
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
    /// Objects written since the last durable save that names them, not yet flushed to
    /// disk.
    unsynced: HashSet<String>,
    /// See [`Saved::take_written`].
    written: Written,
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
            unsynced: HashSet::new(),
            written: Written::new(),
        }
    }

    /// Where the bytes of `object`, `size` long, are.
    fn stored(&self, object: SourceKey, size: u64) -> Stored {
        Stored { source: self.store.clone(), key: object, size }
    }

    /// Where the serving process writes files, as drafts of new objects.
    pub fn drafts(&self) -> Drafts {
        Drafts(self.store.clone())
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
            Entry::File { .. } => PathState::File,
            Entry::Dir { .. } => PathState::Dir,
            Entry::Absent => PathState::Absent,
        };
        Ok(changes.entries.iter().map(|(rel, entry)| (rel.clone(), state(entry))).collect())
    }

    /// Replay the saved changes onto `ws`, whose source is mounted at `root`.
    ///
    /// Loading only reads, so any process may load a workspace, even while another
    /// one serves it.
    pub fn load(&mut self, ws: &mut Workspace, root: &VPath) -> Result<Restored> {
        let changes = self.read_index()?;
        let mut restored = Restored::default();
        // Files saved as one object share its name.
        let mut names: HashMap<&str, SourceKey> = HashMap::new();
        for (rel, entry) in &changes.entries {
            let path = under(root, rel)?;
            let object = match entry {
                Entry::File { object, .. } => {
                    Some(names.entry(object).or_insert_with(|| object.as_str().into()).clone())
                }
                _ => None,
            };
            self.apply(ws, &path, entry, object.clone()).with_context(|| format!("restoring {rel}"))?;
            let mtime = match entry {
                Entry::File { mtime, .. } | Entry::Dir { mtime } => *mtime,
                Entry::Absent => None,
            };
            if let Some(mtime) = mtime {
                restored.times.insert(path.to_string(), from_time(mtime));
            }
            match (entry, object) {
                (Entry::File { .. }, Some(object)) => {
                    let version = ws.stat(path.as_str())?.version;
                    // A save that wasn't durable may have left it unflushed before the
                    // process that wrote it went away, so the next durable save flushes it.
                    self.unsynced.insert(object.to_string());
                    self.objects.insert(rel.clone(), (version, object));
                }
                _ => {
                    restored.dirs.insert(path.to_string());
                }
            }
        }
        // New objects must never reuse the name of one the index still names, nor of any
        // object an earlier save used.
        let last = self.objects.values().filter_map(|(_, object)| object.parse::<u64>().ok()).max();
        self.store.next.fetch_max((last.unwrap_or(0) + 1).max(changes.next), Ordering::SeqCst);
        Ok(restored)
    }

    /// Delete objects the loaded index does not use, left by a crash or by the last
    /// session's delayed deletes. Only the process that serves the workspace, holding
    /// its writer lock, may do this: it alone writes objects.
    pub fn collect_garbage(&mut self) {
        let live: HashSet<&str> = self.objects.values().map(|(_, object)| &**object).collect();
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

    /// Replay one entry; a file reads from `object`, its object's shared name.
    fn apply(&self, ws: &mut Workspace, path: &VPath, entry: &Entry, object: Option<SourceKey>) -> Result<()> {
        let p = path.as_str();
        let current = match ws.stat(p) {
            Ok(meta) => Some(meta.kind),
            // Below a file that replaced a directory: the file already hides it.
            Err(Error::NotFound(_) | Error::NotADirectory(_)) if *entry == Entry::Absent => None,
            Err(Error::NotFound(_)) => None,
            Err(err) => return Err(err.into()),
        };
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
            Entry::File { executable, .. } => {
                let object = object.context("a saved file without its object")?;
                check_object(&object)?;
                // A directory in the way goes first.
                if current == Some(Kind::Dir) {
                    ws.remove(p, true)?;
                }
                // The bytes stay on disk; only the size is read now. A missing object
                // means a newer save replaced it while this index was being loaded.
                let size = match std::fs::metadata(self.objects_dir().join(&*object)) {
                    Ok(meta) => meta.len(),
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                        return Err(ObjectGone(object.to_string()).into());
                    }
                    Err(err) => return Err(err).with_context(|| format!("reading saved object {object}")),
                };
                ws.write_stored(p, self.stored(object, size))?;
                ws.set_executable(p, *executable)?;
            }
        }
        Ok(())
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
        let mut kept = HashMap::new();
        let mut written = Written::new();
        for change in snap.ws.changes()? {
            let rel = relative(root, &change.path)?;
            let entry = match change.kind {
                ChangeKind::Deleted => Entry::Absent,
                ChangeKind::Added | ChangeKind::Modified => {
                    self.entry(snap, &change.path, &rel, &mut kept, &mut written)?
                }
            };
            entries.insert(rel, entry);
        }
        for dir in &snap.dirs {
            let path = VPath::parse(dir)?;
            let rel = relative(root, &path)?;
            let entry = match snap.ws.stat(dir) {
                // A directory that replaced a deleted base file overrides its deletion.
                Ok(meta) if meta.kind.is_dir() => Entry::Dir { mtime: snap.times.get(dir).map(|t| to_time(*t)) },
                _ if entries.contains_key(&rel) => continue,
                Ok(_) => self.entry(snap, &path, &rel, &mut kept, &mut written)?,
                // Gone, or below a file that replaced one of its parents.
                Err(Error::NotFound(_) | Error::NotADirectory(_)) => Entry::Absent,
                Err(err) => return Err(err.into()),
            };
            entries.insert(rel, entry);
        }

        let index = serde_json::to_vec(&Changes { entries, next: self.store.next.load(Ordering::SeqCst) })?;
        let tmp = self.dir.join("changes.json.tmp");
        if durable {
            // Objects reach the disk before the index that names them.
            let live: HashSet<&str> = kept.values().map(|(_, object)| &**object).collect();
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
        }
        std::fs::write(&tmp, index).context("writing changes.json")?;
        if durable {
            sync_path(&tmp)?;
        }
        std::fs::rename(&tmp, self.index_path()).context("writing changes.json")?;

        // The new index is in place: objects replaced by the save before this one are
        // now two indexes old. One that a file here still reads from waits until none does.
        let live: HashSet<&str> = kept.values().map(|(_, object)| &**object).collect();
        let mut waiting = Vec::new();
        for object in std::mem::take(&mut self.doomed) {
            if live.contains(&*object) {
                continue;
            }
            if Arc::strong_count(&object) == 1 {
                let _ = std::fs::remove_file(self.objects_dir().join(&*object));
                self.unsynced.remove(&*object);
            } else {
                waiting.push(object);
            }
        }
        // Files saved as one object (copies) name it once each, but it waits once: a
        // second entry here would count as a file still reading from it.
        let mut replaced = HashSet::new();
        for object in self.objects.values().map(|(_, object)| object) {
            if !live.contains(&**object) && replaced.insert(object.clone()) {
                waiting.push(object.clone());
            }
        }
        self.doomed = waiting;
        self.objects = kept;
        // The index names these files' objects now, even if flushing it fails below.
        self.written = written;
        if durable {
            sync_path(&self.dir).map_err(|err| err.context(NotDurable))?;
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

    /// The entry for a file or directory, writing its bytes to a new object unless the
    /// last save already holds this version.
    fn entry(
        &mut self,
        snap: &Snapshot,
        path: &VPath,
        rel: &str,
        kept: &mut HashMap<String, (u64, SourceKey)>,
        written: &mut Written,
    ) -> Result<Entry> {
        let p = path.as_str();
        let meta = snap.ws.stat(p)?;
        let mtime = snap.times.get(p).map(|t| to_time(*t));
        match meta.kind {
            Kind::Dir => Ok(Entry::Dir { mtime }),
            Kind::File { executable } => {
                let object = match self.objects.get(rel) {
                    Some((version, object)) if *version == meta.version => object.clone(),
                    // A saved file moved, copied or made executable keeps its object.
                    _ => match snap.ws.stored(p)? {
                        Some(stored) if self.is_ours(&stored) => stored.key,
                        _ => {
                            // A ranged read, which keeps no copy of a base file in memory.
                            let data = snap.ws.read_range(p, 0, meta.size)?;
                            let object = self.write_object(&data)?;
                            written.push((path.clone(), meta.version, self.stored(object.clone(), data.len() as u64)));
                            object
                        }
                    },
                };
                kept.insert(rel.to_owned(), (meta.version, object.clone()));
                Ok(Entry::File { executable, object: object.to_string(), mtime })
            }
        }
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
            for entry in changes.entries.values() {
                let Entry::File { object, .. } = entry else { continue };
                check_object(object)?;
                let (from, target) = (self.objects_dir().join(object), objects.join(object));
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

/// Object names are numbers; refuse anything else rather than follow it on disk.
fn check_object(object: &str) -> Result<()> {
    if object.is_empty() || !object.bytes().all(|b| b.is_ascii_digit()) {
        bail!("invalid object name {object:?} in changes.json");
    }
    Ok(())
}

/// `path` relative to `root`, as stored in the index.
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
        assert_eq!(reloaded.unsynced.len(), 2, "a new process flushes what it loaded at its first durable save");
        assert_eq!(loaded.read_text("/mnt/a.txt").unwrap(), "changed");
        assert_eq!(loaded.stat("/mnt/new/run.sh").unwrap().kind, Kind::File { executable: true });
        assert_eq!(listing(&loaded, "/mnt/dir"), ["b.txt"], "the removed directory stays removed");
        assert!(loaded.list("/mnt/empty").unwrap().is_empty(), "empty directories are kept");
        assert_eq!(loaded.changes().unwrap(), ws.changes().unwrap());
        assert_eq!(restored.times["/mnt/a.txt"], mtime);
        assert!(restored.dirs.contains("/mnt/empty"));
    }

    #[test]
    fn saves_write_only_files_that_changed_and_drop_stale_objects() {
        let state = tempfile::tempdir().unwrap();
        let mut ws = base();
        ws.write("/mnt/one.txt", "1").unwrap();
        ws.write("/mnt/two.txt", "2").unwrap();
        let mut saved = Saved::new(state.path());
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
        saved.save(&snap(&ws), &root(), false).unwrap();
        ws.remove("/mnt/two.txt", false).unwrap();
        saved.save(&snap(&ws), &root(), false).unwrap();

        // The next session deletes object 2, which a reader of the older index may
        // still be about to open, so it must never name a new object 2.
        let mut next = base();
        let mut saved = Saved::new(state.path());
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

    #[test]
    fn saved_files_are_read_from_disk_not_memory() {
        let state = tempfile::tempdir().unwrap();
        let mut ws = base();
        ws.write("/mnt/a.txt", "changed").unwrap();
        ws.write("/mnt/big.bin", vec![7u8; 1 << 20]).unwrap();
        let mut saved = Saved::new(state.path());
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
    fn an_object_two_replaced_copies_read_from_still_goes() {
        let state = tempfile::tempdir().unwrap();
        let mut ws = base();
        ws.write("/mnt/a.txt", "shared").unwrap();
        let mut saved = Saved::new(state.path());
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
    fn index_paths_cannot_leave_the_mount() {
        assert!(under(&root(), "../etc/passwd").is_err());
        assert!(under(&root(), "a/../../x").is_err());
        assert_eq!(under(&root(), "a/b").unwrap().as_str(), "/mnt/a/b");
    }
}
