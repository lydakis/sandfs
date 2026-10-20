//! `sandfs`: mount copy-on-write workspaces over read-only sources, such as a host
//! folder.
//!
//! The commands are the core's primitives. `mount` creates a workspace over a source
//! and serves it as a directory (FUSE on Linux, NFS on macOS), `fork` mounts a copy of one without copying its source, and `changes` lists the
//! files a workspace changed. A working copy on disk is just a fork mounted at a path.
//! `serve --mcp` hands a workspace to an agent as MCP file tools instead of a mount.
//! One background process serves every mount (see `server`).

// Without a mount, mounting is stubbed out below, which leaves its helpers unused.
#![cfg_attr(not(mount), allow(dead_code, unused_imports))]

#[cfg(mount)]
mod fs;
mod git;
mod host;
mod mcp;
#[cfg(any(nfs, all(test, fuse)))]
mod nfs;
mod persist;
#[cfg(mount)]
mod server;
mod store;

use std::path::{Path, PathBuf};
#[cfg(mount)]
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use sandfs_core::{ChangeKind, VPath};

#[cfg(fuse)]
use crate::fs::SandFs;
#[cfg(mount)]
use crate::fs::{Inner, Shared};
use crate::persist::{Saved, is_object_gone};
#[cfg(mount)]
use crate::store::Bases;
use crate::store::{Base, Home, MountInfo, ROOT, check_name, is_mounted};

#[derive(Parser)]
#[command(name = "sandfs", version, about = "Lazy, copy-on-write workspaces mounted as directories")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a workspace over a source and mount it at DIR, or remount a workspace.
    ///
    /// SOURCE is a host folder, which is read lazily and never written to, or the
    /// name of an existing workspace (write ./NAME for a folder that shares a
    /// workspace's name). git:<path or url>[@rev] mounts a commit; remote
    /// repositories become partial clones, so file contents are fetched only when a
    /// directory or file is first read.
    Mount {
        source: String,
        dir: PathBuf,
        /// Name for the new workspace (default: DIR's last component).
        #[arg(long)]
        name: Option<String>,
        /// Serve in the foreground instead of in the background.
        #[arg(long, short)]
        foreground: bool,
    },
    /// Fork a workspace, with its changes so far, and mount the fork at DIR. The source
    /// is never copied; each saved change is linked into the fork.
    ///
    /// Without DIR the fork is only kept, not mounted: a snapshot of the workspace as it
    /// is now, which `mount NAME DIR` or `fork NAME DIR` brings back later.
    Fork {
        /// The workspace to fork: its name or the directory it is mounted on.
        from: String,
        dir: Option<PathBuf>,
        /// Name for the fork (default: DIR's last component; required without DIR).
        #[arg(long)]
        name: Option<String>,
        /// Serve in the foreground instead of in the background.
        #[arg(long, short)]
        foreground: bool,
    },
    /// List the files a workspace added (A), modified (M) or deleted (D) relative to its source.
    Changes {
        /// A workspace name or the directory it is mounted on.
        workspace: String,
        /// Compare against this workspace instead of the source: a fork kept as a
        /// checkpoint, or another attempt forked from the same one.
        #[arg(long, value_name = "WORKSPACE")]
        since: Option<String>,
    },
    /// List workspaces and where they are mounted.
    List,
    /// Unmount a workspace. Its changes stay saved; `mount NAME DIR` brings it back.
    Unmount {
        /// A workspace name or the directory it is mounted on.
        workspace: String,
    },
    /// Delete an unmounted workspace and its changes.
    Rm { name: String },
    /// Serve an unmounted workspace to an agent as MCP file tools over stdin and stdout,
    /// or create one over a source and serve that.
    ///
    /// The agent gets read, list, glob, grep, write, edit, move, delete and changes,
    /// with the source's files under /mnt. Every change is saved before its reply, so
    /// `changes`, `fork` and `mount` see it afterwards.
    Serve {
        /// Speak MCP (the only protocol so far).
        #[arg(long)]
        mcp: bool,
        /// A workspace name, or a source as for `mount`.
        workspace: String,
        /// Name for a new workspace (default: the source's last component).
        #[arg(long)]
        name: Option<String>,
    },
    /// Start the process that serves every mount (run by `mount` and `fork`), or with
    /// `--serve`, be that process.
    #[command(hide = true)]
    Daemon {
        #[arg(long)]
        serve: bool,
    },
    /// Tell git which files of the mounted workspace it runs in have changed. A git
    /// mount's `.git` names this as git's file monitor (hook version 2).
    #[command(name = "git-fsmonitor", hide = true)]
    GitFsmonitor {
        version: String,
        /// The token of git's last answer, which the answer doesn't depend on.
        #[arg(allow_hyphen_values = true)]
        token: Option<String>,
    },
}

fn main() {
    if let Err(err) = run(Cli::parse()) {
        eprintln!("sandfs: {err:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    let home = Home::locate()?;
    match cli.command {
        Cmd::Mount { source, dir, name, foreground } => {
            // A bare word naming a workspace remounts it; anything else is a source.
            if home.exists(&source) && name.is_none() {
                return mount(&home, &source, &dir, foreground);
            }
            let name = default_name(name, &dir)?;
            let record = home.resolve(&source)?;
            home.create(&name, &record)?;
            mount_new(&home, &name, &dir, foreground)
        }
        Cmd::Fork { from, dir, name, foreground } => {
            let from = home.find(&from)?;
            let name = match (&dir, name) {
                (Some(dir), name) => default_name(name, dir)?,
                (None, Some(name)) => default_name(Some(name), Path::new(""))?,
                (None, None) => bail!("name the snapshot with --name, or give a directory to mount the fork on"),
            };
            sync(&home, &from)?;
            let record = home.record(&from)?;
            let target = home.create(&name, &record)?;
            if let Err(err) = Saved::new(home.workspace_dir(&from)).copy_to(&target) {
                remove_unused(&home, &name);
                return Err(err);
            }
            match dir {
                Some(dir) => mount_new(&home, &name, &dir, foreground),
                None => {
                    println!("{name} forked from {from}");
                    Ok(())
                }
            }
        }
        Cmd::Changes { workspace, since } => {
            let name = home.find(&workspace)?;
            sync(&home, &name)?;
            let since = since.map(|since| home.find(&since)).transpose()?;
            if let Some(since) = &since {
                sync(&home, since)?;
            }
            // Saved files are read as needed, and a mount saving meanwhile may replace
            // one; reading again from the newer save works.
            let mut attempts = 0;
            let (opened, changes) = loop {
                let (opened, changes) = match &since {
                    Some(since) => {
                        let (opened, base) = home.open_pair(&name, since)?;
                        let changes = opened.ws.changes_since(&base.ws);
                        (opened, changes)
                    }
                    None => {
                        let opened = home.open(&name)?;
                        let changes = opened.ws.changes();
                        (opened, changes)
                    }
                };
                match changes {
                    Ok(changes) => break (opened, changes),
                    Err(err) => {
                        let err = anyhow::Error::from(err);
                        if !is_object_gone(&err) || attempts == 10 {
                            return Err(err);
                        }
                        attempts += 1;
                    }
                }
            };
            let root = VPath::parse(ROOT)?;
            // What git keeps in a git mount's `.git` would drown out the files themselves.
            let git_dir = matches!(opened.record.base, Base::Git { .. }).then(|| VPath::parse(&format!("{ROOT}/.git")));
            let git_dir = git_dir.transpose()?;
            for change in changes {
                if git_dir.as_ref().is_some_and(|dir| change.path.starts_with(dir)) {
                    continue;
                }
                let marker = match change.kind {
                    ChangeKind::Added => 'A',
                    ChangeKind::Modified => 'M',
                    ChangeKind::Deleted => 'D',
                };
                let rel = change.path.strip_prefix(&root).unwrap_or_default().join("/");
                println!("{marker} {rel}");
            }
            Ok(())
        }
        Cmd::List => {
            for name in home.names()? {
                let record = home.record(&name)?;
                let mounted = home.mount_info(&name).map(|info| info.mountpoint.display().to_string());
                println!("{name}\t{}\t{}", record.source, mounted.as_deref().unwrap_or("-"));
            }
            Ok(())
        }
        Cmd::Unmount { workspace } => {
            let name = home.find(&workspace)?;
            let info = home.attached_mount(&name).with_context(|| format!("{name} is not mounted"))?;
            unmount(&home, &name, &info)
        }
        Cmd::Rm { name } => {
            if home.attached_mount(&name).is_some() {
                bail!("{name} is mounted; unmount it first");
            }
            home.record(&name)?;
            let _writer = home.lock(&name, "sandfs rm")?;
            home.remove(&name)
        }
        Cmd::Serve { mcp, workspace, name } => {
            if !mcp {
                bail!("say what to serve: sandfs serve --mcp {workspace}");
            }
            // As with `mount`, a bare word naming a workspace serves it; anything else
            // is a source. This is how workspaces are made where nothing mounts.
            if home.exists(&workspace) && name.is_none() {
                return mcp::serve(&home, &workspace);
            }
            // Named after the resolved folder or repository, so `.` and `repo@v1` work.
            let record = home.resolve(&workspace)?;
            let name = default_name(name, &source_dir(&record.base))?;
            home.create(&name, &record)?;
            mcp::serve(&home, &name)
        }
        Cmd::Daemon { serve } => serve_mounts(home, serve),
        Cmd::GitFsmonitor { version, token: _ } => {
            // Git runs its file monitor at the root of the checkout: the mount.
            let root = std::env::current_dir()?;
            let name = home.find(root.to_str().context("non-UTF-8 mount path")?)?;
            sync(&home, &name)?;
            git::fsmonitor(&Saved::new(home.workspace_dir(&name)).paths()?, &version)
        }
    }
}

/// The folder a source's name comes from: `data` for `/srv/data`, `/srv/data/.git` or a
/// cached `github.com/team/data.git`.
fn source_dir(base: &Base) -> PathBuf {
    match base {
        Base::Dir { path } => path.clone(),
        Base::Git { repo, .. } if repo.file_name().is_some_and(|name| name == ".git") => {
            repo.parent().map_or_else(|| repo.clone(), Path::to_path_buf)
        }
        Base::Git { repo, .. } if repo.extension().is_some_and(|ext| ext == "git") => repo.with_extension(""),
        Base::Git { repo, .. } => repo.clone(),
    }
}

fn default_name(name: Option<String>, dir: &Path) -> Result<String> {
    let name = match name {
        Some(name) => name,
        None => dir.file_name().and_then(|n| n.to_str()).context("give the workspace a --name")?.to_owned(),
    };
    check_name(&name)?;
    Ok(name)
}

/// Ask a mounted workspace to save everything written so far. `fsync` on a FUSE
/// mount's root directory makes the serving process save before it returns; an NFS
/// mount's process saves when asked on its save port, since `fsync` never reaches it.
fn sync(home: &Home, name: &str) -> Result<()> {
    let Some(info) = home.mount_info(name) else { return Ok(()) };
    // A dead server's pid may belong to another process by now. Only the process holding
    // the workspace serves it; without one, the last save is all there is to read.
    #[cfg(mount)]
    if !home.locked_by(name, info.pid) {
        return Ok(());
    }
    let saved = match (info.save_port, info.save_secret) {
        #[cfg(nfs)]
        (Some(port), Some(secret)) => nfs::request_save(port, &secret),
        _ => std::fs::File::open(&info.mountpoint).and_then(|dir| dir.sync_all()).map_err(anyhow::Error::from),
    };
    saved.with_context(|| format!("saving {name} before reading it"))
}

/// Mount a workspace that was just created, removing it again if the mount fails so
/// the same command can simply be retried.
fn mount_new(home: &Home, name: &str, dir: &Path, foreground: bool) -> Result<()> {
    // A fork starts with a copy of its parent's changes.json. Every mount saves when it
    // ends, so an index that is still the initial one means the mount never ran.
    let index = home.workspace_dir(name).join("changes.json");
    let initial = std::fs::read(&index).ok();
    let result = mount(home, name, dir, foreground);
    if result.is_err() && std::fs::read(&index).ok() == initial {
        remove_unused(home, name);
    }
    result
}

/// Remove a workspace that was just created, unless another process has started
/// serving it meanwhile.
fn remove_unused(home: &Home, name: &str) {
    if let Ok(_writer) = home.lock(name, "sandfs, removing a workspace whose mount failed") {
        let _ = home.remove(name);
    }
}

/// Mount `name` at `dir`: serve it here with `foreground`, otherwise start a
/// background process and wait until the mount is up.
#[cfg(mount)]
fn mount(home: &Home, name: &str, dir: &Path, foreground: bool) -> Result<()> {
    if let Some(info) = home.attached_mount(name) {
        let at = info.mountpoint.display();
        if host::alive(info.pid) {
            bail!("{name} is already mounted at {at}");
        }
        // Mounting again would forget where the old mount is, and nothing could detach it.
        bail!("{name} is still attached at {at}, whose process has exited; `sandfs unmount {name}` detaches it");
    }
    // The source is read through its host path, so a mount on top of it or inside it
    // would read itself (and create the mount point inside a read-only source).
    if let Base::Dir { path } = home.record(name)?.base {
        // Opening the source canonicalizes its path again, so compare with that.
        let path = path.canonicalize().unwrap_or(path);
        let target = resolve_new(dir)?;
        if target.starts_with(&path) || path.starts_with(&target) {
            bail!("{} overlaps the source folder {}; mount it somewhere else", target.display(), path.display());
        }
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let dir = dir.canonicalize()?;
    if is_mounted(&dir) {
        bail!("something is already mounted at {}", dir.display());
    }
    if foreground {
        return serve(home, name, &dir, &Arc::default(), || {});
    }
    server::mount(home, name, &dir)?;
    println!("{name} mounted at {}", dir.display());
    Ok(())
}

/// Serve every mount asked of this home's background process (see `server`).
#[cfg(mount)]
fn serve_mounts(home: Home, serve: bool) -> Result<()> {
    if serve { server::run(home) } else { server::detach() }
}

/// The canonical path `dir` will have once created: its nearest existing ancestor,
/// resolved, plus the components that do not exist yet.
fn resolve_new(dir: &Path) -> Result<PathBuf> {
    let absolute = std::path::absolute(dir)?;
    let mut rest = Vec::new();
    let mut existing = absolute.as_path();
    while !existing.exists() {
        rest.push(existing.file_name().context("no existing parent directory")?);
        existing = existing.parent().context("no existing parent directory")?;
    }
    let mut resolved = existing.canonicalize()?;
    resolved.extend(rest.into_iter().rev());
    Ok(resolved)
}

/// Serve `name` at `dir` until it is unmounted, then save its changes. `ready` is called
/// once the mount is up, and never if mounting fails. Workspaces over a commit share it
/// through `bases` with the others this process serves.
#[cfg(mount)]
pub(crate) fn serve(home: &Home, name: &str, dir: &Path, bases: &Arc<Bases>, ready: impl FnOnce()) -> Result<()> {
    let pid = std::process::id();
    let _writer = home.lock(name, &format!("the mount at {} (process {pid})", dir.display()))?;
    // Two workspaces mounted on one directory at once would stack. Locking the directory
    // itself (the one underneath, which a mount covers) lets only one mount proceed;
    // whoever comes after it sees the mount.
    let point = std::fs::File::open(dir).with_context(|| format!("opening {}", dir.display()))?;
    if point.try_lock().is_err() || is_mounted(dir) {
        bail!("something is already mounted at {}", dir.display());
    }
    // A failed final save is reported by the `unmount` that asked for it, not a later one.
    let save_error = home.workspace_dir(name).join(SAVE_ERROR);
    let _ = std::fs::remove_file(&save_error);
    let (mut opened, _base) = home.open_shared(name, bases)?;
    opened.saved.collect_garbage();
    let owner = std::os::unix::fs::MetadataExt::uid(&std::fs::metadata(dir)?);
    let group = std::os::unix::fs::MetadataExt::gid(&std::fs::metadata(dir)?);
    let base_time = UNIX_EPOCH + Duration::from_secs(opened.record.created);
    let root = VPath::parse(ROOT)?;
    let mut inner =
        Inner::new(opened.ws, &root, opened.restored.times, opened.restored.dirs, base_time, (owner, group));
    inner.ranged_reads = matches!(opened.record.base, Base::Dir { .. });
    inner.drafts = Some(opened.saved.drafts());
    // NFS has no close to flush a file's writes, and macOS's NFS client adds files of
    // its own next to the ones people write.
    inner.keep_removed = !cfg!(nfs);
    inner.skip_bookkeeping = cfg!(nfs);
    let shared = Shared::new(inner, opened.saved, root);
    let saver = shared.spawn_saver();

    // A mount that never came up has nothing to save, and saving would make a new
    // workspace look used to the `mount_new` that removes it again.
    let was_mounted = std::cell::Cell::new(false);
    let mut ready = Some(ready);
    let mut mounted = |save: Option<(u16, String)>| -> Result<()> {
        let (save_port, save_secret) = save.unzip();
        let info = MountInfo { pid, mountpoint: dir.to_path_buf(), save_port, save_secret, id: mount_id() };
        home.set_mount_info(name, Some(&info))?;
        was_mounted.set(true);
        eprintln!("sandfs: serving {name} at {} since {:?}", dir.display(), SystemTime::now());
        if let Some(ready) = ready.take() {
            ready();
        }
        Ok(())
    };
    #[cfg(fuse)]
    let result = {
        let mut config = fuser::Config::default();
        config.mount_options = host::mount_options(name);
        let session = fuser::Session::new(SandFs(shared.clone()), dir, &config).context("mounting with FUSE")?;
        mounted(None)?;
        session.run().map_err(anyhow::Error::from)
    };
    #[cfg(nfs)]
    let result = nfs::run(name, dir, &shared, |port, secret| mounted(Some((port, secret))));

    shared.stop();
    let _ = saver.join();
    if !was_mounted.get() {
        return result.context("mounting");
    }
    let saved = shared.save(true);
    match &saved {
        Ok(()) => home.set_mount_info(name, None)?,
        Err(err) => {
            // `unmount` is waiting for mount.json to go. A full disk may refuse a new
            // file but not a rename, so the failure is signalled by moving mount.json
            // to save-error, and the reason is added if it can be.
            let _ = std::fs::rename(home.workspace_dir(name).join("mount.json"), &save_error);
            let _ = std::fs::write(&save_error, format!("{err:#}"));
        }
    }
    result.context("serving the mount")?;
    saved.context("saving changes at unmount")
}

/// A number for a new mount that differs from the ones before it.
#[cfg(mount)]
fn mount_id() -> u64 {
    static COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos() as u64;
    now ^ COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Where a mount's process leaves the error of a final save that failed.
#[cfg(mount)]
const SAVE_ERROR: &str = "save-error";

/// Unmount `name` and wait for the serving process to save, which it signals by
/// removing mount.json, and to let go of the workspace.
#[cfg(mount)]
fn unmount(home: &Home, name: &str, info: &MountInfo) -> Result<()> {
    let state = home.workspace_dir(name);
    // One process serves every mount, so the workspace may be mounted again by it while
    // this waits; mount.json's id says whether the mount recorded is still this one.
    let recorded = || home.recorded_mount(name).map(|current| current.id);
    let replaced = || recorded().is_some_and(|id| id != info.id);
    // The serving process holds the workspace; checking that too means a process that
    // has since taken over a dead server's pid isn't waited for.
    let serving = || host::alive(info.pid) && home.locked_by(name, info.pid) && !replaced();
    host::unmount(&info.mountpoint, !serving())?;
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut wait = Duration::from_millis(1);
    while recorded() == Some(info.id) && serving() {
        if Instant::now() >= deadline {
            bail!("unmounted, but process {} is still saving the workspace's changes", info.pid);
        }
        std::thread::sleep(wait);
        wait = (wait * 2).min(Duration::from_millis(50));
    }
    if recorded() == Some(info.id) {
        // The serving process died without cleaning up; its last save is what remains.
        eprintln!("sandfs: the mount's process had exited; changes since its last save are lost");
        let _ = std::fs::remove_file(state.join("mount.json"));
    }
    // The process lets go of the workspace just after removing mount.json. Wait for
    // that too, so a command right after this one finds the workspace free.
    while serving() {
        if Instant::now() >= deadline {
            bail!("unmounted, but process {} has not let go of {name}", info.pid);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    if let Ok(error) = std::fs::read_to_string(state.join(SAVE_ERROR)) {
        let _ = std::fs::remove_file(state.join(SAVE_ERROR));
        // Still the moved mount.json (or empty) when the reason could not be written.
        let reason = if error.trim().is_empty() || error.starts_with('{') {
            format!("see {}", server::log_path(home).display())
        } else {
            error
        };
        bail!("unmounted, but saving the last changes failed, so changes since the last save are lost: {reason}");
    }
    Ok(())
}

// Without a mount (Windows, or a build without the `mount` feature) everything works
// but mounting; `serve --mcp SOURCE` creates workspaces instead.
#[cfg(not(mount))]
const NO_MOUNT: &str =
    "this build of sandfs can't mount (that needs Linux or macOS); `sandfs serve --mcp SOURCE` works here";

#[cfg(not(mount))]
fn mount(_home: &Home, _name: &str, _dir: &Path, _foreground: bool) -> Result<()> {
    bail!(NO_MOUNT)
}

#[cfg(not(mount))]
fn serve_mounts(_home: Home, _serve: bool) -> Result<()> {
    bail!(NO_MOUNT)
}

#[cfg(not(mount))]
fn unmount(_home: &Home, _name: &str, _info: &MountInfo) -> Result<()> {
    bail!(NO_MOUNT)
}
