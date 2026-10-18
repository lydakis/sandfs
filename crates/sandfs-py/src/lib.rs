//! The `sandfs._core` extension module: Python bindings over `sandfs-core`.
//!
//! The bindings stay thin. Each method converts its arguments, calls the core and maps
//! core errors onto the closest built-in Python exception, so `except FileNotFoundError`
//! works the way Python users expect. Every call into the core releases the GIL, since
//! almost any path lookup can read a source and git may fetch from a remote.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;
use pyo3::create_exception;
use pyo3::exceptions::{
    PyException, PyFileExistsError, PyFileNotFoundError, PyIsADirectoryError, PyNotADirectoryError, PyOSError,
    PyValueError,
};
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use sandfs_core::{ChangeKind, DirSource, Edit, Error, Kind, MemorySource, Source};
use sandfs_git::GitSource;

create_exception!(
    sandfs,
    VersionMismatch,
    PyException,
    "A write passed `expected_version` and the file had changed. Carries `path`, `expected` and `actual`."
);
create_exception!(
    sandfs,
    EditFailed,
    PyValueError,
    "An exact-string edit did not apply: its `old` text matched zero times, or several times without `replace_all`."
);

/// Map a core error onto the matching Python exception.
fn to_py(err: Error) -> PyErr {
    let message = err.to_string();
    match err {
        Error::NotFound(_) => PyFileNotFoundError::new_err(message),
        Error::AlreadyExists(_) => PyFileExistsError::new_err(message),
        Error::NotADirectory(_) => PyNotADirectoryError::new_err(message),
        Error::IsADirectory(_) => PyIsADirectoryError::new_err(message),
        Error::InvalidPath(_) | Error::NotText(_) => PyValueError::new_err(message),
        Error::EditFailed { .. } => EditFailed::new_err(message),
        Error::VersionMismatch { path, expected, actual } => Python::attach(|py| {
            let err = VersionMismatch::new_err(message);
            let value = err.value(py);
            // Setting attributes on a fresh exception instance cannot fail in practice.
            let _ = value.setattr("path", path);
            let _ = value.setattr("expected", expected);
            let _ = value.setattr("actual", actual);
            err
        }),
        Error::DirectoryNotEmpty(_) | Error::Unsupported(..) | Error::Source(_) => PyOSError::new_err(message),
    }
}

/// `.or_raise()` turns a core result into a Python one.
trait OrRaise<T> {
    fn or_raise(self) -> PyResult<T>;
}

impl<T> OrRaise<T> for sandfs_core::Result<T> {
    fn or_raise(self) -> PyResult<T> {
        self.map_err(to_py)
    }
}

/// File contents passed from Python: text is stored as UTF-8.
#[derive(FromPyObject)]
enum Data {
    Text(String),
    Bytes(Vec<u8>),
}

impl From<Data> for Bytes {
    fn from(data: Data) -> Bytes {
        match data {
            Data::Text(text) => text.into(),
            Data::Bytes(bytes) => bytes.into(),
        }
    }
}

fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Dir => "dir",
        Kind::File { .. } => "file",
        Kind::Symlink => "symlink",
    }
}

/// An optional size the way Python prints it.
fn size_repr(size: Option<u64>) -> String {
    size.map_or_else(|| "None".to_owned(), |size| size.to_string())
}

fn is_executable(kind: Kind) -> bool {
    matches!(kind, Kind::File { executable: true })
}

/// One entry of a directory listing.
#[pyclass(module = "sandfs", frozen, get_all, eq)]
#[derive(Clone, PartialEq)]
struct DirEntry {
    /// Entry name, without its directory.
    name: String,
    /// `"file"`, `"dir"` or `"symlink"`.
    kind: String,
    /// Size in bytes when known without reading the file, else `None`.
    size: Option<u64>,
    /// Whether a file has its executable bit set.
    executable: bool,
}

#[pymethods]
impl DirEntry {
    fn __repr__(&self) -> String {
        format!("DirEntry(name={:?}, kind={:?}, size={})", self.name, self.kind, size_repr(self.size))
    }
}

/// What `Workspace.stat` reports about a path.
#[pyclass(module = "sandfs", frozen, get_all, eq)]
#[derive(Clone, PartialEq)]
struct Stat {
    /// `"file"`, `"dir"` or `"symlink"`.
    kind: String,
    /// Size in bytes when known without reading the file, else `None`.
    size: Option<u64>,
    /// Optimistic version: 0 for an untouched base file, new on every change.
    version: u64,
    /// Whether a file has its executable bit set.
    executable: bool,
}

#[pymethods]
impl Stat {
    fn __repr__(&self) -> String {
        format!("Stat(kind={:?}, size={}, version={})", self.kind, size_repr(self.size), self.version)
    }
}

/// One file that differs from the mounted base.
#[pyclass(module = "sandfs", frozen, get_all, eq)]
#[derive(Clone, PartialEq)]
struct Change {
    /// Workspace path of the file.
    path: String,
    /// `"added"`, `"modified"` or `"deleted"`.
    kind: String,
}

#[pymethods]
impl Change {
    fn __repr__(&self) -> String {
        format!("Change(path={:?}, kind={:?})", self.path, self.kind)
    }
}

/// A copy-on-write filesystem: read-only mounts plus an overlay of changes.
///
/// Mounts are read lazily, `fork()` is O(1), and `diff()` lists what changed. A
/// workspace is not meant to be shared between threads; fork one per thread instead.
#[pyclass(module = "sandfs")]
struct Workspace {
    inner: sandfs_core::Workspace,
}

impl Workspace {
    fn mount_source(&mut self, py: Python<'_>, path: &str, source: Arc<dyn Source>) -> PyResult<()> {
        let inner = &mut self.inner;
        py.detach(|| inner.mount(path, source)).or_raise()
    }
}

#[pymethods]
impl Workspace {
    /// An empty workspace: just the root directory.
    #[new]
    fn new() -> Self {
        Workspace { inner: sandfs_core::Workspace::new() }
    }

    fn __repr__(&self) -> String {
        format!("{:?}", self.inner)
    }

    /// Mount `rev` of the git repository at `repo`, read-only and lazily.
    ///
    /// The revision is resolved once. In a `blob:none` partial clone, git fetches each
    /// file's contents only when it is first read.
    #[pyo3(signature = (path, repo, rev = "HEAD"))]
    fn mount_git(&mut self, py: Python<'_>, path: &str, repo: PathBuf, rev: &str) -> PyResult<()> {
        let source = py.detach(|| GitSource::open(repo, rev)).or_raise()?;
        self.mount_source(py, path, Arc::new(source))
    }

    /// Mount the host directory `root`, read-only. Writes go to the overlay, never to disk.
    fn mount_dir(&mut self, py: Python<'_>, path: &str, root: PathBuf) -> PyResult<()> {
        let source = DirSource::new(root).or_raise()?;
        self.mount_source(py, path, Arc::new(source))
    }

    /// Mount in-memory files, given as `{relative path: str or bytes}`, read-only.
    fn mount_files(&mut self, py: Python<'_>, path: &str, files: HashMap<String, Data>) -> PyResult<()> {
        let source =
            MemorySource::from_files(files.into_iter().map(|(name, data)| (name, Bytes::from(data)))).or_raise()?;
        self.mount_source(py, path, Arc::new(source))
    }

    /// A new workspace with the same mounts and changes. O(1); later changes to
    /// either side are invisible to the other.
    fn fork(&self) -> Workspace {
        Workspace { inner: self.inner.fork() }
    }

    /// Whether `path` exists. A final symlink is not followed.
    fn exists(&self, py: Python<'_>, path: &str) -> bool {
        py.detach(|| self.inner.exists(path))
    }

    /// Kind, size and version of `path`. A final symlink is not followed.
    fn stat(&self, py: Python<'_>, path: &str) -> PyResult<Stat> {
        let meta = py.detach(|| self.inner.stat(path)).or_raise()?;
        Ok(Stat {
            kind: kind_name(meta.kind).to_owned(),
            size: meta.size,
            version: meta.version,
            executable: is_executable(meta.kind),
        })
    }

    /// The contents of the file at `path`, following a final symlink.
    fn read<'py>(&self, py: Python<'py>, path: &str) -> PyResult<Bound<'py, PyBytes>> {
        let data = py.detach(|| self.inner.read(path)).or_raise()?;
        Ok(PyBytes::new(py, &data))
    }

    /// The contents of the file at `path` as UTF-8 text; raises `ValueError` if it is not text.
    fn read_text(&self, py: Python<'_>, path: &str) -> PyResult<String> {
        py.detach(|| self.inner.read_text(path)).or_raise()
    }

    /// The target of the symlink at `path`.
    fn read_link(&self, py: Python<'_>, path: &str) -> PyResult<String> {
        py.detach(|| self.inner.read_link(path)).or_raise()
    }

    /// The entries of the directory at `path`, sorted by name.
    #[pyo3(signature = (path = "/"))]
    fn list(&self, py: Python<'_>, path: &str) -> PyResult<Vec<DirEntry>> {
        let entries = py.detach(|| self.inner.list(path)).or_raise()?;
        Ok(entries
            .into_iter()
            .map(|entry| DirEntry {
                name: entry.name,
                kind: kind_name(entry.kind).to_owned(),
                size: entry.size,
                executable: is_executable(entry.kind),
            })
            .collect())
    }

    /// Write `data` (str or bytes) to `path`, creating parent directories, and return
    /// the file's new version. With `expected_version`, raise `VersionMismatch` unless
    /// the file is still at that version (0 for a file that does not exist yet).
    #[pyo3(signature = (path, data, *, expected_version = None))]
    fn write(&mut self, py: Python<'_>, path: &str, data: Data, expected_version: Option<u64>) -> PyResult<u64> {
        let data = Bytes::from(data);
        let inner = &mut self.inner;
        py.detach(|| match expected_version {
            Some(expected) => inner.write_versioned(path, data, expected),
            None => inner.write(path, data),
        })
        .or_raise()
    }

    /// Replace `old` with `new` in the text file at `path` and return the new version.
    ///
    /// `old` must match exactly once unless `replace_all` is set; otherwise `EditFailed`
    /// is raised and the file is unchanged. A final symlink is followed, and
    /// `expected_version` is checked against the file it points to.
    #[pyo3(signature = (path, old, new, *, replace_all = false, expected_version = None))]
    fn edit(
        &mut self,
        py: Python<'_>,
        path: &str,
        old: String,
        new: String,
        replace_all: bool,
        expected_version: Option<u64>,
    ) -> PyResult<u64> {
        let edits = [Edit { old, new, replace_all }];
        let inner = &mut self.inner;
        py.detach(|| match expected_version {
            Some(expected) => inner.edit_versioned(path, &edits, expected),
            None => inner.edit(path, &edits),
        })
        .or_raise()
    }

    /// Create a directory; with `parents`, create missing parents and accept an existing directory.
    #[pyo3(signature = (path, *, parents = false))]
    fn mkdir(&mut self, py: Python<'_>, path: &str, parents: bool) -> PyResult<()> {
        let inner = &mut self.inner;
        py.detach(|| inner.mkdir(path, parents)).or_raise()
    }

    /// Create a symlink at `path` pointing to `target`.
    fn symlink(&mut self, py: Python<'_>, path: &str, target: &str) -> PyResult<()> {
        let inner = &mut self.inner;
        py.detach(|| inner.symlink(path, target)).or_raise()
    }

    /// Remove a file, symlink or empty directory; with `recursive`, remove a whole tree.
    #[pyo3(signature = (path, *, recursive = false))]
    fn remove(&mut self, py: Python<'_>, path: &str, recursive: bool) -> PyResult<()> {
        let inner = &mut self.inner;
        py.detach(|| inner.remove(path, recursive)).or_raise()
    }

    /// Copy `src` to `dst`; directories need `recursive`. Base files are copied by
    /// reference, so nothing is read.
    #[pyo3(signature = (src, dst, *, recursive = false))]
    fn copy(&mut self, py: Python<'_>, src: &str, dst: &str, recursive: bool) -> PyResult<()> {
        let inner = &mut self.inner;
        py.detach(|| inner.copy(src, dst, recursive)).or_raise()
    }

    /// Move or rename `src` to `dst`, which must not exist yet.
    #[pyo3(name = "move")]
    fn rename(&mut self, py: Python<'_>, src: &str, dst: &str) -> PyResult<()> {
        let inner = &mut self.inner;
        py.detach(|| inner.rename(src, dst)).or_raise()
    }

    /// The files that differ from the mounts, sorted by path.
    fn diff(&self, py: Python<'_>) -> PyResult<Vec<Change>> {
        let changes = py.detach(|| self.inner.diff()).or_raise()?;
        Ok(changes
            .into_iter()
            .map(|change| Change {
                path: change.path.as_str().to_owned(),
                kind: match change.kind {
                    ChangeKind::Added => "added",
                    ChangeKind::Modified => "modified",
                    ChangeKind::Deleted => "deleted",
                }
                .to_owned(),
            })
            .collect())
    }
}

#[pymodule]
fn _core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Workspace>()?;
    m.add_class::<DirEntry>()?;
    m.add_class::<Stat>()?;
    m.add_class::<Change>()?;
    m.add("VersionMismatch", m.py().get_type::<VersionMismatch>())?;
    m.add("EditFailed", m.py().get_type::<EditFailed>())?;
    Ok(())
}
