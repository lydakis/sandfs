//! The `sandfs._core` extension module: Python bindings over `sandfs-core`.
//!
//! The bindings stay thin. Each method converts its arguments, calls the core and maps
//! core errors onto the closest built-in Python exception, so `except FileNotFoundError`
//! works the way Python users expect. Every call into the core releases the GIL, since
//! almost any path lookup can read a source, and a source may be slow (a remote, an
//! API). Sources written in Python take the GIL back only while their own code runs.

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;
use pyo3::create_exception;
use pyo3::exceptions::{
    PyException, PyFileExistsError, PyFileNotFoundError, PyIsADirectoryError, PyNotADirectoryError, PyOSError,
    PyTypeError, PyValueError,
};
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use sandfs_core::{ChangeKind, DirSource, Error, Kind, Listing, MemorySource, Source, SourceEntry, SourceKey};
use sandfs_git::GitSource;

create_exception!(
    sandfs,
    VersionMismatch,
    PyException,
    "A write passed `expected_version` and the file had changed. Carries `path`, `expected` and `actual`."
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
    }
}

/// An optional size the way Python prints it.
fn size_repr(size: Option<u64>) -> String {
    size.map_or_else(|| "None".to_owned(), |size| size.to_string())
}

fn is_executable(kind: Kind) -> bool {
    matches!(kind, Kind::File { executable: true })
}

/// One entry of a directory listing, returned by `Workspace.list` and by a Python
/// source's `list`.
#[pyclass(module = "sandfs", frozen, get_all, eq)]
#[derive(Clone, PartialEq)]
struct DirEntry {
    /// Entry name, without its directory.
    name: String,
    /// `"file"` or `"dir"`.
    kind: String,
    /// Size in bytes when known without reading the file, else `None`.
    size: Option<u64>,
    /// Whether a file has its executable bit set.
    executable: bool,
}

#[pymethods]
impl DirEntry {
    #[new]
    #[pyo3(signature = (name, kind = "file", *, size = None, executable = false))]
    fn new(name: String, kind: &str, size: Option<u64>, executable: bool) -> PyResult<Self> {
        if kind != "file" && kind != "dir" {
            return Err(PyValueError::new_err(format!("kind must be \"file\" or \"dir\", not {kind:?}")));
        }
        Ok(DirEntry { name, kind: kind.to_owned(), size, executable })
    }

    fn __repr__(&self) -> String {
        format!("DirEntry(name={:?}, kind={:?}, size={})", self.name, self.kind, size_repr(self.size))
    }
}

/// What `Workspace.stat` reports about a path.
#[pyclass(module = "sandfs", frozen, get_all, eq)]
#[derive(Clone, PartialEq)]
struct Stat {
    /// `"file"` or `"dir"`.
    kind: String,
    /// Size in bytes; 0 for a directory.
    size: u64,
    /// Optimistic version: 0 for an untouched base file, new on every change.
    version: u64,
    /// Whether a file has its executable bit set.
    executable: bool,
}

#[pymethods]
impl Stat {
    fn __repr__(&self) -> String {
        format!("Stat(kind={:?}, size={}, version={})", self.kind, self.size, self.version)
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

/// A read-only view of a host directory, read lazily. It is not a snapshot: a directory
/// that changes while mounted can show a mix of old and new contents. Symlinks and
/// special files are skipped.
#[pyclass(module = "sandfs", name = "DirSource", frozen)]
struct PyDirSource {
    inner: Arc<dyn Source>,
}

#[pymethods]
impl PyDirSource {
    #[new]
    fn new(py: Python<'_>, root: PathBuf) -> PyResult<Self> {
        let source = py.detach(|| DirSource::new(root)).or_raise()?;
        Ok(PyDirSource { inner: Arc::new(source) })
    }

    fn __repr__(&self) -> String {
        format!("{:?}", self.inner)
    }
}

/// Files held in memory, given as `{relative path: str or bytes}`.
#[pyclass(module = "sandfs", name = "MemorySource", frozen)]
struct PyMemorySource {
    inner: Arc<dyn Source>,
}

#[pymethods]
impl PyMemorySource {
    #[new]
    fn new(py: Python<'_>, files: HashMap<String, Data>) -> PyResult<Self> {
        let files = files.into_iter().map(|(name, data)| (name, Bytes::from(data)));
        let source = py.detach(|| MemorySource::from_files(files)).or_raise()?;
        Ok(PyMemorySource { inner: Arc::new(source) })
    }
}

/// A commit of a local git repository, read lazily without a checkout.
///
/// `rev` is resolved once. In a `blob:none` partial clone, git fetches a file's
/// contents only when it is first read.
#[pyclass(module = "sandfs", name = "GitSource", frozen)]
struct PyGitSource {
    inner: Arc<dyn Source>,
}

#[pymethods]
impl PyGitSource {
    #[new]
    #[pyo3(signature = (repo, rev = "HEAD"))]
    fn new(py: Python<'_>, repo: PathBuf, rev: &str) -> PyResult<Self> {
        let source = py.detach(|| GitSource::open(repo, rev)).or_raise()?;
        Ok(PyGitSource { inner: Arc::new(source) })
    }

    fn __repr__(&self) -> String {
        format!("{:?}", self.inner)
    }
}

/// A source written in Python: any object with `list(path)` returning `DirEntry`s and
/// `read(path)` returning bytes or str, and optionally `read_range(path, offset, length)`
/// for sources that can read part of a file. Paths are absolute within the source, so
/// the root is `"/"`, and they double as the source keys.
struct PySource {
    object: Py<PyAny>,
}

impl fmt::Debug for PySource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = Python::attach(|py| self.object.bind(py).repr().map(|repr| repr.to_string()));
        f.debug_tuple("PySource").field(&name.unwrap_or_else(|_| "?".to_owned())).finish()
    }
}

/// A Python exception raised by a source, as a core error that keeps its message.
fn source_error(py: Python<'_>, path: &str, err: PyErr) -> Error {
    let kind = err.get_type(py).name().map_or_else(|_| "Exception".to_owned(), |name| name.to_string());
    Error::Source(format!("{path}: {kind}: {}", err.value(py)))
}

impl Source for PySource {
    fn root(&self) -> sandfs_core::Result<SourceKey> {
        Ok("/".into())
    }

    fn read_dir(&self, key: &str) -> sandfs_core::Result<Listing> {
        Python::attach(|py| {
            let fail = |err| source_error(py, key, err);
            let entries = self.object.bind(py).call_method1("list", (key,)).map_err(fail)?;
            let mut listing = Listing::new();
            for item in entries.try_iter().map_err(fail)? {
                let item = item.map_err(fail)?;
                let entry = item.extract::<PyRef<'_, DirEntry>>().map_err(|err| fail(err.into()))?;
                let name = entry.name.as_str();
                if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
                    return Err(Error::Source(format!("{key}: invalid entry name {name:?}")));
                }
                let kind = match entry.kind.as_str() {
                    "dir" => Kind::Dir,
                    _ => Kind::File { executable: entry.executable },
                };
                let child = if key == "/" { format!("/{name}") } else { format!("{key}/{name}") };
                let size = if kind.is_dir() { None } else { entry.size };
                listing.insert(name.to_owned(), SourceEntry { kind, key: child.as_str().into(), size });
            }
            Ok(listing)
        })
    }

    fn read_file(&self, key: &str) -> sandfs_core::Result<Bytes> {
        Python::attach(|py| {
            let data = self.object.bind(py).call_method1("read", (key,)).map_err(|err| source_error(py, key, err))?;
            let data = data.extract::<Data>().map_err(|err| source_error(py, key, err))?;
            Ok(Bytes::from(data))
        })
    }

    fn read_range(&self, key: &str, offset: u64, len: u64) -> sandfs_core::Result<Bytes> {
        let ranged = Python::attach(|py| -> sandfs_core::Result<Option<Bytes>> {
            let object = self.object.bind(py);
            let fail = |err| source_error(py, key, err);
            if !object.hasattr("read_range").map_err(fail)? {
                return Ok(None);
            }
            let data = object.call_method1("read_range", (key, offset, len)).map_err(fail)?;
            Ok(Some(Bytes::from(data.extract::<Data>().map_err(fail)?)))
        })?;
        match ranged {
            // A source may return more than asked; never hand back more than the range.
            Some(data) => Ok(data.slice(..data.len().min(usize::try_from(len).unwrap_or(usize::MAX)))),
            None => {
                let data = self.read_file(key)?;
                let start = usize::try_from(offset).unwrap_or(usize::MAX).min(data.len());
                let end = start.saturating_add(usize::try_from(len).unwrap_or(usize::MAX)).min(data.len());
                Ok(data.slice(start..end))
            }
        }
    }
}

/// The core source behind a built-in source object, or a wrapper around a Python one.
fn as_source(source: &Bound<'_, PyAny>) -> PyResult<Arc<dyn Source>> {
    if let Ok(dir) = source.extract::<PyRef<'_, PyDirSource>>() {
        return Ok(dir.inner.clone());
    }
    if let Ok(memory) = source.extract::<PyRef<'_, PyMemorySource>>() {
        return Ok(memory.inner.clone());
    }
    if let Ok(git) = source.extract::<PyRef<'_, PyGitSource>>() {
        return Ok(git.inner.clone());
    }
    if source.hasattr("list")? && source.hasattr("read")? {
        return Ok(Arc::new(PySource { object: source.clone().unbind() }));
    }
    Err(PyTypeError::new_err("a source needs list(path) and read(path) methods"))
}

/// A copy-on-write filesystem: read-only mounts plus an overlay of changes.
///
/// Mounts are read lazily, `fork()` is O(1), and `changes()` lists what changed. A
/// workspace is not meant to be shared between threads; fork one per thread instead.
#[pyclass(module = "sandfs")]
struct Workspace {
    inner: sandfs_core::Workspace,
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

    /// Mount `source` read-only at `path`: a `DirSource`, `MemorySource`, `GitSource`,
    /// or any object with `list(path)` and `read(path)` methods. Writes go to the
    /// overlay, never to the source, and the source must return the same answer every
    /// time, since what it returns is cached and shared with forks.
    fn mount(&mut self, py: Python<'_>, path: &str, source: &Bound<'_, PyAny>) -> PyResult<()> {
        let source = as_source(source)?;
        let inner = &mut self.inner;
        py.detach(|| inner.mount(path, source)).or_raise()
    }

    /// A new workspace with the same mounts and changes. O(1); later changes to
    /// either side are invisible to the other.
    fn fork(&self) -> Workspace {
        Workspace { inner: self.inner.fork() }
    }

    /// Whether `path` exists.
    fn exists(&self, py: Python<'_>, path: &str) -> bool {
        py.detach(|| self.inner.exists(path))
    }

    /// Kind, size and version of `path`. If a source did not report a file's size,
    /// the file is read once to find it.
    fn stat(&self, py: Python<'_>, path: &str) -> PyResult<Stat> {
        let meta = py.detach(|| self.inner.stat(path)).or_raise()?;
        Ok(Stat {
            kind: kind_name(meta.kind).to_owned(),
            size: meta.size,
            version: meta.version,
            executable: is_executable(meta.kind),
        })
    }

    /// The contents of the file at `path`, or with `offset` or `length` only that part
    /// of it. A part of a file that has not been read whole is read from its source
    /// and not cached, so a large file is never held in memory whole.
    #[pyo3(signature = (path, *, offset = 0, length = None))]
    fn read<'py>(
        &self,
        py: Python<'py>,
        path: &str,
        offset: u64,
        length: Option<u64>,
    ) -> PyResult<Bound<'py, PyBytes>> {
        let data = py
            .detach(|| match (offset, length) {
                (0, None) => self.inner.read(path),
                (offset, length) => self.inner.read_range(path, offset, length.unwrap_or(u64::MAX)),
            })
            .or_raise()?;
        Ok(PyBytes::new(py, &data))
    }

    /// The contents of the file at `path` as UTF-8 text; raises `ValueError` if it is not text.
    fn read_text(&self, py: Python<'_>, path: &str) -> PyResult<String> {
        py.detach(|| self.inner.read_text(path)).or_raise()
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

    /// Create a directory; with `parents`, create missing parents and accept an existing directory.
    #[pyo3(signature = (path, *, parents = false))]
    fn mkdir(&mut self, py: Python<'_>, path: &str, parents: bool) -> PyResult<()> {
        let inner = &mut self.inner;
        py.detach(|| inner.mkdir(path, parents)).or_raise()
    }

    /// Remove a file or empty directory; with `recursive`, remove a whole tree.
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
    fn changes(&self, py: Python<'_>) -> PyResult<Vec<Change>> {
        let changes = py.detach(|| self.inner.changes()).or_raise()?;
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
    m.add_class::<PyDirSource>()?;
    m.add_class::<PyMemorySource>()?;
    m.add_class::<PyGitSource>()?;
    m.add("VersionMismatch", m.py().get_type::<VersionMismatch>())?;
    Ok(())
}
