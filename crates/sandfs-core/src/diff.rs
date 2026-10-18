//! Changes between a workspace and its mounts.

use crate::path::VPath;

/// How a file differs from the mounted base.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ChangeKind {
    /// The file does not exist in the base.
    Added,
    /// The file exists in the base with different content or mode.
    Modified,
    /// The file exists in the base but not in the workspace.
    Deleted,
}

/// One changed file, as returned by [`crate::Workspace::diff`].
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Change {
    /// Workspace path of the file.
    pub path: VPath,
    /// What happened to it.
    pub kind: ChangeKind,
}
