//! Error type shared by every workspace operation.

use thiserror::Error;

/// Result alias used throughout sandfs.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Everything that can go wrong in a workspace operation.
///
/// Variants carry the workspace path they refer to so callers (an MCP server, a shell)
/// can turn them into messages without extra context.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum Error {
    /// The path does not exist (or is hidden).
    #[error("{0}: no such file or directory")]
    NotFound(String),
    /// The path already exists.
    #[error("{0}: already exists")]
    AlreadyExists(String),
    /// A path component that must be a directory is not one.
    #[error("{0}: not a directory")]
    NotADirectory(String),
    /// A file operation was attempted on a directory.
    #[error("{0}: is a directory")]
    IsADirectory(String),
    /// A non-recursive removal hit a directory with children.
    #[error("{0}: directory not empty")]
    DirectoryNotEmpty(String),
    /// The path is not a valid absolute workspace path.
    #[error("{0}: invalid path")]
    InvalidPath(String),
    /// The operation would change workspace structure that is fixed, such as a mount point.
    #[error("{0}: {1}")]
    Unsupported(String, &'static str),
    /// The path is read-only in this workspace (see `Workspace::set_read_only`).
    #[error("{0}: read-only")]
    ReadOnly(String),
    /// An optimistic write found a different version than the caller expected.
    #[error("{path}: version mismatch (expected {expected}, found {actual})")]
    VersionMismatch {
        /// Path of the file.
        path: String,
        /// Version the caller expected.
        expected: u64,
        /// Version the file actually has.
        actual: u64,
    },
    /// A file that must be text is not valid UTF-8.
    #[error("{0}: not valid UTF-8 text")]
    NotText(String),
    /// A backing source (a directory, an API, a callback) failed.
    #[error("source error: {0}")]
    Source(String),
}
