//! Copy-on-write workspaces over lazily read sources.
//!
//! A [`Workspace`] is a set of mounts (the read-only base) plus an overlay holding
//! everything written, created or deleted since. Both live in persistent maps, so
//! [`Workspace::fork`] is a cheap clone that shares all structure, and the overlay is
//! exactly the change set. Base content is read through a cache shared by every fork,
//! so a file fetched once is never fetched again.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use imbl::OrdMap;

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

impl fmt::Debug for Mount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Mount").field("id", &self.id).field("source", &self.source).finish()
    }
}

/// Listings and blobs fetched from sources; keys are immutable, so forks share it.
#[derive(Default)]
struct Cache {
    dirs: Mutex<HashMap<(u64, SourceKey), Arc<Listing>>>,
    blobs: Mutex<HashMap<(u64, SourceKey), Bytes>>,
}

/// File content: bytes written into the overlay, or a reference to a base file that
/// has not been (or need not be) read. References let copies and renames of base files
/// stay lazy.
#[derive(Clone, Debug)]
enum Content {
    Bytes(Bytes),
    Base { mount: Arc<Mount>, key: SourceKey, size: Option<u64> },
}

#[derive(Clone, Debug)]
enum Overlay {
    File {
        content: Content,
        kind: Kind,
        version: u64,
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
}

/// A copy-on-write filesystem view: read-only mounts plus an overlay of changes.
#[derive(Clone, Default)]
pub struct Workspace {
    mounts: OrdMap<VPath, Arc<Mount>>,
    overlay: OrdMap<VPath, Overlay>,
    cache: Arc<Cache>,
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
        self.clone()
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
            Resolved::Dir { .. } => Metadata { kind: Kind::Dir, size: 0, version: 0 },
            Resolved::File { content, kind, version } => {
                let size = match content_size(&content) {
                    Some(size) => size,
                    None => self.content_bytes(&content)?.len() as u64,
                };
                Metadata { kind, size, version }
            }
        })
    }

    /// Read a file's bytes.
    pub fn read(&self, path: &str) -> Result<Bytes> {
        let path = VPath::parse(path)?;
        match self.resolve(&path)? {
            Resolved::Dir { .. } => Err(Error::IsADirectory(path.to_string())),
            Resolved::File { content, .. } => self.content_bytes(&content),
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
            Resolved::File { content: Content::Bytes(bytes), .. } => Ok(slice(&bytes, offset, len)),
            Resolved::File { content: Content::Base { mount, key, .. }, .. } => {
                let cached = self.cache.blobs.lock().expect("cache lock").get(&(mount.id, key.clone())).cloned();
                match cached {
                    Some(bytes) => Ok(slice(&bytes, offset, len)),
                    None => mount.source.read_range(&key, offset, len),
                }
            }
        }
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
        let mut entries: BTreeMap<String, DirEntry> = BTreeMap::new();
        if let Some(base) = &base {
            for (name, entry) in self.base_listing(base)?.iter() {
                let kind = entry.kind;
                entries.insert(name.clone(), DirEntry { name: name.clone(), kind, size: entry.size });
            }
        }
        // Mount points and the directories leading to them are part of the structure.
        for mount_path in self.mounts.keys() {
            if let Some(rest) = mount_path.strip_prefix(&path) {
                if let Some(first) = rest.first() {
                    let name = (*first).to_owned();
                    entries.insert(name.clone(), DirEntry { name, kind: Kind::Dir, size: None });
                }
            }
        }
        let depth = path.depth() + 1;
        for (child, entry) in self.overlay_descendants(&path) {
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
                Overlay::File { content, kind, .. } => {
                    let size = content_size(content);
                    entries.insert(name.clone(), DirEntry { name, kind: *kind, size });
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

    fn write_checked(&mut self, path: &str, data: impl Into<Bytes>, expected: Option<u64>) -> Result<u64> {
        let path = VPath::parse(path)?;
        let parent = path.parent().ok_or_else(|| Error::IsADirectory(path.to_string()))?;
        let (kind, version) = match self.resolve(&path) {
            Ok(Resolved::Dir { .. }) => return Err(Error::IsADirectory(path.to_string())),
            Ok(Resolved::File { kind, version, .. }) => (kind, version),
            Err(Error::NotFound(_)) => (Kind::File { executable: false }, self.missing_version(&path)),
            Err(err) => return Err(err),
        };
        if let Some(expected) = expected {
            if expected != version {
                return Err(Error::VersionMismatch { path: path.to_string(), expected, actual: version });
            }
        }
        self.ensure_dir(&parent)?;
        let content = Content::Bytes(data.into());
        let version = self.tick();
        self.overlay.insert(path, Overlay::File { content, kind, version });
        Ok(version)
    }

    /// Set or clear a file's executable bit. Returns the file's version, which changes
    /// only when the bit does.
    pub fn set_executable(&mut self, path: &str, executable: bool) -> Result<u64> {
        let path = VPath::parse(path)?;
        match self.resolve(&path)? {
            Resolved::Dir { .. } => Err(Error::IsADirectory(path.to_string())),
            Resolved::File { kind, version, .. } if kind == (Kind::File { executable }) => Ok(version),
            Resolved::File { content, .. } => {
                let version = self.tick();
                self.overlay.insert(path, Overlay::File { content, kind: Kind::File { executable }, version });
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
        self.atomically(|ws| ws.copy_tree(&from, &to))
    }

    /// Move or rename a file or directory tree. The destination must not exist.
    pub fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        let from_path = VPath::parse(from)?;
        let to_path = VPath::parse(to)?;
        self.check_removable(&from_path)?;
        self.resolve(&from_path)?;
        self.check_destination(&from_path, &to_path)?;
        self.atomically(|ws| {
            ws.copy_tree(&from_path, &to_path)?;
            ws.remove_unchecked(&from_path);
            Ok(())
        })
    }

    /// Files added, modified or deleted relative to the mounts, sorted by path.
    ///
    /// Only files are reported; an empty directory is not a change. The cost is
    /// proportional to the overlay, plus listing any base directories that were deleted.
    pub fn changes(&self) -> Result<Vec<Change>> {
        let mut changes: BTreeMap<VPath, ChangeKind> = BTreeMap::new();
        // First every base file the overlay hides: below whiteouts, recreated
        // directories, and files written where a base directory used to be.
        for (path, entry) in self.overlay.iter() {
            let hides_base = match entry {
                Overlay::Deleted { .. } | Overlay::Dir { opaque: true } => true,
                Overlay::File { .. } => self.base_node(path)?.is_some_and(|node| node.kind().is_dir()),
                Overlay::Dir { opaque: false } => false,
            };
            if hides_base {
                for file in self.base_files_under(path)? {
                    changes.insert(file, ChangeKind::Deleted);
                }
            }
        }
        for (path, entry) in self.overlay.iter() {
            let Overlay::File { content, kind, .. } = entry else { continue };
            match self.base_node(path)? {
                Some(BaseNode::Entry(mount, base)) if !base.kind.is_dir() => {
                    if self.same_as_base(content, *kind, &mount, &base)? {
                        changes.remove(path);
                    } else {
                        changes.insert(path.clone(), ChangeKind::Modified);
                    }
                }
                _ => {
                    changes.insert(path.clone(), ChangeKind::Added);
                }
            }
        }
        Ok(changes.into_iter().map(|(path, kind)| Change { path, kind }).collect())
    }

    // ------------------------------------------------------------------
    // Resolution
    // ------------------------------------------------------------------

    fn resolve(&self, path: &VPath) -> Result<Resolved> {
        let mut base_hidden = false;
        for ancestor in path.ancestors() {
            match self.overlay.get(&ancestor) {
                Some(Overlay::Deleted { .. }) => return Err(Error::NotFound(path.to_string())),
                Some(Overlay::File { .. }) => return Err(Error::NotADirectory(ancestor.to_string())),
                Some(Overlay::Dir { opaque: true }) => base_hidden = true,
                Some(Overlay::Dir { opaque: false }) | None => {}
            }
        }
        let base = || -> Result<Option<BaseNode>> {
            if base_hidden { Ok(self.structural_node(path)) } else { self.base_node(path) }
        };
        match self.overlay.get(path) {
            Some(Overlay::Deleted { .. }) => Err(Error::NotFound(path.to_string())),
            Some(Overlay::File { content, kind, version }) => {
                Ok(Resolved::File { content: content.clone(), kind: *kind, version: *version })
            }
            Some(Overlay::Dir { opaque: true }) => Ok(Resolved::Dir { base: self.structural_node(path) }),
            Some(Overlay::Dir { opaque: false }) => {
                Ok(Resolved::Dir { base: base()?.filter(|node| node.kind().is_dir()) })
            }
            None if path.is_root() => Ok(Resolved::Dir { base: Some(BaseNode::Implicit) }),
            None => match base()? {
                None => Err(Error::NotFound(path.to_string())),
                Some(node) => match node {
                    BaseNode::Entry(mount, entry) if !entry.kind.is_dir() => Ok(Resolved::File {
                        content: Content::Base { mount, key: entry.key.clone(), size: entry.size },
                        kind: entry.kind,
                        version: 0,
                    }),
                    node => Ok(Resolved::Dir { base: Some(node) }),
                },
            },
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
        let rest = path.strip_prefix(mount_path).unwrap_or_default();
        let mut key = mount.root.clone();
        for (idx, part) in rest.iter().enumerate() {
            let listing = self.cached_listing(mount, &key)?;
            let Some(entry) = listing.get(*part) else { return Ok(implicit) };
            if idx + 1 == rest.len() {
                if implicit.is_some() && !entry.kind.is_dir() {
                    return Ok(implicit);
                }
                return Ok(Some(BaseNode::Entry(mount.clone(), entry.clone())));
            }
            if !entry.kind.is_dir() {
                return Ok(implicit);
            }
            key = entry.key.clone();
        }
        Ok(implicit)
    }

    fn base_listing(&self, node: &BaseNode) -> Result<Arc<Listing>> {
        match node {
            BaseNode::MountRoot(mount) => self.cached_listing(mount, &mount.root),
            BaseNode::Entry(mount, entry) if entry.kind.is_dir() => self.cached_listing(mount, &entry.key),
            _ => Ok(Arc::new(Listing::new())),
        }
    }

    fn cached_listing(&self, mount: &Arc<Mount>, key: &SourceKey) -> Result<Arc<Listing>> {
        let cache_key = (mount.id, key.clone());
        if let Some(listing) = self.cache.dirs.lock().expect("cache lock").get(&cache_key) {
            return Ok(listing.clone());
        }
        let listing = Arc::new(mount.source.read_dir(key)?);
        self.cache.dirs.lock().expect("cache lock").insert(cache_key, listing.clone());
        Ok(listing)
    }

    fn content_bytes(&self, content: &Content) -> Result<Bytes> {
        match content {
            Content::Bytes(bytes) => Ok(bytes.clone()),
            Content::Base { mount, key, .. } => {
                let cache_key = (mount.id, key.clone());
                if let Some(bytes) = self.cache.blobs.lock().expect("cache lock").get(&cache_key) {
                    return Ok(bytes.clone());
                }
                let bytes = mount.source.read_file(key)?;
                self.cache.blobs.lock().expect("cache lock").insert(cache_key, bytes.clone());
                Ok(bytes)
            }
        }
    }

    fn same_as_base(&self, content: &Content, kind: Kind, mount: &Arc<Mount>, base: &SourceEntry) -> Result<bool> {
        if kind != base.kind {
            return Ok(false);
        }
        if let Content::Base { mount: from, key, .. } = content {
            if from.id == mount.id && *key == base.key {
                return Ok(true);
            }
        }
        let original = Content::Base { mount: mount.clone(), key: base.key.clone(), size: base.size };
        if let (Some(a), Some(b)) = (content_size(content), content_size(&original)) {
            if a != b {
                return Ok(false);
            }
        }
        Ok(self.content_bytes(content)? == self.content_bytes(&original)?)
    }

    /// Every base file at or below `path`, listing directories but reading no files.
    fn base_files_under(&self, path: &VPath) -> Result<Vec<VPath>> {
        let mut files = Vec::new();
        let mut stack = vec![(path.clone(), self.base_node(path)?)];
        while let Some((current, node)) = stack.pop() {
            match node {
                None => {}
                Some(BaseNode::Entry(_, ref entry)) if !entry.kind.is_dir() => files.push(current),
                Some(node) => {
                    for (name, entry) in self.base_listing(&node)?.iter() {
                        let child = current.join(name)?;
                        stack.push((child, Some(self.child_node(&node, entry))));
                    }
                }
            }
        }
        Ok(files)
    }

    fn child_node(&self, parent: &BaseNode, entry: &SourceEntry) -> BaseNode {
        match parent {
            BaseNode::MountRoot(mount) | BaseNode::Entry(mount, _) => BaseNode::Entry(mount.clone(), entry.clone()),
            BaseNode::Implicit => BaseNode::Implicit,
        }
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
        let saved = self.overlay.clone();
        let result = change(self);
        if result.is_err() {
            self.overlay = saved;
        }
        result
    }

    fn ensure_dir(&mut self, path: &VPath) -> Result<()> {
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

    fn copy_tree(&mut self, from: &VPath, to: &VPath) -> Result<()> {
        match self.resolve(from)? {
            Resolved::File { content, kind, .. } => {
                let version = self.tick();
                self.overlay.insert(to.clone(), Overlay::File { content, kind, version });
            }
            Resolved::Dir { .. } => {
                self.insert_dir(to.clone());
                for entry in self.list(from.as_str())? {
                    self.copy_tree(&from.join(&entry.name)?, &to.join(&entry.name)?)?;
                }
            }
        }
        Ok(())
    }

    fn remove_unchecked(&mut self, path: &VPath) {
        let doomed: Vec<VPath> = self.overlay_descendants(path).map(|(key, _)| key.clone()).collect();
        for key in doomed {
            self.overlay.remove(&key);
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
        path.ancestors()
            .iter()
            .any(|ancestor| matches!(self.overlay.get(ancestor), Some(Overlay::Dir { opaque: true })))
    }
}

fn source_is_dir(resolved: &Resolved) -> bool {
    matches!(resolved, Resolved::Dir { .. })
}

fn content_size(content: &Content) -> Option<u64> {
    match content {
        Content::Bytes(bytes) => Some(bytes.len() as u64),
        Content::Base { size, .. } => *size,
    }
}
