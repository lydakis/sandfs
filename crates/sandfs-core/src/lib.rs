//! A lazy, copy-on-write filesystem for agents.
//!
//! A [`Workspace`] layers an in-memory overlay over read-only [`Source`]s (a git
//! commit, a host directory, in-memory data). Sources are read only when a path below
//! them is touched, [`Workspace::fork`] is O(1), and [`Workspace::diff`] reports the
//! overlay as file-level changes. Executing code is deliberately out of scope: sandfs
//! is the filesystem an agent works in, not the sandbox that runs its commands.

mod diff;
mod error;
mod path;
mod source;
mod workspace;

pub use diff::{Change, ChangeKind};
pub use error::{Error, Result};
pub use path::VPath;
pub use source::{DirSource, Kind, Listing, MemorySource, Source, SourceEntry, SourceKey};
pub use workspace::{DirEntry, Edit, Metadata, Workspace};
