//! Copies of folders that cost nothing to make, saved on disk and mounted as real
//! folders any program can work in: FUSE on Linux, and on macOS an NFS server on
//! localhost that macOS's own client mounts, so nothing needs installing. This is the
//! library behind the `sandfs` command, for a program that wants those copies without
//! anyone installing or running `sandfs`.
//!
//! Workspaces live in a [`Home`], a folder of their saved changes. Each is over a
//! source, read lazily and never written to: a folder on disk, a commit of a git
//! repository (a remote one is fetched a file at a time, as files are read), or a
//! frozen copy of a plain folder. Every command of `sandfs` is a method of [`Home`]
//! that returns what it found:
//!
//! - [`Home::mount`] makes a workspace over a source and mounts it at a folder.
//! - [`Home::fork`] copies a workspace, with its changes so far and none of its
//!   source, mounted or kept as a snapshot; a copy of a git clone can also be a real
//!   folder of cloned files, as it is by default on macOS.
//! - [`Home::changes`] lists the files a workspace added, modified or deleted, against
//!   its source or another workspace, and with [`ChangesOptions::copy_to`] writes them
//!   into a folder.
//! - [`Home::update`] moves a workspace onto a newer version of its source, keeping
//!   its own changes, even while it is mounted.
//! - [`Home::workspaces`], [`Home::unmount`], [`Home::remove`] and [`Home::clean`]
//!   list, unmount and delete them, and free downloaded space.
//! - [`Home::serve_mcp`] hands a workspace to an agent as MCP file tools, and
//!   [`Home::shell`] runs a command confined to a mount by the operating system's
//!   sandbox.
//!
//! ```no_run
//! # fn main() -> anyhow::Result<()> {
//! // First thing in `main`: sandfs runs this program again to serve mounts.
//! sandfs::handle_relaunch(&[]);
//! let home = sandfs::Home::locate()?;
//! // A folder, mounted at /tmp/docs; /srv/docs itself is never written to.
//! let docs = home.mount("/srv/docs", "/tmp/docs".as_ref(), &Default::default())?;
//! // A copy of it, mounted at /tmp/attempt, which any program can work in.
//! let attempt = home.fork(&docs.name, Some("/tmp/attempt".as_ref()), &Default::default())?;
//! std::fs::write("/tmp/attempt/notes.md", "draft\n")?;
//! for change in home.changes(&attempt.name, &Default::default())?.changes {
//!     println!("{:?} {}", change.kind, change.path); // Added notes.md
//! }
//! home.remove(&attempt.name, true)?;
//! # Ok(())
//! # }
//! ```
//!
//! `examples/attempts.rs` in the repository runs a whole round trip in a temporary
//! folder: two copies of a folder, changed two ways, compared, and one written back.
//! [Graft](https://github.com/lydakis/graft), which gives each task its own copy of a
//! GitHub repository, is a program built on this crate.
//!
//! # The one line in `main`
//!
//! A mount is served by one background process per home, which outlives the program
//! that asked for it. That process is this program, started again, so
//! [`handle_relaunch`] must be the first thing in `main`: in those runs it serves the
//! mounts and exits. A program with commands of its own passes a word none of them
//! uses, such as `&["sandfs"]`.
//!
//! # Before you build on it
//!
//! - sandfs is pre-1.0: a minor release (0.8 to 0.9) may change this API, and the
//!   changelog says how. Depend on one minor version, such as `sandfs = "0.8"`.
//! - Every file operation in a mount goes through the mount's process, and on macOS
//!   each write is also a round trip over localhost before it returns, so write-heavy
//!   work runs slower there than in a plain folder: compiling ripgrep from scratch
//!   took about 18 s in a mount against 13 s outside one on the same Mac (October
//!   2026).
//! - Mounting needs the default `mount` feature, and on Linux FUSE 3
//!   (`fusermount3`). Without it, and on Windows, everything but mounting works.
//!
//! `sandfs-core` is the in-memory engine underneath: sources, forks and changes, with
//! nothing on disk.

// Without a mount, mounting is stubbed out, which leaves its helpers unused.
#![cfg_attr(not(mount), allow(dead_code, unused_imports))]

mod copy;
mod folder;
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

// Keeps the README example compiling.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeExample;

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
    /// `~/.local/share/sandfs`, which needn't exist yet.
    pub fn locate() -> Result<Home> {
        Ok(Home { inner: store::Home::locate()? })
    }

    /// The home in `root`, which needn't exist yet. Mounts started from it are served
    /// with `SANDFS_HOME` set to it.
    pub fn at(root: impl AsRef<Path>) -> Result<Home> {
        Ok(Home { inner: store::Home::at(std::path::absolute(root)?) })
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

    /// Fork `from`, with its changes so far, and mount the fork at `dir` (or keep it there
    /// as a real folder, see [`ForkOptions::real_folder`]); without `dir`, keep it
    /// unmounted, as a snapshot. The source is never copied.
    ///
    /// `from` is a workspace (its name, the folder it is mounted on, or a folder in
    /// that); a git repository's working folder that sandfs doesn't serve, such as a
    /// clone, forked as its HEAD commit plus the files it changed since; or a plain
    /// folder, frozen as it is now by cloning its files into the home, which needs the
    /// folder and the home on one disk that clones files (APFS, btrfs or XFS) unless
    /// [`ForkOptions::allow_copy`] is set. A folder on another disk than the home is
    /// refused with a [`CrossDevice`] error. Later changes to `from` don't reach the
    /// fork.
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
    /// Keep a fork of a git repository's folder (or of such a fork) at `dir` as a real
    /// folder rather than a mount: `Some(true)` or `Some(false)`. By default it is one on
    /// macOS, where a mount is slowest at work on many small files, and on Linux where
    /// the folder's disk clones files into `dir` (btrfs, XFS; not ext4), unless the
    /// fork has read-only paths, which need a mount. Its files are clones of the folder's,
    /// which cost no space on a disk that clones files (APFS, btrfs, XFS) and are copied
    /// on others, and git in it is as fast as in the clone. [`Home::changes`] and
    /// [`Home::fork`] read it as they read a mount; [`Home::remove`] with `unmount`
    /// deletes it. It can't be mounted, updated, served or confined.
    pub real_folder: Option<bool>,
    /// For a plain folder whose files can't be cloned into the home, because it is on
    /// another disk or on one that doesn't clone files (such as ext4), copy them in
    /// full instead of refusing. That takes as much space as the folder, and the time
    /// to copy it; [`Home::update`] copies the same way. Files that can be cloned still
    /// are.
    pub allow_copy: bool,
}

/// [`Home::fork`] refused to freeze a plain folder, since `folder` is on another disk
/// than the home, and a file is cloned only within one disk. Set
/// [`ForkOptions::allow_copy`] to copy the files instead, or mount the folder itself
/// with [`Home::mount`], which shows its later changes too. Find it with
/// `err.downcast_ref::<sandfs::CrossDevice>()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrossDevice {
    /// The folder forked, or the folder in it that is on another disk.
    pub folder: PathBuf,
    /// The home.
    pub home: PathBuf,
}

impl std::fmt::Display for CrossDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} is on another disk than sandfs's home ({}), and forking a plain folder clones its files, \
             which works only within one disk. `sandfs fork --allow-copy` copies them in full instead; \
             `sandfs mount {} DIR` mounts the folder itself, showing its later changes too",
            self.folder.display(),
            self.home.display(),
            self.folder.display()
        )
    }
}

impl std::error::Error for CrossDevice {}

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
    /// Where it is mounted or kept, or `None` for a snapshot.
    pub dir: Option<PathBuf>,
    /// Whether it is kept as a real folder rather than mounted (see
    /// [`ForkOptions::real_folder`]).
    pub real_folder: bool,
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
