//! A lazy, copy-on-write sandbox filesystem for agents.
//!
//! A [`Workspace`] layers an in-memory overlay over read-only [`Source`]s: a host
//! directory, in-memory files, or any tree a harness implements (an API, an object
//! store, a git commit). Files are bytes of any kind. Sources are read only when a
//! path below them is touched, [`Workspace::fork`] is O(1), and [`Workspace::changes`]
//! reports the overlay as file-level changes. Running commands is deliberately out of
//! scope: sandfs is the filesystem an agent works in, not the runtime that executes.

mod change;
mod error;
mod path;
mod source;
mod workspace;

pub use change::{Change, ChangeKind};
pub use error::{Error, Result};
pub use path::VPath;
pub use source::{DirSource, Kind, Listing, MemorySource, Source, SourceEntry, SourceKey};
pub use workspace::{DirEntry, Metadata, Stored, Workspace};

// Keeps the README example compiling.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeExample;
