//! Lazy, copy-on-write workspaces over host folders and git repositories, saved on
//! disk and mounted as directories: FUSE on Linux, and on macOS an NFS server on
//! localhost that macOS's own client mounts. This is the library behind the `sandfs`
//! command, for programs that want its workspaces without running it.
//!
//! Workspaces live in a [`Home`], a folder of their saved changes. Each is over a
//! source, read lazily and never written to: a host folder, a commit of a git
//! repository (remote ones become partial clones, fetched as files are read), or a
//! frozen copy of a plain folder. Every operation of the command is a method:
//!
//! ```no_run
//! # fn main() -> anyhow::Result<()> {
//! // First thing in `main`: sandfs runs this program again to serve mounts.
//! sandfs::handle_relaunch(&[]);
//! let home = sandfs::Home::locate()?;
//! let docs = home.mount("/srv/docs", "/tmp/docs".as_ref(), &Default::default())?;
//! let attempt = home.fork(&docs.name, Some("/tmp/attempt".as_ref()), &Default::default())?;
//! std::fs::write("/tmp/attempt/notes.md", "draft\n")?;
//! for change in home.changes(&attempt.name, &Default::default())?.changes {
//!     println!("{:?} {}", change.kind, change.path);
//! }
//! home.remove(&attempt.name, true)?;
//! # Ok(())
//! # }
//! ```
//!
//! A mount is served by one background process per home, which outlives the program
//! that asked for it. That process is this program, started again; see
//! [`handle_relaunch`].
//!
//! `sandfs-core` is the in-memory engine underneath: sources, forks and changes,
//! with nothing on disk.

// Without a mount, mounting is stubbed out, which leaves its helpers unused.
#![cfg_attr(not(mount), allow(dead_code, unused_imports))]

mod copy;
#[cfg(mount)]
mod fs;
mod git;
mod github;
mod host;
mod mcp;
#[cfg(any(nfs, all(test, fuse)))]
mod nfs;
#[cfg(any(nfs, all(test, fuse)))]
mod nfsserve;
mod ops;
mod persist;
mod relaunch;
#[cfg(mount)]
mod server;
mod shell;
mod store;
mod update;

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::Result;
pub use sandfs_core::ChangeKind;

pub(crate) use crate::ops::resolve_new;
#[cfg(mount)]
pub(crate) use crate::ops::{serve, served_times};
pub use crate::relaunch::handle_relaunch;
pub use crate::update::Updated;

/// A folder of workspaces: each one's source, saved changes and mount.
#[derive(Debug)]
pub struct Home {
    inner: store::Home,
}

impl Home {
    /// The home in `$SANDFS_HOME`, else `$XDG_DATA_HOME/sandfs`, else
    /// `~/.local/share/sandfs`, created if missing.
    pub fn locate() -> Result<Home> {
        Ok(Home { inner: store::Home::locate()? })
    }

    /// The home's folder.
    pub fn root(&self) -> &Path {
        self.inner.root()
    }

    /// Create a workspace over `source` and mount it at `dir`, or mount the existing
    /// workspace `source` names there again.
    ///
    /// `source` is a host folder, read lazily and never written to; `git:PATH-OR-URL`,
    /// optionally with `@REV`, or a URL on its own, for a commit; or the name of a
    /// workspace. The mount is served in the background (see [`handle_relaunch`])
    /// unless [`MountOptions::foreground`] is set, in which case this returns once it
    /// is unmounted.
    pub fn mount(&self, source: &str, dir: &Path, options: &MountOptions) -> Result<Mounted> {
        ops::mount_source(&self.inner, source, dir, options)
    }

    /// Fork `from`, with its changes so far, and mount the fork at `dir`; without
    /// `dir`, keep it unmounted, as a snapshot. The source is never copied.
    ///
    /// `from` is a workspace (its name, the folder it is mounted on, or a folder in
    /// that); a git repository's working folder that sandfs doesn't serve, such as a
    /// clone, forked as its HEAD commit plus the files it changed since; or a plain
    /// folder, frozen as it is now (which needs a disk that clones files: APFS, btrfs
    /// or XFS). Later changes to `from` don't reach the fork.
    pub fn fork(&self, from: &str, dir: Option<&Path>, options: &ForkOptions) -> Result<Forked> {
        ops::fork(&self.inner, from, dir, options)
    }

    /// The files `workspace` added, modified or deleted, relative to its source or to
    /// [`ChangesOptions::since`], and with [`ChangesOptions::copy_to`] written there.
    pub fn changes(&self, workspace: &str, options: &ChangesOptions) -> Result<Changes> {
        ops::changes(&self.inner, workspace, options)
    }

    /// Move `workspace` onto a newer version of its source, keeping its own changes:
    /// `source`, or the source looked up again as it was given (the newest commit of a
    /// branch, a new frozen copy of a plain folder). A mounted workspace moves in
    /// place, so programs working in it carry on.
    pub fn update(&self, workspace: &str, source: Option<&str>) -> Result<Updated> {
        let name = self.inner.find(workspace)?;
        update::update(&self.inner, &name, source)
    }

    /// Every workspace, with its source and where it is mounted.
    pub fn workspaces(&self) -> Result<Vec<WorkspaceInfo>> {
        ops::workspaces(&self.inner)
    }

    /// The name of the workspace `workspace` names: a name, or the folder it is
    /// mounted on.
    pub fn find(&self, workspace: &str) -> Result<String> {
        self.inner.find(workspace)
    }

    /// Unmount `workspace` once its changes are saved. `mount` brings it back.
    pub fn unmount(&self, workspace: &str) -> Result<()> {
        ops::unmount_workspace(&self.inner, workspace)
    }

    /// Delete the workspace `name` and its changes. A mounted one is refused, unless
    /// `unmount` says to unmount it first; then `name` may also be its folder.
    pub fn remove(&self, name: &str, unmount: bool) -> Result<()> {
        ops::remove(&self.inner, name, unmount)
    }

    /// Free the disk space remote repositories' downloaded files take up: shrink each
    /// local copy none of whose workspaces is in use to what they need, and delete
    /// those no workspace uses. Workspaces and their changes are never touched.
    pub fn clean(&self) -> Result<Vec<Cleaned>> {
        ops::clean(&self.inner)
    }

    /// Serve `workspace` to an agent as MCP file tools over standard input and output,
    /// until the agent hangs up; or create a workspace over a source, as for
    /// [`Home::mount`], and serve that.
    pub fn serve_mcp(&self, workspace: &str, options: &ServeOptions) -> Result<()> {
        ops::serve_mcp(&self.inner, workspace, options)
    }

    /// Run `command`, or the user's shell when empty, confined to the mounted
    /// `workspace`: it sees the mount and the system's programs, not the user's files,
    /// and no network unless `network`. Returns its exit code.
    pub fn shell(&self, workspace: &str, network: bool, command: &[OsString]) -> Result<i32> {
        ops::shell(&self.inner, workspace, network, command)
    }
}

/// How [`Home::mount`] makes a workspace.
#[derive(Debug, Clone, Default)]
pub struct MountOptions {
    /// The new workspace's name; by default the folder's last component.
    pub name: Option<String>,
    /// Files or folders, relative to the source, that programs may read but not
    /// change, even in the workspace. Only for a new workspace; forks keep them.
    pub read_only: Vec<String>,
    /// Serve the mount in this process, returning once it is unmounted.
    pub foreground: bool,
}

/// How [`Home::fork`] makes a fork.
#[derive(Debug, Clone, Default)]
pub struct ForkOptions {
    /// The fork's name; by default the folder's last component, and needed without one.
    pub name: Option<String>,
    /// Read-only paths to add to those `from` has, as for [`MountOptions::read_only`].
    pub read_only: Vec<String>,
    /// With a git source, start git in the fork on this new branch, at the commit
    /// `from` is on, as `git switch -c` would.
    pub branch: Option<String>,
    /// Serve the mount in this process, returning once it is unmounted.
    pub foreground: bool,
}

/// What [`Home::changes`] lists, and where it copies them.
#[derive(Debug, Clone, Default)]
pub struct ChangesOptions {
    /// Only these files, or the files in these folders, relative to the workspace.
    pub paths: Vec<String>,
    /// Compare with this workspace instead of the source: a snapshot, or another fork
    /// of the same one.
    pub since: Option<String>,
    /// Also write the changes into this folder: added and modified files are copied
    /// there and deleted ones removed, so a copy of the source (or of `since`) comes to
    /// match the workspace. A file there that changed since the copy was made is never
    /// replaced or removed: the copy is refused, naming it.
    pub copy_to: Option<PathBuf>,
    /// With `copy_to`, replace or remove such files too.
    pub force: bool,
}

/// How [`Home::serve_mcp`] makes a workspace over a source.
#[derive(Debug, Clone, Default)]
pub struct ServeOptions {
    /// The new workspace's name; by default the source's last component.
    pub name: Option<String>,
    /// Read-only paths, as for [`MountOptions::read_only`].
    pub read_only: Vec<String>,
}

/// A workspace [`Home::mount`] mounted.
#[derive(Debug, Clone)]
pub struct Mounted {
    /// The workspace's name.
    pub name: String,
    /// Where it is mounted, resolved.
    pub dir: PathBuf,
}

/// A fork [`Home::fork`] made.
#[derive(Debug, Clone)]
pub struct Forked {
    /// The fork's name.
    pub name: String,
    /// What it was forked from: a workspace's name, or a folder.
    pub from: String,
    /// Where it is mounted, or `None` for a snapshot.
    pub dir: Option<PathBuf>,
}

/// What [`Home::changes`] found.
#[derive(Debug, Clone)]
pub struct Changes {
    /// The changes, by path.
    pub changes: Vec<Change>,
    /// Changes in a git mount's `.git` settings that a copy left out, since git runs
    /// the programs they name.
    pub held_back: usize,
    /// The changed files among `.git/config` and `.git/hooks`, relative to the
    /// workspace, for whoever reviews it before running git there.
    pub git_settings: Vec<String>,
}

/// One changed file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// Added, modified or deleted.
    pub kind: ChangeKind,
    /// Relative to the workspace, with `/` between folders.
    pub path: String,
}

/// A workspace, as [`Home::workspaces`] lists it.
#[derive(Debug, Clone)]
pub struct WorkspaceInfo {
    /// Its name.
    pub name: String,
    /// Its source, as given.
    pub source: String,
    /// Where it is mounted, if it is.
    pub mounted: Option<PathBuf>,
}

/// What [`Home::clean`] did with one local copy of a remote repository.
#[derive(Debug, Clone)]
pub struct Cleaned {
    /// The copy, relative to the home's `repos` folder.
    pub repository: String,
    /// What happened to it.
    pub outcome: CleanOutcome,
}

/// See [`Cleaned`].
#[derive(Debug, Clone)]
pub enum CleanOutcome {
    /// No workspace uses it any more, so it went.
    Deleted {
        /// Bytes freed.
        freed: u64,
    },
    /// Shrunk to the commits and folder listings its workspaces need.
    Shrunk {
        /// Bytes freed.
        freed: u64,
    },
    /// Left alone, since this workspace over it is in use.
    Kept {
        /// The workspace in use.
        busy: String,
    },
}
