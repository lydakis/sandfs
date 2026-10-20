//! Copy-on-write workspaces over lazily read sources.
//!
//! A [`Workspace`] is a set of mounts (the read-only base) plus an overlay holding
//! everything written, created or deleted since. Both live in persistent maps, so
//! [`Workspace::fork`] is a cheap clone that shares all structure, and the overlay is
//! exactly the change set. Base content is read through a cache shared by every fork,
//! so a file fetched once is never fetched again.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use bytes::Bytes;
use imbl::ordmap::DiffItem;
use imbl::{OrdMap, OrdSet};

use crate::change::{Change, ChangeKind};
use crate::error::{Error, Result};
use crate::path::VPath;
use crate::source::{Kind, Listing, Source, SourceEntry, SourceKey, slice};

static NEXT_MOUNT_ID: AtomicU64 = AtomicU64::new(1);

struct Mount {
    id: u64,
    source: Arc<dyn Source>,
    root: SourceKey,
}

/// Mounts are equal when they are the same mount: ids are never reused.
impl PartialEq for Mount {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl fmt::Debug for Mount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Mount").field("id", &self.id).field("source", &self.source).finish()
    }
}

/// A cache entry. Its own lock is held while the source loads it, so forks on other
/// threads that want the same entry wait for that load instead of repeating it.
type Slot<T> = Mutex<Option<T>>;

/// A `(mount id, source key)` pair.
type CacheKey = (u64, SourceKey);

/// Listings and blobs fetched from sources; keys are immutable, so forks share it.
#[derive(Default)]
struct Cache {
    dirs: Mutex<HashMap<CacheKey, Arc<Slot<Arc<Listing>>>>>,
    blobs: Mutex<HashMap<CacheKey, Arc<Slot<Bytes>>>>,
    /// Entries already found, by mount id and path below the mount, so resolving a
    /// path costs one lookup instead of one listing lookup per component. Only
    /// entries that exist are kept, so probing for missing names costs no memory.
    entries: Mutex<HashMap<u64, HashMap<Box<str>, SourceEntry>>>,
    /// Directories already prefetched, and whether recursively, so each is asked once.
    prefetched: Mutex<HashSet<(CacheKey, bool)>>,
    /// Base files each directory needed fetched in the current look: sizes missing
    /// from its listing, and files a walk or a program reading by path read. A set,
    /// so a file read in pieces counts once.
    touched: Mutex<HashMap<CacheKey, HashSet<SourceKey>>>,
    /// Directories listed again for their files' sizes, at most once each.
    sized: Mutex<HashMap<CacheKey, Arc<Slot<Arc<Listing>>>>>,
}

impl Cache {
    /// The cached value for `key`, loading it with `fetch` on a miss. Concurrent
    /// misses on one key fetch once. A failed fetch caches nothing, so it is retried.
    fn get_or_load<T: Clone>(
        map: &Mutex<HashMap<CacheKey, Arc<Slot<T>>>>,
        key: CacheKey,
        fetch: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        // The map lock is held only to find the slot, so loads of different keys
        // never wait for each other.
        let slot = map.lock().expect("cache lock").entry(key).or_default().clone();
        let mut value = slot.lock().expect("cache slot lock");
        if let Some(value) = value.as_ref() {
            return Ok(value.clone());
        }
        let fetched = fetch()?;
        *value = Some(fetched.clone());
        Ok(fetched)
    }

    fn entry(&self, mount: u64, rel: &str) -> Option<SourceEntry> {
        self.entries.lock().expect("cache lock").get(&mount)?.get(rel).cloned()
    }

    fn remember_entry(&self, mount: u64, rel: &str, entry: SourceEntry) {
        self.entries.lock().expect("cache lock").entry(mount).or_default().insert(rel.into(), entry);
    }

    /// Replace a cached listing with a newer answer from the same source.
    fn replace_listing(&self, key: CacheKey, listing: Arc<Listing>) {
        self.dirs.lock().expect("cache lock").insert(key, Arc::new(Mutex::new(Some(listing))));
    }

    /// The cached blob for `key`, if it is loaded and not being loaded right now.
    fn peek_blob(&self, key: &CacheKey) -> Option<Bytes> {
        let slot = self.blobs.lock().expect("cache lock").get(key)?.clone();
        slot.try_lock().ok()?.clone()
    }
}

/// File content: bytes written into the overlay, a reference to a base file that
/// has not been (or need not be) read, or bytes kept in a store outside memory.
/// References let copies and renames of base files stay lazy.
#[derive(Clone, Debug)]
enum Content {
    Bytes(Bytes),
    Base { mount: Arc<Mount>, key: SourceKey, size: Option<u64>, modified: Option<SystemTime> },
    Stored(Stored),
}

/// Where a file's bytes are kept outside memory: a file in a source its owner manages,
/// such as files already saved to disk. See [`Workspace::write_stored`].
///
/// The workspace reads a stored file from its source every time and never keeps a
/// copy, so it costs no memory however large it is; the source (or the operating
/// system's cache under it) decides what stays in memory.
#[derive(Clone, Debug)]
pub struct Stored {
    /// The source holding the bytes. It needn't be mounted anywhere.
    pub source: Arc<dyn Source>,
    /// The file's key in `source`.
    pub key: SourceKey,
    /// The file's size in bytes.
    pub size: u64,
}

/// Content is equal when it is the same content: the same written bytes (not just
/// equal ones), the same source file or the same stored file. That is enough to skip what forks share
/// when comparing workspaces without reading anything; unequal content may still
/// hold equal bytes.
impl PartialEq for Content {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Content::Bytes(a), Content::Bytes(b)) => a.as_ptr() == b.as_ptr() && a.len() == b.len(),
            (Content::Base { mount: a, key: x, .. }, Content::Base { mount: b, key: y, .. }) => a.id == b.id && x == y,
            (Content::Stored(a), Content::Stored(b)) => Arc::ptr_eq(&a.source, &b.source) && a.key == b.key,
            _ => false,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Overlay {
    File {
        content: Content,
        kind: Kind,
        version: u64,
    },
    /// One name of a file with several (hard links). The file itself is in
    /// [`Workspace`]'s `links`, so a write through any name is seen through all of them.
    Linked {
        id: u64,
    },
    /// `opaque` directories were deleted and recreated: base entries below are hidden.
    Dir {
        opaque: bool,
    },
    /// A whiteout. It keeps the version the deletion was given, so a stale version
    /// token for the old file cannot match the missing path.
    Deleted {
        version: u64,
    },
}

/// A file with several names, made by [`Workspace::hard_link`].
#[derive(Clone, Debug, PartialEq)]
struct Linked {
    content: Content,
    kind: Kind,
    version: u64,
    /// Every path naming it.
    names: OrdSet<VPath>,
}

/// Where a path's base (pre-overlay) node comes from.
#[derive(Clone, Debug)]
enum BaseNode {
    MountRoot(Arc<Mount>),
    Entry(Arc<Mount>, SourceEntry),
    /// A directory that exists only because a mount point lies below it.
    Implicit,
}

impl BaseNode {
    fn kind(&self) -> Kind {
        match self {
            BaseNode::MountRoot(_) | BaseNode::Implicit => Kind::Dir,
            BaseNode::Entry(_, entry) => entry.kind,
        }
    }
}

/// A path as the workspace currently sees it.
#[derive(Clone, Debug)]
enum Resolved {
    Dir { base: Option<BaseNode> },
    File { content: Content, kind: Kind, version: u64 },
}

/// One entry returned by [`Workspace::list`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    /// Entry name.
    pub name: String,
    /// What the entry is.
    pub kind: Kind,
    /// Size in bytes, when known without reading the file.
    pub size: Option<u64>,
}

/// File metadata returned by [`Workspace::stat`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Metadata {
    /// What the path is.
    pub kind: Kind,
    /// Size in bytes; 0 for directories.
    pub size: u64,
    /// Optimistic version: 0 for untouched base files. Every change gets a new
    /// version from a per-workspace clock, so versions are never reused.
    pub version: u64,
    /// When the bytes last changed, as the source reports it, for a file no one has
    /// written; `None` for everything else. A copy or a move keeps it.
    pub modified: Option<SystemTime>,
}

/// What one workspace listed and read lately, to tell a walk reading through
/// directories from a look around. Each fork starts its own, since a walk is one
/// program's doing.
#[derive(Default)]
struct Walks {
    /// Directories listed through [`Workspace::list`].
    listed: HashSet<VPath>,
    /// Directories a walk listed (listed inside a listed directory), and the one it
    /// started from, each with the directory the walk started at.
    starts: HashMap<VPath, VPath>,
    /// What each walk, by its start, needed files in lately; `None` for files needed by
    /// path, outside any walk.
    reading: HashMap<Option<VPath>, Reading>,
}

/// The last directories a walk, or a program reading files by path, needed files in,
/// with no pause longer than [`WALK_GAP`].
struct Reading {
    /// At most [`WALK_DIRS`], the latest last.
    recent: VecDeque<VPath>,
    last: Instant,
}

/// A copy-on-write filesystem view: read-only mounts plus an overlay of changes.
#[derive(Clone, Default)]
pub struct Workspace {
    mounts: OrdMap<VPath, Arc<Mount>>,
    overlay: OrdMap<VPath, Overlay>,
    /// Files with several names, by the id their [`Overlay::Linked`] entries share.
    links: OrdMap<u64, Linked>,
    cache: Arc<Cache>,
    walks: Arc<Mutex<Walks>>,
    /// The last version handed out.
    clock: u64,
}

impl fmt::Debug for Workspace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Workspace")
            .field("mounts", &self.mounts.keys().map(VPath::as_str).collect::<Vec<_>>())
            .field("overlay_entries", &self.overlay.len())
            .finish()
    }
}

impl Workspace {
    /// An empty workspace: just the root directory.
    pub fn new() -> Self {
        Self::default()
    }

    /// Mount `source` read-only at `path`. The path must not exist yet, except as a
    /// directory implied by another mount.
    pub fn mount(&mut self, path: &str, source: Arc<dyn Source>) -> Result<()> {
        let path = VPath::parse(path)?;
        if path.is_root() {
            return Err(Error::Unsupported(path.to_string(), "cannot mount at the root"));
        }
        match self.resolve(&path) {
            Err(Error::NotFound(_)) => {}
            Ok(Resolved::Dir { base: Some(BaseNode::Implicit) }) => {}
            Ok(_) => return Err(Error::AlreadyExists(path.to_string())),
            Err(err) => return Err(err),
        }
        let root = source.root()?;
        // A mount can go where something was deleted: deleted ancestors become empty
        // directories (still hiding their old base entries) and the whiteout goes.
        for ancestor in path.ancestors() {
            if let Some(Overlay::Deleted { .. }) = self.overlay.get(&ancestor) {
                self.overlay.insert(ancestor, Overlay::Dir { opaque: true });
            }
        }
        self.overlay.remove(&path);
        let mount = Mount { id: NEXT_MOUNT_ID.fetch_add(1, Ordering::Relaxed), source, root };
        self.mounts.insert(path, Arc::new(mount));
        Ok(())
    }

    /// An independent copy that shares all unchanged structure and the source cache.
    ///
    /// Forking costs O(1); later writes to either copy are invisible to the other.
    /// A fork is also how snapshots work: keep one and assign it back to roll back.
    pub fn fork(&self) -> Workspace {
        Workspace { walks: Arc::default(), ..self.clone() }
    }

    /// True if the path exists.
    pub fn exists(&self, path: &str) -> bool {
        VPath::parse(path).is_ok_and(|path| self.resolve(&path).is_ok())
    }

    /// Metadata for a path. When a source did not report a file's size, the file is
    /// read once to find it, so the size is always exact.
    pub fn stat(&self, path: &str) -> Result<Metadata> {
        let path = VPath::parse(path)?;
        Ok(match self.resolve(&path)? {
            Resolved::Dir { .. } => Metadata { kind: Kind::Dir, size: 0, version: 0, modified: None },
            Resolved::File { content, kind, version } => {
                let size = match content_size(&content) {
                    Some(size) => size,
                    None => match self.cached_size(&content) {
                        Some(size) => size,
                        None => match self.listed_size(&path, &content)? {
                            Some(size) => size,
                            None => {
                                let size = self.content_bytes(&content)?.len() as u64;
                                self.fetched(&path);
                                size
                            }
                        },
                    },
                };
                let modified = match content {
                    Content::Base { modified, .. } => modified,
                    _ => None,
                };
                Metadata { kind, size, version, modified }
            }
        })
    }

    /// [`Metadata::modified`] alone: the source's time for a file nobody wrote, found
    /// without reading the file even when the source left its size out.
    pub fn modified(&self, path: &str) -> Result<Option<SystemTime>> {
        Ok(match self.resolve(&VPath::parse(path)?)? {
            Resolved::File { content: Content::Base { modified, .. }, .. } => modified,
            _ => None,
        })
    }

    /// Read a file's bytes.
    pub fn read(&self, path: &str) -> Result<Bytes> {
        let path = VPath::parse(path)?;
        match self.resolve(&path)? {
            Resolved::Dir { .. } => Err(Error::IsADirectory(path.to_string())),
            Resolved::File { kind: Kind::Symlink, .. } => Err(is_a_symlink(&path)),
            Resolved::File { content, .. } => {
                let Content::Base { mount, key, .. } = &content else { return self.content_bytes(&content) };
                let fetching = self.reading(&path, mount, key)?;
                let bytes = self.content_bytes(&content);
                if fetching {
                    self.fetched(&path);
                }
                bytes
            }
        }
    }

    /// Read up to `len` bytes of a file starting at `offset`; fewer near the end of
    /// the file, none past it.
    ///
    /// A base file that has not been read whole is read from its source in that range
    /// and not cached, so a large file is never held in memory whole.
    pub fn read_range(&self, path: &str, offset: u64, len: u64) -> Result<Bytes> {
        let path = VPath::parse(path)?;
        match self.resolve(&path)? {
            Resolved::Dir { .. } => Err(Error::IsADirectory(path.to_string())),
            Resolved::File { kind: Kind::Symlink, .. } => Err(is_a_symlink(&path)),
            Resolved::File { content: Content::Bytes(bytes), .. } => Ok(slice(&bytes, offset, len)),
            Resolved::File { content: Content::Stored(stored), .. } => {
                stored.source.read_range(&stored.key, offset, len)
            }
            Resolved::File { content: Content::Base { mount, key, .. }, .. } => {
                match self.cache.peek_blob(&(mount.id, key.clone())) {
                    Some(bytes) => Ok(slice(&bytes, offset, len)),
                    None => {
                        self.reading(&path, &mount, &key)?;
                        let bytes = mount.source.read_range(&key, offset, len);
                        self.fetched(&path);
                        bytes
                    }
                }
            }
        }
    }

    /// The target of the symbolic link at `path`. The workspace never follows links:
    /// `read`, `write` and the rest act on the link itself or refuse it, and a mounted
    /// folder leaves resolving links to the system.
    pub fn read_link(&self, path: &str) -> Result<String> {
        let path = VPath::parse(path)?;
        match self.resolve(&path)? {
            Resolved::File { content, kind: Kind::Symlink, .. } => {
                let target = self.content_bytes(&content)?;
                String::from_utf8(target.to_vec()).map_err(|_| Error::NotText(path.to_string()))
            }
            _ => Err(Error::Unsupported(path.to_string(), "not a symlink")),
        }
    }

    /// Create a symbolic link at `path` to `target`, creating missing parent
    /// directories. The path must not exist yet. Returns the link's version.
    ///
    /// The target is stored as it is and never resolved by the workspace, so it may
    /// point anywhere, or nowhere.
    pub fn symlink(&mut self, path: &str, target: &str) -> Result<u64> {
        let path = VPath::parse(path)?;
        if target.is_empty() || target.contains('\0') {
            return Err(Error::InvalidPath(target.to_owned()));
        }
        match self.resolve(&path) {
            Err(Error::NotFound(_)) => {}
            Ok(_) => return Err(Error::AlreadyExists(path.to_string())),
            Err(err) => return Err(err),
        }
        let parent = path.parent().ok_or_else(|| Error::AlreadyExists(path.to_string()))?;
        self.ensure_dir(&parent)?;
        let content = Content::Bytes(Bytes::copy_from_slice(target.as_bytes()));
        let version = self.tick();
        self.overlay.insert(path, Overlay::File { content, kind: Kind::Symlink, version });
        Ok(version)
    }

    /// Give the file at `from` a second name, `to`, creating missing parent
    /// directories, like a hard link: a write through either name is seen through
    /// both, and removing one leaves the other. `to` must not exist yet.
    ///
    /// Copies and forks don't share the file: a copy is a new file, and each fork
    /// changes its own.
    pub fn hard_link(&mut self, from: &str, to: &str) -> Result<()> {
        let from = VPath::parse(from)?;
        let to = VPath::parse(to)?;
        let (content, kind, version) = match self.resolve(&from)? {
            Resolved::Dir { .. } => return Err(Error::IsADirectory(from.to_string())),
            Resolved::File { content, kind, version } => (content, kind, version),
        };
        match self.resolve(&to) {
            Err(Error::NotFound(_)) => {}
            Ok(_) => return Err(Error::AlreadyExists(to.to_string())),
            Err(err) => return Err(err),
        }
        let parent = to.parent().ok_or_else(|| Error::AlreadyExists(to.to_string()))?;
        self.ensure_dir(&parent)?;
        let id = match self.overlay.get(&from) {
            Some(Overlay::Linked { id }) => *id,
            _ => {
                let id = self.tick();
                let names = OrdSet::unit(from.clone());
                self.links.insert(id, Linked { content, kind, version, names });
                self.overlay.insert(from, Overlay::Linked { id });
                id
            }
        };
        self.link_name(id, to);
        Ok(())
    }

    /// Every path naming the file at `path`, itself included, sorted: more than one
    /// after a [`Workspace::hard_link`].
    pub fn hard_links(&self, path: &str) -> Result<Vec<VPath>> {
        let path = VPath::parse(path)?;
        if let Resolved::Dir { .. } = self.resolve(&path)? {
            return Err(Error::IsADirectory(path.to_string()));
        }
        Ok(match self.overlay.get(&path) {
            Some(Overlay::Linked { id }) => self.links.get(id).map(|linked| linked.names.iter().cloned().collect()),
            _ => None,
        }
        .unwrap_or_else(|| vec![path]))
    }

    /// Whether any file has several names, so callers that track files by path (a
    /// mount handing out inode numbers) know when [`Workspace::hard_links`] matters.
    pub fn has_hard_links(&self) -> bool {
        !self.links.is_empty()
    }

    /// Read a file as UTF-8 text.
    pub fn read_text(&self, path: &str) -> Result<String> {
        let bytes = self.read(path)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| Error::NotText(path.to_owned()))
    }

    /// The entries of a directory, sorted by name.
    pub fn list(&self, path: &str) -> Result<Vec<DirEntry>> {
        let path = VPath::parse(path)?;
        let base = match self.resolve(&path)? {
            Resolved::Dir { base } => base,
            Resolved::File { .. } => return Err(Error::NotADirectory(path.to_string())),
        };
        if let Some(base) = &base {
            self.note_listed(&path, base);
        }
        self.entries(&path, base.as_ref())
    }

    /// The entries of the directory at `path`, whose base node is `base`.
    fn entries(&self, path: &VPath, base: Option<&BaseNode>) -> Result<Vec<DirEntry>> {
        let mut entries: BTreeMap<String, DirEntry> = BTreeMap::new();
        if let Some(base) = base {
            for (name, entry) in self.base_listing(base)?.iter() {
                let kind = entry.kind;
                entries.insert(name.clone(), DirEntry { name: name.clone(), kind, size: entry.size });
            }
        }
        // Mount points and the directories leading to them are part of the structure.
        for mount_path in self.mounts.keys() {
            if let Some(rest) = mount_path.strip_prefix(path) {
                if let Some(first) = rest.first() {
                    let name = (*first).to_owned();
                    entries.insert(name.clone(), DirEntry { name, kind: Kind::Dir, size: None });
                }
            }
        }
        let depth = path.depth() + 1;
        for (child, entry) in self.overlay_descendants(path) {
            if child.depth() != depth {
                continue;
            }
            let name = child.name().unwrap_or_default().to_owned();
            match entry {
                Overlay::Deleted { .. } => {
                    entries.remove(&name);
                }
                Overlay::Dir { .. } => {
                    entries.insert(name.clone(), DirEntry { name, kind: Kind::Dir, size: None });
                }
                Overlay::File { .. } | Overlay::Linked { .. } => {
                    let (content, kind, _) = self.overlay_file(entry).expect("a file entry");
                    let size = content_size(&content);
                    entries.insert(name.clone(), DirEntry { name, kind, size });
                }
            }
        }
        Ok(entries.into_values().collect())
    }

    /// Write a file, creating missing parent directories. Returns the new version.
    pub fn write(&mut self, path: &str, data: impl Into<Bytes>) -> Result<u64> {
        self.write_checked(path, data, None)
    }

    /// Write a file only if its current version is `expected` (0 for a new file).
    pub fn write_versioned(&mut self, path: &str, data: impl Into<Bytes>, expected: u64) -> Result<u64> {
        self.write_checked(path, data, Some(expected))
    }

    /// Write a file whose bytes stay in `stored`'s source, read from there whenever the
    /// file is read and never copied into memory. Returns the new version. The source
    /// must keep answering for the key while this workspace, or a fork of it, has the
    /// file.
    pub fn write_stored(&mut self, path: &str, stored: Stored) -> Result<u64> {
        self.write_content(path, Content::Stored(stored), None)
    }

    /// Read the file at `path` from `stored` from now on instead of from memory, if it
    /// still has `version`, which it keeps: the caller vouches that `stored` holds
    /// exactly the file's bytes. Returns whether it did. This is how a file already
    /// saved elsewhere stops taking memory.
    pub fn store(&mut self, path: &str, version: u64, stored: Stored) -> Result<bool> {
        let path = VPath::parse(path)?;
        // Through a hard link, every name of the file reads from `stored`.
        match self.overlay.get(&path).and_then(|entry| self.overlay_file(entry)) {
            Some((_, kind, current)) if current == version => {
                self.put_file(path, Content::Stored(stored), kind, version);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// Where the file at `path` is stored outside memory, if it is: written with
    /// [`Workspace::write_stored`] or moved there with [`Workspace::store`], or a copy
    /// or move of such a file.
    pub fn stored(&self, path: &str) -> Result<Option<Stored>> {
        let path = VPath::parse(path)?;
        Ok(match self.resolve(&path)? {
            Resolved::File { content: Content::Stored(stored), .. } => Some(stored),
            _ => None,
        })
    }

    fn write_checked(&mut self, path: &str, data: impl Into<Bytes>, expected: Option<u64>) -> Result<u64> {
        self.write_content(path, Content::Bytes(data.into()), expected)
    }

    fn write_content(&mut self, path: &str, content: Content, expected: Option<u64>) -> Result<u64> {
        let path = VPath::parse(path)?;
        let parent = path.parent().ok_or_else(|| Error::IsADirectory(path.to_string()))?;
        let (kind, version, exists) = match self.resolve(&path) {
            Ok(Resolved::Dir { .. }) => return Err(Error::IsADirectory(path.to_string())),
            Ok(Resolved::File { kind: Kind::Symlink, .. }) => return Err(is_a_symlink(&path)),
            Ok(Resolved::File { kind, version, .. }) => (kind, version, true),
            Err(Error::NotFound(_)) => (Kind::File { executable: false }, self.missing_version(&path), false),
            Err(err) => return Err(err),
        };
        if let Some(expected) = expected {
            if expected != version {
                return Err(Error::VersionMismatch { path: path.to_string(), expected, actual: version });
            }
        }
        // An existing file's parent is a directory already, and so is a directory
        // written here (nothing above it can hide it, as in `resolve`).
        if !exists && !matches!(self.overlay.get(&parent), Some(Overlay::Dir { .. })) {
            self.ensure_dir(&parent)?;
        }
        let version = self.tick();
        self.put_file(path, content, kind, version);
        Ok(version)
    }

    /// Set or clear a file's executable bit. Returns the file's version, which changes
    /// only when the bit does.
    pub fn set_executable(&mut self, path: &str, executable: bool) -> Result<u64> {
        let path = VPath::parse(path)?;
        match self.resolve(&path)? {
            Resolved::Dir { .. } => Err(Error::IsADirectory(path.to_string())),
            Resolved::File { kind: Kind::Symlink, .. } => Err(is_a_symlink(&path)),
            Resolved::File { kind, version, .. } if kind == (Kind::File { executable }) => Ok(version),
            Resolved::File { content, .. } => {
                let version = self.tick();
                self.put_file(path, content, Kind::File { executable }, version);
                Ok(version)
            }
        }
    }

    /// Create a directory. With `parents`, missing ancestors are created and an
    /// existing directory is not an error, like `mkdir -p`.
    pub fn mkdir(&mut self, path: &str, parents: bool) -> Result<()> {
        let path = VPath::parse(path)?;
        match self.resolve(&path) {
            Ok(Resolved::Dir { .. }) if parents => return Ok(()),
            Ok(_) => return Err(Error::AlreadyExists(path.to_string())),
            Err(Error::NotFound(_)) => {}
            Err(err) => return Err(err),
        }
        let parent = path.parent().unwrap_or_else(VPath::root);
        if parents {
            self.ensure_dir(&parent)?;
        } else if !matches!(self.resolve(&parent)?, Resolved::Dir { .. }) {
            return Err(Error::NotADirectory(parent.to_string()));
        }
        self.insert_dir(path);
        Ok(())
    }

    /// Remove a file or (with `recursive`) a directory tree.
    pub fn remove(&mut self, path: &str, recursive: bool) -> Result<()> {
        let path = VPath::parse(path)?;
        self.check_removable(&path)?;
        if let Resolved::Dir { .. } = self.resolve(&path)? {
            if !recursive && !self.list(path.as_str())?.is_empty() {
                return Err(Error::DirectoryNotEmpty(path.to_string()));
            }
        }
        self.remove_unchecked(&path);
        Ok(())
    }

    /// Copy a file, or a directory tree with `recursive`. Base files are copied by
    /// reference, so nothing is read until the copy is.
    pub fn copy(&mut self, from: &str, to: &str, recursive: bool) -> Result<()> {
        let from = VPath::parse(from)?;
        let to = VPath::parse(to)?;
        let source = self.resolve(&from)?;
        if source_is_dir(&source) && !recursive {
            return Err(Error::IsADirectory(from.to_string()));
        }
        self.check_destination(&from, &to)?;
        self.atomically(|ws| ws.copy_tree(&from, &to, false))
    }

    /// Move or rename a file or directory tree. The destination must not exist.
    pub fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        let from_path = VPath::parse(from)?;
        let to_path = VPath::parse(to)?;
        self.check_removable(&from_path)?;
        self.resolve(&from_path)?;
        self.check_destination(&from_path, &to_path)?;
        self.atomically(|ws| {
            ws.copy_tree(&from_path, &to_path, true)?;
            ws.remove_unchecked(&from_path);
            Ok(())
        })
    }

    /// Files added, modified or deleted relative to the mounts, sorted by path.
    ///
    /// Only files and symbolic links are reported; an empty directory is not a change.
    /// A file written back with the bytes it had is not a change either, except one of
    /// [`COMPARE_LIMIT`] bytes or more, which counts as modified without being read.
    /// Each name of a file with several (hard links) is reported on its own, the way
    /// it differs from the base at that path. The cost is proportional to the overlay,
    /// plus listing any base directories that were deleted, plus reading written files
    /// under that size that kept their size.
    pub fn changes(&self) -> Result<Vec<Change>> {
        let original =
            Workspace { overlay: OrdMap::new(), links: OrdMap::new(), walks: Arc::default(), ..self.clone() };
        self.changes_since(&original)
    }

    /// Files added, modified or deleted relative to `base`, sorted by path: what
    /// changed since a checkpoint (a fork kept earlier), or how two forks of one
    /// checkpoint differ. Reported the way [`Workspace::changes`] reports them.
    ///
    /// Everything the two share since a fork is skipped without being looked at, so
    /// the cost follows what changed after they split, not the size of either.
    /// Workspaces that share no mounts are compared file by file, reading both. A
    /// written file of [`COMPARE_LIMIT`] bytes or more against a source file counts as
    /// modified without being read, as in `changes()`; two large source files, or two
    /// large written files, are read a piece at a time.
    pub fn changes_since(&self, base: &Workspace) -> Result<Vec<Change>> {
        // Paths whose entries differ, and whether everything below them may differ too.
        let mut touched: BTreeMap<VPath, bool> = BTreeMap::new();
        let mut note = |path: &VPath, below: bool| *touched.entry(path.clone()).or_default() |= below;
        for item in base.overlay.diff(&self.overlay) {
            let (path, entries) = match item {
                DiffItem::Add(path, new) => (path, [None, Some(new)]),
                DiffItem::Update { old: (path, old), new: (_, new) } => (path, [Some(old), Some(new)]),
                DiffItem::Remove(path, old) => (path, [Some(old), None]),
            };
            // A plain directory hides nothing below it; anything else may hide or
            // replace a whole directory.
            note(path, entries.into_iter().flatten().any(|entry| !matches!(entry, Overlay::Dir { opaque: false })));
        }
        // A write through one name of a linked file changes all of them.
        for item in base.links.diff(&self.links) {
            let linked = match item {
                DiffItem::Add(_, new) => [None, Some(new)],
                DiffItem::Update { old: (_, old), new: (_, new) } => [Some(old), Some(new)],
                DiffItem::Remove(_, old) => [Some(old), None],
            };
            for name in linked.into_iter().flatten().flat_map(|linked| linked.names.iter()) {
                note(name, true);
            }
        }
        for item in base.mounts.diff(&self.mounts) {
            let (DiffItem::Add(path, _) | DiffItem::Update { new: (path, _), .. } | DiffItem::Remove(path, _)) = item;
            note(path, true);
        }
        let mut changes = BTreeMap::new();
        let mut below_touched = BTreeSet::new();
        for (path, below) in &touched {
            let (old, new) = (base.lookup(path)?, self.lookup(path)?);
            if *below {
                for (ws, resolved) in [(base, &old), (self, &new)] {
                    if let Some(Resolved::Dir { base: node }) = resolved {
                        below_touched.extend(ws.files_below(path, node.clone())?);
                    }
                }
            }
            self.compare(base, path, file(old), file(new), &mut changes)?;
        }
        for path in below_touched.iter().filter(|path| !touched.contains_key(*path)) {
            self.compare(base, path, file(base.lookup(path)?), file(self.lookup(path)?), &mut changes)?;
        }
        Ok(changes.into_iter().map(|(path, kind)| Change { path, kind }).collect())
    }

    /// Record how the file at `path` changed from `old` in `base` to `new` here.
    fn compare(
        &self,
        base: &Workspace,
        path: &VPath,
        old: Option<(Content, Kind)>,
        new: Option<(Content, Kind)>,
        changes: &mut BTreeMap<VPath, ChangeKind>,
    ) -> Result<()> {
        let kind = match (old, new) {
            (None, None) => return Ok(()),
            (None, Some(_)) => ChangeKind::Added,
            (Some(_), None) => ChangeKind::Deleted,
            (Some(old), Some(new)) if self.same_file(&new, base, &old)? => return Ok(()),
            (Some(_), Some(_)) => ChangeKind::Modified,
        };
        changes.insert(path.clone(), kind);
        Ok(())
    }

    // ------------------------------------------------------------------
    // Resolution
    // ------------------------------------------------------------------

    fn resolve(&self, path: &VPath) -> Result<Resolved> {
        let entry = self.overlay.get(path);
        // Nothing is kept below a file or a whiteout: removing a directory removes
        // everything below it, and nothing can be added below a file. So an entry for
        // one of those has nothing above it to check, and neither has a directory that
        // hides the base anyway.
        match entry {
            Some(Overlay::Deleted { .. }) => return Err(Error::NotFound(path.to_string())),
            Some(entry @ (Overlay::File { .. } | Overlay::Linked { .. })) => {
                let (content, kind, version) = self.overlay_file(entry).expect("a file entry");
                return Ok(Resolved::File { content, kind, version });
            }
            Some(Overlay::Dir { opaque: true }) => return Ok(Resolved::Dir { base: self.structural_node(path) }),
            Some(Overlay::Dir { opaque: false }) | None => {}
        }
        let mut base_hidden = false;
        // An untouched workspace has nothing above any path to check.
        let ancestors = (!self.overlay.is_empty()).then(|| path.ancestor_strs()).into_iter().flatten();
        for ancestor in ancestors {
            match self.overlay.get(ancestor) {
                Some(Overlay::Deleted { .. }) => return Err(Error::NotFound(path.to_string())),
                Some(Overlay::File { .. } | Overlay::Linked { .. }) => {
                    return Err(Error::NotADirectory(ancestor.to_owned()));
                }
                Some(Overlay::Dir { opaque: true }) => base_hidden = true,
                Some(Overlay::Dir { opaque: false }) | None => {}
            }
        }
        let base = || -> Result<Option<BaseNode>> {
            if base_hidden { Ok(self.structural_node(path)) } else { self.base_node(path) }
        };
        // What is left is a directory written here, or nothing in the overlay.
        if entry.is_some() {
            return Ok(Resolved::Dir { base: base()?.filter(|node| node.kind().is_dir()) });
        }
        if path.is_root() {
            return Ok(Resolved::Dir { base: Some(BaseNode::Implicit) });
        }
        match base()? {
            None => Err(Error::NotFound(path.to_string())),
            Some(BaseNode::Entry(mount, entry)) if !entry.kind.is_dir() => Ok(Resolved::File {
                content: Content::Base { mount, key: entry.key.clone(), size: entry.size, modified: entry.modified },
                kind: entry.kind,
                version: 0,
            }),
            Some(node) => Ok(Resolved::Dir { base: Some(node) }),
        }
    }

    /// Mount points and the directories leading to them, which no overlay can hide.
    fn structural_node(&self, path: &VPath) -> Option<BaseNode> {
        if let Some(mount) = self.mounts.get(path) {
            return Some(BaseNode::MountRoot(mount.clone()));
        }
        self.mounts.keys().any(|mount| mount.starts_with(path)).then_some(BaseNode::Implicit)
    }

    /// The node at `path` in the mounts alone, ignoring the overlay.
    fn base_node(&self, path: &VPath) -> Result<Option<BaseNode>> {
        if let Some(mount) = self.mounts.get(path) {
            return Ok(Some(BaseNode::MountRoot(mount.clone())));
        }
        let implicit = self.structural_node(path);
        // The deepest mount above `path` owns it; a directory there keeps its base
        // entries even when another mount sits further down.
        let owner = self
            .mounts
            .iter()
            .filter(|(mount_path, _)| path.starts_with(mount_path))
            .max_by_key(|(mount_path, _)| mount_path.depth());
        let Some((mount_path, mount)) = owner else { return Ok(implicit) };
        match self.source_entry(mount, mount_path, path)? {
            // A file cannot sit where another mount needs a directory.
            Some(entry) if implicit.is_none() || entry.kind.is_dir() => Ok(Some(BaseNode::Entry(mount.clone(), entry))),
            _ => Ok(implicit),
        }
    }

    /// The source entry at `path`, which lies below `mount_path`: from the cache of
    /// paths already found, or by walking listings from the mount's root.
    fn source_entry(&self, mount: &Arc<Mount>, mount_path: &VPath, path: &VPath) -> Result<Option<SourceEntry>> {
        let rel = &path.as_str()[mount_path.as_str().len() + 1..];
        if let Some(entry) = self.cache.entry(mount.id, rel) {
            return Ok(Some(entry));
        }
        let mut parts = rel.split('/').peekable();
        let mut key = mount.root.clone();
        while let Some(part) = parts.next() {
            let listing = self.cached_listing(mount, &key)?;
            let Some(entry) = listing.get(part) else { return Ok(None) };
            if parts.peek().is_none() {
                self.cache.remember_entry(mount.id, rel, entry.clone());
                return Ok(Some(entry.clone()));
            }
            if !entry.kind.is_dir() {
                return Ok(None);
            }
            key = entry.key.clone();
        }
        Ok(None)
    }

    fn base_listing(&self, node: &BaseNode) -> Result<Arc<Listing>> {
        match node {
            BaseNode::MountRoot(mount) => self.cached_listing(mount, &mount.root),
            BaseNode::Entry(mount, entry) if entry.kind.is_dir() => self.cached_listing(mount, &entry.key),
            _ => Ok(Arc::new(Listing::new())),
        }
    }

    // ------------------------------------------------------------------
    // Prefetching: readahead for directories
    // ------------------------------------------------------------------

    /// Record that `path`, whose base node is `node`, is being listed. Listing a
    /// directory inside one already listed is a walk (`find`, `grep -r`, a copy).
    /// Nothing is prefetched yet: a walk that only lists (`find`, `ls -R`) needs no
    /// file, so the source is asked only once the walk reads files.
    fn note_listed(&self, path: &VPath, node: &BaseNode) {
        if dir_key(node).is_none() {
            return;
        }
        let mut walks = self.walks.lock().expect("walks lock");
        walks.listed.insert(path.clone());
        let Some(parent) = path.parent().filter(|parent| walks.listed.contains(parent)) else { return };
        let start = walks.starts.entry(parent.clone()).or_insert(parent).clone();
        walks.starts.insert(path.clone(), start);
    }

    /// Before a base file's bytes are fetched: a walk reading the files it lists
    /// (`grep -r`, a copy), or a program reading files by path one after another (a
    /// build), has the source prefetch, once it has read two files in one directory.
    /// A read after a pause, as a person's next look, prefetches nothing.
    /// Whether the bytes are to be fetched, rather than cached already.
    fn reading(&self, path: &VPath, mount: &Arc<Mount>, key: &SourceKey) -> Result<bool> {
        if self.cache.peek_blob(&(mount.id, key.clone())).is_some() {
            return Ok(false);
        }
        let Some(parent) = path.parent() else { return Ok(true) };
        let Some(dir) = self.base_dir(&parent, mount)? else { return Ok(true) };
        if self.touch(key, &parent, mount, &dir, true)? == Touch::Warmed {
            self.prefetch_dir(mount, &dir);
        }
        Ok(true)
    }

    /// After a base file at `path` was fetched: a pause is measured from here, not from
    /// when the fetch began, so files that each take longer than [`WALK_GAP`] to arrive
    /// still come one after another.
    fn fetched(&self, path: &VPath) {
        let Some(parent) = path.parent() else { return };
        let mut walks = self.walks.lock().expect("walks lock");
        let start = walks.starts.get(&parent).cloned();
        if let Some(reading) = walks.reading.get_mut(&start) {
            reading.last = Instant::now();
        }
    }

    /// Count the base file `file` of the directory `dir` at `path` as needing a fetch.
    /// One file says nothing about the rest; the second shows the directory is being
    /// gone through. Outside a walk, a read shows it only when it follows the last
    /// file needed without a pause.
    fn touch(&self, file: &SourceKey, path: &VPath, mount: &Arc<Mount>, dir: &SourceKey, read: bool) -> Result<Touch> {
        let start = self.walks.lock().expect("walks lock").starts.get(path).cloned();
        let walking = start.is_some();
        let steady = self.walk_reads(start, path, mount)?;
        // A read by path after a pause starts the directory's count again: it is a new
        // look, its file the first (even one fetched before, as ranged reads do), and the
        // next file read right after it the second.
        let fresh = read && !walking && !steady;
        let (count, first) = {
            let mut touched = self.cache.touched.lock().expect("cache lock");
            let files = touched.entry((mount.id, dir.clone())).or_default();
            if fresh {
                files.clear();
            }
            let first = files.insert(file.clone());
            (files.len(), first)
        };
        if fresh {
            return Ok(Touch::Cold);
        }
        Ok(match (count, first) {
            (0 | 1, _) => Touch::Cold,
            (2, true) => Touch::Warmed,
            _ => Touch::Warm,
        })
    }

    /// Record that the walk from `start` (`None`: a program reading by path) needed a
    /// file in the directory at `path`, and say whether it came without a pause after
    /// the last one. Once it has needed files in [`WALK_DIRS`] directories in a row
    /// without pausing, it is reading through them (`grep -r`, a copy, a build) rather
    /// than looking around, so everything below the directory holding the last three
    /// is prefetched, once. Following the latest ones, rather than all since the walk
    /// began, asks for the part of a large tree it is in now, ahead of the rest.
    fn walk_reads(&self, start: Option<VPath>, path: &VPath, mount: &Arc<Mount>) -> Result<bool> {
        let now = Instant::now();
        let (steady, below) = {
            let mut walks = self.walks.lock().expect("walks lock");
            let mut steady = true;
            let reading = walks.reading.entry(start).or_insert_with(|| {
                steady = false;
                Reading { recent: VecDeque::new(), last: now }
            });
            if now.duration_since(reading.last) > WALK_GAP {
                steady = false;
                reading.recent.clear();
            }
            reading.last = now;
            // A directory needed again becomes the latest.
            let again = reading.recent.iter().position(|dir| dir == path);
            if let Some(at) = again {
                reading.recent.remove(at);
            }
            reading.recent.push_back(path.clone());
            if again.is_some() {
                return Ok(steady);
            }
            if reading.recent.len() > WALK_DIRS {
                reading.recent.pop_front();
            }
            let below = (reading.recent.len() == WALK_DIRS)
                .then(|| reading.recent.iter().fold(path.clone(), |below, dir| common_ancestor(&below, dir)));
            (steady, below)
        };
        let Some(below) = below else { return Ok(steady) };
        if let Some(key) = self.base_dir(&below, mount)? {
            if self.cache.prefetched.lock().expect("cache lock").insert(((mount.id, key.clone()), true)) {
                mount.source.prefetch(&key, true);
            }
        }
        Ok(steady)
    }

    /// Ask the source to prefetch the files of the directory `dir`, once.
    /// It is asked even when everything below a parent was: a source may still be
    /// fetching a large tree, and this asks for these files first.
    fn prefetch_dir(&self, mount: &Arc<Mount>, dir: &SourceKey) {
        if self.cache.prefetched.lock().expect("cache lock").insert(((mount.id, dir.clone()), false)) {
            mount.source.prefetch(dir, false);
        }
    }

    /// The size of a base file already read.
    fn cached_size(&self, content: &Content) -> Option<u64> {
        let Content::Base { mount, key, .. } = content else { return None };
        Some(self.cache.peek_blob(&(mount.id, key.clone()))?.len() as u64)
    }

    /// The size of the base file at `path` when its directory's listing left it out.
    /// The first such file in a directory is read to find it, since one file says
    /// nothing about the rest. From the second on (`ls -l`), the source is asked to
    /// prefetch the directory and it is listed again, once, for every size at once.
    /// `None` when the size must come from reading the file.
    fn listed_size(&self, path: &VPath, content: &Content) -> Result<Option<u64>> {
        let Content::Base { mount, key, .. } = content else { return Ok(None) };
        let (Some(parent), Some(name)) = (path.parent(), path.name()) else { return Ok(None) };
        let Some(dir) = self.base_dir(&parent, mount)? else { return Ok(None) };
        if self.touch(key, &parent, mount, &dir, false)? == Touch::Cold {
            return Ok(None);
        }
        // Single-flight, like every other cache entry: forks asking at once prefetch once.
        let cache_key = (mount.id, dir.clone());
        let listing = Cache::get_or_load(&self.cache.sized, cache_key.clone(), || {
            self.prefetch_dir(mount, &dir);
            let listing = Arc::new(mount.source.read_dir(&dir)?);
            self.cache.replace_listing(cache_key.clone(), listing.clone());
            Ok(listing)
        })?;
        // A file copied or moved from another directory isn't in this listing.
        Ok(listing.get(name).filter(|entry| entry.key == *key).and_then(|entry| entry.size))
    }

    /// The source key of the base directory at `path`, if `mount` provides it.
    fn base_dir(&self, path: &VPath, mount: &Arc<Mount>) -> Result<Option<SourceKey>> {
        Ok(match self.resolve(path) {
            Ok(Resolved::Dir { base: Some(node) }) => {
                dir_key(&node).filter(|(owner, _)| owner.id == mount.id).map(|(_, key)| key)
            }
            Ok(_) | Err(Error::NotFound(_)) => None,
            Err(err) => return Err(err),
        })
    }

    fn cached_listing(&self, mount: &Arc<Mount>, key: &SourceKey) -> Result<Arc<Listing>> {
        Cache::get_or_load(&self.cache.dirs, (mount.id, key.clone()), || Ok(Arc::new(mount.source.read_dir(key)?)))
    }

    fn content_bytes(&self, content: &Content) -> Result<Bytes> {
        match content {
            Content::Bytes(bytes) => Ok(bytes.clone()),
            Content::Base { mount, key, .. } => {
                Cache::get_or_load(&self.cache.blobs, (mount.id, key.clone()), || mount.source.read_file(key))
            }
            Content::Stored(stored) => stored.source.read_file(&stored.key),
        }
    }

    /// True if a file here has the same kind and bytes as one in `other`. Content
    /// both share (a fork's, or the same source file) is not read.
    fn same_file(
        &self,
        (content, kind): &(Content, Kind),
        other: &Workspace,
        theirs: &(Content, Kind),
    ) -> Result<bool> {
        if *kind != theirs.1 {
            return Ok(false);
        }
        if *content == theirs.0 {
            return Ok(true);
        }
        let sizes = (content_size(content), content_size(&theirs.0));
        if let (Some(a), Some(b)) = sizes {
            if a != b {
                return Ok(false);
            }
        }
        if sizes.0.or(sizes.1).is_some_and(|size| size >= COMPARE_LIMIT) {
            // A written file this large counts as modified against the source file it
            // replaced without being read, as in `changes()`, since that comparison
            // would come at every call. Two source files, or two written files, are
            // compared piece by piece once both sizes are known.
            let in_source = |content: &Content| matches!(content, Content::Base { .. });
            if in_source(content) != in_source(&theirs.0) {
                return Ok(false);
            }
            if let (Some(size), Some(_)) = sizes {
                return same_bytes_in_pieces(content, &theirs.0, size);
            }
        }
        Ok(self.content_bytes(content)? == other.content_bytes(&theirs.0)?)
    }

    /// What `path` is, or `None` if nothing is there.
    fn lookup(&self, path: &VPath) -> Result<Option<Resolved>> {
        match self.resolve(path) {
            Ok(resolved) => Ok(Some(resolved)),
            Err(Error::NotFound(_) | Error::NotADirectory(_)) => Ok(None),
            Err(err) => Err(err),
        }
    }

    /// Every file and link below the directory at `path`, whose base node is `base`,
    /// listing directories but reading no files.
    fn files_below(&self, path: &VPath, base: Option<BaseNode>) -> Result<Vec<VPath>> {
        let mut files = Vec::new();
        let mut stack = vec![(path.clone(), base)];
        while let Some((dir, base)) = stack.pop() {
            for entry in self.entries(&dir, base.as_ref())? {
                let child = dir.join(&entry.name)?;
                if !entry.kind.is_dir() {
                    files.push(child);
                } else if let Ok(Resolved::Dir { base }) = self.resolve(&child) {
                    stack.push((child, base));
                }
            }
        }
        Ok(files)
    }

    fn overlay_descendants(&self, path: &VPath) -> impl Iterator<Item = (&VPath, &Overlay)> {
        self.overlay.range(path.descendants_start()..path.descendants_end())
    }

    // ------------------------------------------------------------------
    // Mutation helpers
    // ------------------------------------------------------------------

    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    /// The version a missing path reports: its whiteout's, or 0 if it never existed.
    fn missing_version(&self, path: &VPath) -> u64 {
        match self.overlay.get(path) {
            Some(Overlay::Deleted { version }) => *version,
            _ => 0,
        }
    }

    /// Run a multi-step change, restoring the overlay if any step fails. Saving the
    /// overlay is O(1) because it is a persistent map.
    fn atomically(&mut self, change: impl FnOnce(&mut Self) -> Result<()>) -> Result<()> {
        let saved = (self.overlay.clone(), self.links.clone());
        let result = change(self);
        if result.is_err() {
            (self.overlay, self.links) = saved;
        }
        result
    }

    /// The contents, kind and version of a file entry, following a hard link to the
    /// file it names.
    fn overlay_file(&self, entry: &Overlay) -> Option<(Content, Kind, u64)> {
        match entry {
            Overlay::File { content, kind, version } => Some((content.clone(), *kind, *version)),
            Overlay::Linked { id } => {
                let linked = self.links.get(id)?;
                Some((linked.content.clone(), linked.kind, linked.version))
            }
            Overlay::Dir { .. } | Overlay::Deleted { .. } => None,
        }
    }

    /// Put a file at `path`: through a hard link, into the file every name shares.
    fn put_file(&mut self, path: VPath, content: Content, kind: Kind, version: u64) {
        if let Some(Overlay::Linked { id }) = self.overlay.get(&path) {
            if let Some(linked) = self.links.get_mut(id) {
                (linked.content, linked.kind, linked.version) = (content, kind, version);
                return;
            }
        }
        self.overlay.insert(path, Overlay::File { content, kind, version });
    }

    /// Add `name` to the names of the linked file `id`.
    fn link_name(&mut self, id: u64, name: VPath) {
        if let Some(linked) = self.links.get_mut(&id) {
            linked.names.insert(name.clone());
        }
        self.overlay.insert(name, Overlay::Linked { id });
    }

    fn ensure_dir(&mut self, path: &VPath) -> Result<()> {
        // A directory that resolves has every ancestor already, which is the usual case.
        if let Ok(Resolved::Dir { .. }) = self.resolve(path) {
            return Ok(());
        }
        let mut chain = path.ancestors();
        chain.push(path.clone());
        for dir in chain {
            match self.resolve(&dir) {
                Ok(Resolved::Dir { .. }) => {}
                Ok(Resolved::File { .. }) => return Err(Error::NotADirectory(dir.to_string())),
                Err(Error::NotFound(_)) => self.insert_dir(dir),
                Err(err) => return Err(err),
            }
        }
        Ok(())
    }

    fn insert_dir(&mut self, path: VPath) {
        // Recreating a deleted directory must not bring its old base children back.
        let opaque = matches!(self.overlay.get(&path), Some(Overlay::Deleted { .. }));
        self.overlay.insert(path, Overlay::Dir { opaque });
    }

    fn check_removable(&self, path: &VPath) -> Result<()> {
        if path.is_root() {
            return Err(Error::Unsupported(path.to_string(), "cannot remove the root"));
        }
        if self.mounts.keys().any(|mount| mount.starts_with(path)) {
            return Err(Error::Unsupported(path.to_string(), "contains a mount point"));
        }
        Ok(())
    }

    fn check_destination(&self, from: &VPath, to: &VPath) -> Result<()> {
        if to.starts_with(from) {
            return Err(Error::Unsupported(to.to_string(), "cannot copy or move a directory into itself"));
        }
        match self.resolve(to) {
            Err(Error::NotFound(_)) => {}
            Ok(_) => return Err(Error::AlreadyExists(to.to_string())),
            Err(err) => return Err(err),
        }
        let parent = to.parent().ok_or_else(|| Error::AlreadyExists(to.to_string()))?;
        match self.resolve(&parent)? {
            Resolved::Dir { .. } => Ok(()),
            Resolved::File { .. } => Err(Error::NotADirectory(parent.to_string())),
        }
    }

    /// Copy `from` to `to`. A move keeps hard links: the new name joins the file's
    /// names (the old one leaves them when it is removed). A copy is a new file.
    fn copy_tree(&mut self, from: &VPath, to: &VPath, moving: bool) -> Result<()> {
        if let (true, Some(Overlay::Linked { id })) = (moving, self.overlay.get(from)) {
            let id = *id;
            self.link_name(id, to.clone());
            return Ok(());
        }
        match self.resolve(from)? {
            Resolved::File { content, kind, .. } => {
                let version = self.tick();
                self.overlay.insert(to.clone(), Overlay::File { content, kind, version });
            }
            Resolved::Dir { .. } => {
                self.insert_dir(to.clone());
                for entry in self.list(from.as_str())? {
                    self.copy_tree(&from.join(&entry.name)?, &to.join(&entry.name)?, moving)?;
                }
            }
        }
        Ok(())
    }

    fn remove_unchecked(&mut self, path: &VPath) {
        let doomed: Vec<VPath> = self.overlay_descendants(path).map(|(key, _)| key.clone()).collect();
        for key in doomed.iter().chain([path]) {
            // A file with other names lives on under them.
            if let Some(Overlay::Linked { id }) = self.overlay.remove(key) {
                if let Some(linked) = self.links.get_mut(&id) {
                    linked.names.remove(key);
                    // A file left with one name is an ordinary file again.
                    if linked.names.len() <= 1 {
                        let Some(Linked { content, kind, version, names }) = self.links.remove(&id) else { continue };
                        for name in names {
                            self.overlay.insert(name, Overlay::File { content: content.clone(), kind, version });
                        }
                    }
                }
            }
        }
        // A whiteout is only needed when the base still has something to hide.
        let in_base = !self.base_hidden(path) && matches!(self.base_node(path), Ok(Some(_)));
        if in_base {
            let version = self.tick();
            self.overlay.insert(path.clone(), Overlay::Deleted { version });
        } else {
            self.overlay.remove(path);
        }
    }

    fn base_hidden(&self, path: &VPath) -> bool {
        path.ancestor_strs().any(|ancestor| matches!(self.overlay.get(ancestor), Some(Overlay::Dir { opaque: true })))
    }
}

/// The error for reading, writing or changing the mode of a symbolic link, which the
/// workspace never follows.
fn is_a_symlink(path: &VPath) -> Error {
    Error::Unsupported(path.to_string(), "is a symbolic link, which the workspace does not follow")
}

fn source_is_dir(resolved: &Resolved) -> bool {
    matches!(resolved, Resolved::Dir { .. })
}

/// How many directories a walk must need files in before everything below them is
/// prefetched: one or two may be a look around (`cat src/main.rs`), three is a walk
/// reading through (`grep -r`).
const WALK_DIRS: usize = 3;

/// The longest pause within a walk reading through directories. A program walking a
/// tree needs its next file within one fetch; someone looking around pauses longer.
const WALK_GAP: Duration = Duration::from_secs(2);

/// What counting one more file needing a fetch in a directory found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Touch {
    /// The first: nothing to prefetch yet.
    Cold,
    /// The second, just now: the directory is being gone through.
    Warmed,
    /// Past the second.
    Warm,
}

/// The deepest directory both `a` and `b` are in (or are).
fn common_ancestor(a: &VPath, b: &VPath) -> VPath {
    let mut common = VPath::root();
    for path in a.ancestors().into_iter().chain([a.clone()]) {
        if !b.starts_with(&path) {
            break;
        }
        common = path;
    }
    common
}

/// The mount and source key of a base directory, if it has one.
fn dir_key(node: &BaseNode) -> Option<(Arc<Mount>, SourceKey)> {
    match node {
        BaseNode::MountRoot(mount) => Some((mount.clone(), mount.root.clone())),
        BaseNode::Entry(mount, entry) if entry.kind.is_dir() => Some((mount.clone(), entry.key.clone())),
        _ => None,
    }
}

/// The size from which [`Workspace::changes`] counts a written file as modified
/// without comparing it to the file it replaced.
pub const COMPARE_LIMIT: u64 = 64 << 20;

/// The file or link a lookup found, if that is what it found.
fn file(resolved: Option<Resolved>) -> Option<(Content, Kind)> {
    match resolved? {
        Resolved::File { content, kind, .. } => Some((content, kind)),
        Resolved::Dir { .. } => None,
    }
}

/// Whether two files of `size` bytes hold the same bytes, read a piece at a time with
/// ranged reads so neither is held in memory whole or cached.
fn same_bytes_in_pieces(a: &Content, b: &Content, size: u64) -> Result<bool> {
    const PIECE: u64 = 8 << 20;
    let range = |content: &Content, offset: u64| match content {
        Content::Base { mount, key, .. } => mount.source.read_range(key, offset, PIECE),
        Content::Bytes(bytes) => Ok(slice(bytes, offset, PIECE)),
        Content::Stored(stored) => stored.source.read_range(&stored.key, offset, PIECE),
    };
    let mut offset = 0;
    while offset < size {
        if range(a, offset)? != range(b, offset)? {
            return Ok(false);
        }
        offset += PIECE;
    }
    Ok(true)
}

fn content_size(content: &Content) -> Option<u64> {
    match content {
        Content::Bytes(bytes) => Some(bytes.len() as u64),
        Content::Base { size, .. } => *size,
        Content::Stored(stored) => Some(stored.size),
    }
}
