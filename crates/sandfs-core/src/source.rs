//! Read-only sources that back a workspace's base layer.
//!
//! A source exposes an immutable tree through opaque keys: a directory source uses
//! relative paths, while an API or object store can use whatever ids it already has. The workspace asks for a directory
//! listing the first time something under it is touched and for file bytes the first
//! time a file is read, and caches both, so a source only pays for what is used.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use bytes::Bytes;

use crate::error::{Error, Result};

/// Opaque identifier a source uses for one of its trees or files.
pub type SourceKey = Arc<str>;

/// What a directory entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    /// A directory.
    Dir,
    /// A file: any bytes.
    File {
        /// Whether the executable bit is set, reported as the mode on a FUSE mount.
        executable: bool,
    },
    /// A symbolic link. Its target is its contents: what [`Source::read_file`] returns
    /// for it. sandfs stores links but never follows them; in a mounted folder the
    /// system resolves them, the same as on any other filesystem.
    Symlink,
}

impl Kind {
    /// True for directories.
    pub fn is_dir(self) -> bool {
        matches!(self, Kind::Dir)
    }

    /// True for symbolic links.
    pub fn is_symlink(self) -> bool {
        matches!(self, Kind::Symlink)
    }
}

/// One entry in a source directory listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceEntry {
    /// What the entry is.
    pub kind: Kind,
    /// Key to pass back to [`Source::read_dir`] or [`Source::read_file`].
    pub key: SourceKey,
    /// Size in bytes when the source knows it without reading the file. When it is
    /// `None`, [`crate::Workspace::stat`] reads the file once to find it.
    pub size: Option<u64>,
    /// When the file's bytes (or a link's target) last changed, if the source knows. Programs that skip
    /// work for unchanged files (build and test caches, `make`) compare it, so it
    /// must stay the same for as long as the file does.
    pub modified: Option<SystemTime>,
}

/// A directory listing, sorted by name.
pub type Listing = BTreeMap<String, SourceEntry>;

/// An immutable, lazily read tree.
///
/// Implementations must return the same answer for the same key every time; the
/// workspace caches results and shares them between forks. The one exception is a
/// listing's sizes: after a [`Source::prefetch`], a listing may report sizes it left
/// out before. A source whose files can change underneath it (a folder on disk) can
/// also say what a file is now, through [`Source::entry_now`].
pub trait Source: Send + Sync + fmt::Debug {
    /// Key of the root directory.
    fn root(&self) -> Result<SourceKey>;
    /// List the directory with this key.
    fn read_dir(&self, key: &str) -> Result<Listing>;
    /// Read the file with this key; for a symbolic link, its target.
    fn read_file(&self, key: &str) -> Result<Bytes>;

    /// Read up to `len` bytes of the file with this key, starting at `offset`. Fewer
    /// bytes come back near the end of the file, and none past it.
    ///
    /// The default reads the whole file and slices it. A source that can read part of
    /// a file (a disk, an object store with range requests) should override it, so a
    /// large file is never held in memory whole.
    fn read_range(&self, key: &str, offset: u64, len: u64) -> Result<Bytes> {
        Ok(slice(&self.read_file(key)?, offset, len))
    }

    /// A hint that the files in the directory with this key are about to be read: its
    /// own files, or with `recursive` everything below it. It is readahead for
    /// directories. A source where each request is a slow trip (a server, an object
    /// store) can fetch them in one trip instead of one per file.
    ///
    /// The workspace calls it without `recursive` once a second file in a directory is
    /// needed: a second size the listing left out (it then lists the directory once
    /// more, so a source can report the sizes it now has), or a second file read by a
    /// walk (a directory listed inside one already listed) or right after the last
    /// file read (a program opening files by path, such as a build). Once one
    /// workspace's walk or program has needed files in three directories in a row
    /// without pausing, it calls it with `recursive` for the directory holding those
    /// three, so a long walk asks for each part of a tree as it reaches it. Each
    /// directory is asked once, even when a parent was asked recursively, since a source
    /// may still be fetching the parent. A lookup, a single read (even in pieces), reads
    /// with pauses between them and a walk that only lists never call it.
    ///
    /// Files the source has at hand ([`Source::is_local`]) never count: reading them is
    /// no sign that the rest are about to be read.
    ///
    /// A prefetch must not change any answer the source gives, only how quickly it
    /// arrives, except that a listing may now include sizes. A source may ignore it, and
    /// the default does. It may be called from any thread, concurrently with reads,
    /// and should bound how much it fetches: a directory may hold far more than will be
    /// read.
    fn prefetch(&self, key: &str, recursive: bool) {
        let _ = (key, recursive);
    }

    /// Whether the file with this key is at hand, so reading it needs no slow trip: a
    /// source that keeps what it fetched (a partial clone's local files) says so for
    /// those. Such reads never lead to a [`Source::prefetch`], so a program reading only
    /// files already fetched, as `git status` reads every `.gitignore`, fetches nothing
    /// more. The default, `false`, counts every read.
    fn is_local(&self, key: &str) -> bool {
        let _ = key;
        false
    }

    /// The file at `key` as the source has it now, for a source whose files can change
    /// while a workspace is over it (a folder on disk). The workspace asks only once a
    /// read shows a file is no longer what its listing said (see
    /// [`crate::Workspace::recheck`]). `None`, the default, keeps the answer the
    /// workspace has, as a source whose answers never change should; so does a source
    /// that no longer has the file.
    fn entry_now(&self, key: &str) -> Result<Option<SourceEntry>> {
        let _ = key;
        Ok(None)
    }

    /// Let go of what the source keeps only to answer faster, such as helper processes
    /// sitting idle, when a workspace over it is trimmed ([`crate::Workspace::trim`]).
    /// Every answer stays the same. The default keeps nothing, so does nothing.
    fn trim(&self) {}
}

/// The part of `bytes` a ranged read asks for, clamped to its end.
pub(crate) fn slice(bytes: &Bytes, offset: u64, len: u64) -> Bytes {
    let start = usize::try_from(offset).unwrap_or(usize::MAX).min(bytes.len());
    let end = start.saturating_add(usize::try_from(len).unwrap_or(usize::MAX)).min(bytes.len());
    bytes.slice(start..end)
}

/// An in-memory source, mostly for tests and for seeding workspaces from data.
#[derive(Debug, Default)]
pub struct MemorySource {
    dirs: HashMap<String, Listing>,
    files: HashMap<String, Bytes>,
}

impl MemorySource {
    /// Build a source from `(relative path, contents)` pairs; parent directories are implied.
    pub fn from_files<I, P, B>(files: I) -> Result<Self>
    where
        I: IntoIterator<Item = (P, B)>,
        P: AsRef<str>,
        B: Into<Bytes>,
    {
        let mut source = MemorySource::default();
        source.dirs.insert(String::new(), Listing::new());
        for (path, data) in files {
            source.insert(path.as_ref(), data.into(), false)?;
        }
        Ok(source)
    }

    /// Add a file at a relative path; parent directories are implied.
    pub fn insert(&mut self, path: &str, data: Bytes, executable: bool) -> Result<()> {
        let kind = Kind::File { executable };
        let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
        let Some((name, dirs)) = parts.split_last() else {
            return Err(Error::InvalidPath(path.to_owned()));
        };
        let mut dir_key = String::new();
        self.dirs.entry(String::new()).or_default();
        for part in dirs {
            let child_key = if dir_key.is_empty() { (*part).to_owned() } else { format!("{dir_key}/{part}") };
            let listing = self.dirs.entry(dir_key.clone()).or_default();
            let entry = listing.entry((*part).to_owned()).or_insert_with(|| SourceEntry {
                kind: Kind::Dir,
                key: child_key.as_str().into(),
                size: None,
                modified: None,
            });
            if !entry.kind.is_dir() {
                return Err(Error::NotADirectory(child_key));
            }
            self.dirs.entry(child_key.clone()).or_default();
            dir_key = child_key;
        }
        let file_key = if dir_key.is_empty() { (*name).to_owned() } else { format!("{dir_key}/{name}") };
        let size = Some(data.len() as u64);
        if self.dirs.contains_key(&file_key) {
            return Err(Error::IsADirectory(file_key));
        }
        self.dirs
            .entry(dir_key)
            .or_default()
            .insert((*name).to_owned(), SourceEntry { kind, key: file_key.as_str().into(), size, modified: None });
        self.files.insert(file_key, data);
        Ok(())
    }
}

impl Source for MemorySource {
    fn root(&self) -> Result<SourceKey> {
        Ok("".into())
    }

    fn read_dir(&self, key: &str) -> Result<Listing> {
        self.dirs.get(key).cloned().ok_or_else(|| Error::Source(format!("no directory {key:?}")))
    }

    fn read_file(&self, key: &str) -> Result<Bytes> {
        self.files.get(key).cloned().ok_or_else(|| Error::Source(format!("no file {key:?}")))
    }
}

/// A read-only view of a host directory, read lazily. It is not a snapshot: each path is
/// read when first touched, and ranged reads go back to the directory every time, so a
/// directory that changes while mounted can show a mix of old and new contents.
///
/// Symlinks are listed as links, with their targets, and never followed, so a link
/// cannot lead the workspace outside `root`. In a mounted folder a link reaches what it
/// reaches on the host, except that one into `root` reaches the workspace's copy. Special files (sockets, devices, fifos) and links
/// whose targets aren't UTF-8 are skipped. Each read also checks that the resolved host
/// path is still inside `root`, in case a directory was swapped for a symlink after it
/// was listed. That check is not atomic with the read, so treat the directory as
/// trusted while it is mounted.
#[derive(Debug)]
pub struct DirSource {
    root: PathBuf,
}

impl DirSource {
    /// Expose the directory at `root`.
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        if !root.is_dir() {
            return Err(Error::Source(format!("{} is not a directory", root.display())));
        }
        Ok(DirSource { root: root.canonicalize().map_err(|err| io_error(root, err))? })
    }

    /// Fail unless `dir`, with every symlink resolved, is still `root` or inside it.
    fn check_inside(&self, dir: &Path) -> Result<()> {
        if dir.canonicalize().map_err(|err| io_error(dir, err))?.starts_with(&self.root) {
            Ok(())
        } else {
            Err(Error::Source(format!("{} is outside the mounted directory", dir.display())))
        }
    }

    /// Fail unless the directory at `key` is `dir`, inside `root`, with no symlink on
    /// the way.
    fn check_dir(&self, key: &str, dir: &Path) -> Result<()> {
        // As in `open_file`: on Linux the kernel names the directory actually opened,
        // which costs one call where resolving `dir` costs one per component.
        #[cfg(target_os = "linux")]
        if Path::new(key).components().all(|part| matches!(part, Component::Normal(_))) {
            use std::os::fd::AsRawFd;
            use std::os::unix::fs::OpenOptionsExt;
            // A directory-only open fails at once on anything else, such as a fifo
            // swapped in since the listing, which a plain open would block on.
            let opened = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY)
                .open(dir)
                .map_err(|err| io_error(dir, err))?;
            if let Ok(path) = std::fs::read_link(format!("/proc/self/fd/{}", opened.as_raw_fd())) {
                return if path == dir {
                    Ok(())
                } else {
                    Err(Error::Source(format!("{} is outside the mounted directory", dir.display())))
                };
            }
        }
        let _ = key;
        self.check_inside(dir)
    }

    /// The target of the link at `key`, if it is UTF-8 like every workspace path.
    ///
    /// The target keeps the meaning it has on the host, wherever the workspace is
    /// mounted. A relative one that stays inside the folder is kept as written: in a
    /// mount it stays in the mount, through the workspace's own links. Any other one
    /// that lands inside is made relative to the link, so a mount reaches the
    /// workspace's copy of the file instead of the folder itself, which a write must
    /// never reach. A relative one that lands outside becomes the absolute path it
    /// reaches on the host; an absolute one outside is kept as written.
    fn link_target(&self, key: &str) -> Result<Bytes> {
        let path = self.host_path(key);
        let target = std::fs::read_link(&path).map_err(|err| io_error(&path, err))?;
        let parent = Path::new(key).parent().unwrap_or(Path::new(""));
        let rewritten = match self.named_inside(parent, &target) {
            Some(_) if !target.has_root() => target.clone(),
            Some(inside) => relative_link(parent, &inside),
            None => {
                let reached = resolve(&path.parent().unwrap_or(&self.root).join(&target));
                // A `..` left over follows a folder that isn't there, so the link dangles
                // on the host. Made relative, it would come alive once the folder is made
                // in the workspace, and could then climb out of the mount; it stays a
                // host path instead.
                let inside = reached
                    .strip_prefix(&self.root)
                    .ok()
                    .filter(|inside| inside.components().all(|part| part != Component::ParentDir));
                match inside {
                    Some(inside) => relative_link(parent, inside),
                    // Rooted, even without a drive on Windows: the same path from anywhere.
                    None if target.has_root() => target.clone(),
                    None => reached,
                }
            }
        };
        // A trailing separator, or one and `.`, means the target must be a folder; the
        // rewriting drops it.
        let raw = target.as_os_str().as_encoded_bytes();
        let separator = |byte: &u8| std::path::is_separator(*byte as char);
        let folder_only = match raw {
            [.., last] if separator(last) => true,
            [.., before, b'.'] => separator(before),
            _ => false,
        };
        match rewritten.into_os_string().into_string() {
            Ok(rewritten) if folder_only && !rewritten.ends_with(std::path::is_separator) && rewritten != target => {
                Ok(Bytes::from(rewritten + std::path::MAIN_SEPARATOR_STR))
            }
            Ok(rewritten) => Ok(Bytes::from(rewritten)),
            Err(_) => Err(Error::NotText(path.display().to_string())),
        }
    }

    /// The place in the folder that `target`, written in a link in the folder `parent`,
    /// names without leaving the folder on the way, if it does. An absolute target with
    /// `..` in it is left to the OS, since a link before the `..` could lead anywhere,
    /// and so is any target that passes through a link out of the folder, which could
    /// lead back into it.
    fn named_inside(&self, parent: &Path, target: &Path) -> Option<PathBuf> {
        let (mut inside, rest): (Vec<Component>, &Path) = if target.has_root() {
            let rest = target.strip_prefix(&self.root).ok()?;
            if rest.components().any(|part| part == Component::ParentDir) {
                return None;
            }
            (Vec::new(), rest)
        } else {
            (parent.components().collect(), target)
        };
        let mut host = self.root.join(inside.iter().collect::<PathBuf>());
        let mut exists = true;
        for part in rest.components() {
            match part {
                Component::ParentDir => {
                    inside.pop()?;
                }
                Component::Normal(_) => inside.push(part),
                Component::CurDir => continue,
                Component::RootDir | Component::Prefix(_) => return None,
            }
            // Each step that exists must still be in the folder once its links are
            // followed; past a missing one, nothing does.
            host.push(part);
            if exists {
                match host.canonicalize() {
                    Ok(real) if !real.starts_with(&self.root) => return None,
                    Ok(_) => {}
                    Err(_) => exists = false,
                }
            }
        }
        Some(inside.into_iter().collect())
    }

    /// The entry for the file at `key`, of type `file_type`, with `meta` its metadata
    /// (its own for a link, not its target's). `None` for what a workspace doesn't
    /// hold: special files, and links whose target no workspace path could hold.
    fn entry_of(
        &self,
        key: String,
        file_type: std::fs::FileType,
        meta: impl FnOnce() -> std::io::Result<std::fs::Metadata>,
    ) -> Result<Option<SourceEntry>> {
        let (kind, size, modified) = if file_type.is_dir() {
            (Kind::Dir, None, None)
        } else if file_type.is_file() {
            let meta = meta().map_err(|err| io_error(&self.host_path(&key), err))?;
            (Kind::File { executable: is_executable(&meta) }, Some(meta.len()), meta.modified().ok())
        } else if file_type.is_symlink() {
            let modified = meta().ok().and_then(|meta| meta.modified().ok());
            match self.link_target(&key) {
                Ok(target) => (Kind::Symlink, Some(target.len() as u64), modified),
                Err(Error::NotText(_)) => return Ok(None),
                Err(err) => return Err(err),
            }
        } else {
            return Ok(None);
        };
        Ok(Some(SourceEntry { kind, key: key.as_str().into(), size, modified }))
    }

    fn host_path(&self, key: &str) -> PathBuf {
        if key.is_empty() { self.root.clone() } else { self.root.join(key) }
    }

    /// Open the regular file at `key` inside `root`, or read the target of the link there.
    fn open_file(&self, key: &str) -> Result<Opened> {
        // Keys from listings are plain relative names; an absolute key or `..` would
        // name a file outside the folder that the check below would accept as-is.
        if !Path::new(key).components().all(|part| matches!(part, Component::Normal(_))) {
            return Err(Error::Source(format!("{key} is outside the mounted directory")));
        }
        let path = self.host_path(key);
        // Checked before opening, so a special file (a fifo) is never opened, and a
        // link is never followed: its target is read from the folder holding it.
        let file_type = path.symlink_metadata().map_err(|err| io_error(&path, err))?.file_type();
        if file_type.is_symlink() {
            let parent = Path::new(key).parent().and_then(Path::to_str).unwrap_or_default();
            self.check_dir(parent, path.parent().unwrap_or(&self.root))?;
            return Ok(Opened::Link(self.link_target(key)?));
        }
        if !file_type.is_file() {
            return Err(Error::Source(format!("{} is not a regular file", path.display())));
        }
        let file = std::fs::File::open(&path).map_err(|err| io_error(&path, err))?;
        // On Linux the kernel names the file that was actually opened. It must be this
        // very path: a symlink anywhere on the way would resolve to another one. That
        // is one call, where resolving the parent costs one per component.
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            if let Ok(opened) = std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd())) {
                return if opened == path {
                    Ok(Opened::File(file))
                } else {
                    Err(Error::Source(format!("{} is outside the mounted directory", path.display())))
                };
            }
        }
        // Elsewhere, or without /proc, the parent is resolved instead; a symlink in
        // the final component was refused above.
        self.check_inside(path.parent().unwrap_or(&self.root))?;
        Ok(Opened::File(file))
    }
}

/// What [`DirSource::open_file`] found at a key.
enum Opened {
    File(std::fs::File),
    /// A symbolic link's target.
    Link(Bytes),
}

impl Source for DirSource {
    fn root(&self) -> Result<SourceKey> {
        Ok("".into())
    }

    fn read_dir(&self, key: &str) -> Result<Listing> {
        let dir = self.host_path(key);
        self.check_dir(key, &dir)?;
        let entries = std::fs::read_dir(&dir).map_err(|err| io_error(&dir, err))?;
        let mut listing = Listing::new();
        for entry in entries {
            let entry = entry.map_err(|err| io_error(&dir, err))?;
            let Ok(name) = entry.file_name().into_string() else {
                continue; // non-UTF-8 names cannot be addressed by a workspace path
            };
            // The entry's type usually comes with the listing, so only files, which
            // need a size and a mode, cost a stat (relative to the open directory).
            let file_type = entry.file_type().map_err(|err| io_error(&entry.path(), err))?;
            let child_key = if key.is_empty() { name.clone() } else { format!("{key}/{name}") };
            if let Some(found) = self.entry_of(child_key, file_type, || entry.metadata())? {
                listing.insert(name, found);
            }
        }
        Ok(listing)
    }

    fn entry_now(&self, key: &str) -> Result<Option<SourceEntry>> {
        if key.is_empty() || !Path::new(key).components().all(|part| matches!(part, Component::Normal(_))) {
            return Ok(None);
        }
        // The folder holding it is checked as a listing checks it: one swapped for a
        // link since must not lead outside.
        let parent = Path::new(key).parent().and_then(Path::to_str).unwrap_or_default();
        let path = self.host_path(key);
        let dir = path.parent().unwrap_or(&self.root);
        if !dir.is_dir() {
            return Ok(None);
        }
        self.check_dir(parent, dir)?;
        let meta = match path.symlink_metadata() {
            Ok(meta) => meta,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(io_error(&path, err)),
        };
        self.entry_of(key.to_owned(), meta.file_type(), || Ok(meta.clone()))
    }

    fn read_file(&self, key: &str) -> Result<Bytes> {
        let mut file = match self.open_file(key)? {
            Opened::File(file) => file,
            Opened::Link(target) => return Ok(target),
        };
        let error = |err| io_error(&self.host_path(key), err);
        let size = file.metadata().map_err(error)?.len();
        let mut data = Vec::with_capacity(usize::try_from(size).unwrap_or(0));
        file.read_to_end(&mut data).map_err(error)?;
        Ok(Bytes::from(data))
    }

    fn read_range(&self, key: &str, offset: u64, len: u64) -> Result<Bytes> {
        let mut file = match self.open_file(key)? {
            Opened::File(file) => file,
            Opened::Link(target) => return Ok(slice(&target, offset, len)),
        };
        let error = |err| io_error(&self.host_path(key), err);
        let available = file.metadata().map_err(error)?.len().saturating_sub(offset);
        file.seek(SeekFrom::Start(offset)).map_err(error)?;
        let mut data = Vec::with_capacity(usize::try_from(len.min(available)).unwrap_or(0));
        file.take(len).read_to_end(&mut data).map_err(error)?;
        Ok(Bytes::from(data))
    }
}

/// Where the OS would find the absolute `path`: the longest part of it that exists,
/// resolved through every link, plus the rest as written. The OS can't take `..` back
/// out of a folder that isn't there, so a missing folder before one keeps the link
/// dangling here too.
fn resolve(path: &Path) -> PathBuf {
    let parts: Vec<Component> = path.components().collect();
    (0..=parts.len())
        .rev()
        .find_map(|cut| {
            let existing: PathBuf = parts[..cut].iter().collect();
            let mut path = if cut == 0 { PathBuf::new() } else { existing.canonicalize().ok()? };
            path.extend(&parts[cut..]);
            Some(path)
        })
        .unwrap_or_default()
}

/// The relative link from the folder `from` to `to`, both relative to one root.
fn relative_link(from: &Path, to: &Path) -> PathBuf {
    let (from, to): (Vec<_>, Vec<_>) = (from.components().collect(), to.components().collect());
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut link: PathBuf = from[common..].iter().map(|_| Component::ParentDir).collect();
    link.extend(&to[common..]);
    if link.as_os_str().is_empty() { PathBuf::from(".") } else { link }
}

/// An error from the disk at `path`, naming it, so a log of failures says where.
fn io_error(path: &Path, err: std::io::Error) -> Error {
    Error::Source(format!("{}: {err}", path.display()))
}

#[cfg(unix)]
fn is_executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_meta: &std::fs::Metadata) -> bool {
    false
}
