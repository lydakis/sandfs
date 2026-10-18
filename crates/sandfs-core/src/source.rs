//! Read-only sources that back a workspace's base layer.
//!
//! A source exposes an immutable tree through opaque keys: a directory source uses
//! relative paths, while an API or object store can use whatever ids it already has. The workspace asks for a directory
//! listing the first time something under it is touched and for file bytes the first
//! time a file is read, and caches both, so a source only pays for what is used.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::io::{Read, Seek, SeekFrom};
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
    /// A file: any bytes.
    File {
        /// Whether the executable bit is set, reported as the mode on a FUSE mount.
        executable: bool,
    },
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
    /// Size in bytes when the source knows it without reading the file. When it is
    /// `None`, [`crate::Workspace::stat`] reads the file once to find it.
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
    /// Read the file with this key.
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
/// Symlinks and special files (sockets, devices, fifos) are skipped, so a link cannot
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

    /// The host path of a regular file inside `root`.
    fn file_path(&self, key: &str) -> Result<PathBuf> {
        let path = self.host_path(key);
        // A symlink in the final component is refused below, so only the parent needs
        // checking.
        self.check_inside(path.parent().unwrap_or(&self.root))?;
        if !path.symlink_metadata().map_err(io_error)?.is_file() {
            return Err(Error::Source(format!("{} is not a regular file", path.display())));
        }
        Ok(path)
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
            let kind = if meta.is_dir() {
                Kind::Dir
            } else if meta.is_file() {
                Kind::File { executable: is_executable(&meta) }
            } else {
                continue; // symlinks and special files are not part of a workspace
            };
            let child_key = if key.is_empty() { name.clone() } else { format!("{key}/{name}") };
            let size = matches!(kind, Kind::File { .. }).then(|| meta.len());
            listing.insert(name, SourceEntry { kind, key: child_key.as_str().into(), size });
        }
        Ok(listing)
    }

    fn read_file(&self, key: &str) -> Result<Bytes> {
        let path = self.file_path(key)?;
        std::fs::read(path).map(Bytes::from).map_err(io_error)
    }

    fn read_range(&self, key: &str, offset: u64, len: u64) -> Result<Bytes> {
        let mut file = std::fs::File::open(self.file_path(key)?).map_err(io_error)?;
        file.seek(SeekFrom::Start(offset)).map_err(io_error)?;
        let mut data = Vec::new();
        file.take(len).read_to_end(&mut data).map_err(io_error)?;
        Ok(Bytes::from(data))
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
