//! Read-only sources that back a workspace's base layer.
//!
//! A source exposes an immutable tree through opaque keys: a git source uses object
//! ids, a directory source uses relative paths. The workspace asks for a directory
//! listing the first time something under it is touched and for file bytes the first
//! time a file is read, and caches both, so a source only pays for what is used.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;

use crate::error::{Error, Result};

/// Opaque identifier a source uses for one of its trees or files.
pub type SourceKey = Arc<str>;

/// What a directory entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    /// A directory.
    Dir,
    /// A regular file.
    File {
        /// Whether the executable bit is set.
        executable: bool,
    },
    /// A symbolic link; its content is the link target.
    Symlink,
}

impl Kind {
    /// True for directories.
    pub fn is_dir(self) -> bool {
        matches!(self, Kind::Dir)
    }
}

/// One entry in a source directory listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceEntry {
    /// What the entry is.
    pub kind: Kind,
    /// Key to pass back to [`Source::read_dir`] or [`Source::read_file`].
    pub key: SourceKey,
    /// Size in bytes when the source knows it without reading the file.
    pub size: Option<u64>,
}

/// A directory listing, sorted by name.
pub type Listing = BTreeMap<String, SourceEntry>;

/// An immutable, lazily read tree.
///
/// Implementations must return the same answer for the same key every time; the
/// workspace caches results and shares them between forks.
pub trait Source: Send + Sync + fmt::Debug {
    /// Key of the root directory.
    fn root(&self) -> Result<SourceKey>;
    /// List the directory with this key.
    fn read_dir(&self, key: &str) -> Result<Listing>;
    /// Read the file (or symlink target) with this key.
    fn read_file(&self, key: &str) -> Result<Bytes>;
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
            source.insert(path.as_ref(), data.into(), Kind::File { executable: false })?;
        }
        Ok(source)
    }

    /// Add a file, executable file or symlink at a relative path.
    pub fn insert(&mut self, path: &str, data: Bytes, kind: Kind) -> Result<()> {
        if kind.is_dir() {
            return Err(Error::Unsupported(path.to_owned(), "insert files, not directories"));
        }
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
            .insert((*name).to_owned(), SourceEntry { kind, key: file_key.as_str().into(), size });
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

/// A read-only view of a host directory, read lazily and treated as a snapshot.
///
/// Symlinks are reported as symlinks and never followed on the host, so a link cannot
/// lead the workspace outside `root`. Each read also checks that the resolved host
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
        Ok(DirSource { root: root.canonicalize().map_err(io_error)? })
    }

    /// Fail unless `dir`, with every symlink resolved, is still `root` or inside it.
    fn check_inside(&self, dir: &Path) -> Result<()> {
        if dir.canonicalize().map_err(io_error)?.starts_with(&self.root) {
            Ok(())
        } else {
            Err(Error::Source(format!("{} is outside the mounted directory", dir.display())))
        }
    }

    fn host_path(&self, key: &str) -> PathBuf {
        if key.is_empty() { self.root.clone() } else { self.root.join(key) }
    }
}

impl Source for DirSource {
    fn root(&self) -> Result<SourceKey> {
        Ok("".into())
    }

    fn read_dir(&self, key: &str) -> Result<Listing> {
        let dir = self.host_path(key);
        self.check_inside(&dir)?;
        let entries = std::fs::read_dir(dir).map_err(io_error)?;
        let mut listing = Listing::new();
        for entry in entries {
            let entry = entry.map_err(io_error)?;
            let Ok(name) = entry.file_name().into_string() else {
                continue; // non-UTF-8 names cannot be addressed by a workspace path
            };
            let meta = entry.path().symlink_metadata().map_err(io_error)?;
            let kind = if meta.file_type().is_symlink() {
                Kind::Symlink
            } else if meta.is_dir() {
                Kind::Dir
            } else if meta.is_file() {
                Kind::File { executable: is_executable(&meta) }
            } else {
                continue; // sockets, devices and fifos are not part of a workspace
            };
            let child_key = if key.is_empty() { name.clone() } else { format!("{key}/{name}") };
            let size = matches!(kind, Kind::File { .. }).then(|| meta.len());
            listing.insert(name, SourceEntry { kind, key: child_key.as_str().into(), size });
        }
        Ok(listing)
    }

    fn read_file(&self, key: &str) -> Result<Bytes> {
        let path = self.host_path(key);
        // The final component is never followed, so only its parent needs checking.
        self.check_inside(path.parent().unwrap_or(&self.root))?;
        let meta = path.symlink_metadata().map_err(io_error)?;
        if meta.file_type().is_symlink() {
            let target = std::fs::read_link(&path).map_err(io_error)?;
            return Ok(Bytes::from(target.to_string_lossy().into_owned()));
        }
        std::fs::read(path).map(Bytes::from).map_err(io_error)
    }
}

fn io_error(err: std::io::Error) -> Error {
    Error::Source(err.to_string())
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
