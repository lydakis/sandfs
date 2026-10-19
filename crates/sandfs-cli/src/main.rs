//! `sandfs`: mount copy-on-write workspaces over read-only sources, such as a host
//! folder.
//!
//! The commands are the core's primitives. `mount` creates a workspace over a source
//! and serves it as a directory (FUSE on Linux, NFS on macOS), `fork` mounts a copy of one without copying its source, and `changes` lists the
//! files a workspace changed. A working copy on disk is just a fork mounted at a path.
//! `serve --mcp` hands a workspace to an agent as MCP file tools instead of a mount.

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
mod store;

#[cfg(mount)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use sandfs_core::{ChangeKind, VPath};

#[cfg(fuse)]
use crate::fs::SandFs;
#[cfg(mount)]
use crate::fs::{Inner, Shared};
use crate::persist::Saved;
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
    Fork {
        /// The workspace to fork: its name or the directory it is mounted on.
        from: String,
        dir: PathBuf,
        /// Name for the fork (default: DIR's last component).
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
    /// Serve a workspace's mount in this process (used by `mount` and `fork`).
    #[command(hide = true)]
    Daemon { name: String, dir: PathBuf },
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
            let name = default_name(name, &dir)?;
            sync(&home, &from)?;
            let record = home.record(&from)?;
            let target = home.create(&name, &record)?;
            if let Err(err) = Saved::new(home.workspace_dir(&from)).copy_to(&target) {
                remove_unused(&home, &name);
                return Err(err);
            }
            mount_new(&home, &name, &dir, foreground)
        }
        Cmd::Changes { workspace } => {
            let name = home.find(&workspace)?;
            sync(&home, &name)?;
            let opened = home.open(&name)?;
            let root = VPath::parse(ROOT)?;
            for change in opened.ws.changes()? {
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
        Cmd::Daemon { name, dir } => serve(&home, &name, &dir),
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
    let saved = match info.save_port {
        #[cfg(nfs)]
        Some(port) => nfs::request_save(port),
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
    if let Some(info) = home.mount_info(name) {
        bail!("{name} is already mounted at {}", info.mountpoint.display());
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
        return serve(home, name, &dir);
    }

    let log_path = home.workspace_dir(name).join("mount.log");
    let log = std::fs::OpenOptions::new().create(true).append(true).open(&log_path)?;
    let mut child = Command::new(std::env::current_exe()?)
        .arg("daemon")
        .arg(name)
        .arg(&dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .process_group(0) // keep running when the terminal goes away
        .spawn()
        .context("starting the mount process")?;
    // Opening the workspace can fetch from a remote, so allow it some time.
    let deadline = Instant::now() + Duration::from_secs(120);
    // Most mounts are up within a few milliseconds, so poll quickly at first.
    let mut wait = Duration::from_millis(1);
    while Instant::now() < deadline {
        // The kernel shows the mount slightly before the daemon records it.
        if home.mount_info(name).is_some_and(|info| info.pid == child.id()) {
            println!("{name} mounted at {}", dir.display());
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            let log = std::fs::read_to_string(&log_path).unwrap_or_default();
            let tail: Vec<&str> = log.lines().rev().take(5).collect();
            let tail: Vec<&str> = tail.into_iter().rev().collect();
            bail!("mounting failed ({status}):\n{}", tail.join("\n"));
        }
        std::thread::sleep(wait);
        wait = (wait * 2).min(Duration::from_millis(50));
    }
    let _ = child.kill();
    bail!("timed out waiting for the mount at {}", dir.display())
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

/// Serve `name` at `dir` until it is unmounted, then save its changes.
#[cfg(mount)]
fn serve(home: &Home, name: &str, dir: &Path) -> Result<()> {
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
    let mut opened = home.open(name)?;
    opened.saved.collect_garbage();
    let owner = std::os::unix::fs::MetadataExt::uid(&std::fs::metadata(dir)?);
    let group = std::os::unix::fs::MetadataExt::gid(&std::fs::metadata(dir)?);
    let base_time = UNIX_EPOCH + Duration::from_secs(opened.record.created);
    let root = VPath::parse(ROOT)?;
    let mut inner =
        Inner::new(opened.ws, &root, opened.restored.times, opened.restored.dirs, base_time, (owner, group));
    inner.ranged_reads = matches!(opened.record.base, Base::Dir { .. });
    // NFS has no close to flush a file's writes, and macOS's NFS client adds files of
    // its own next to the ones people write.
    inner.keep_removed = !cfg!(nfs);
    inner.skip_bookkeeping = cfg!(nfs);
    let shared = Shared::new(inner, opened.saved, root);
    let saver = shared.spawn_saver();

    // A mount that never came up has nothing to save, and saving would make a new
    // workspace look used to the `mount_new` that removes it again.
    let was_mounted = std::cell::Cell::new(false);
    let mounted = |save_port: Option<u16>| -> Result<()> {
        home.set_mount_info(name, Some(&MountInfo { pid, mountpoint: dir.to_path_buf(), save_port }))?;
        was_mounted.set(true);
        eprintln!("sandfs: serving {name} at {} since {:?}", dir.display(), SystemTime::now());
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
    let result = nfs::run(name, dir, &shared, |port| mounted(Some(port)));

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

/// Where a mount's process leaves the error of a final save that failed.
#[cfg(mount)]
const SAVE_ERROR: &str = "save-error";

/// Unmount `name` and wait for the serving process to save, which it signals by
/// removing mount.json, and to let go of the workspace.
#[cfg(mount)]
fn unmount(home: &Home, name: &str, info: &MountInfo) -> Result<()> {
    let state = home.workspace_dir(name);
    let mount_file = state.join("mount.json");
    host::unmount(&info.mountpoint, !host::alive(info.pid))?;
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut wait = Duration::from_millis(1);
    while mount_file.exists() && host::alive(info.pid) {
        if Instant::now() >= deadline {
            bail!("unmounted, but process {} is still saving the workspace's changes", info.pid);
        }
        std::thread::sleep(wait);
        wait = (wait * 2).min(Duration::from_millis(50));
    }
    if mount_file.exists() {
        // The serving process died without cleaning up; its last save is what remains.
        eprintln!("sandfs: the mount's process had exited; changes since its last save are lost");
        let _ = std::fs::remove_file(&mount_file);
    }
    // The process lets go of the workspace as it exits, just after removing mount.json.
    // Wait for that too, so a command right after this one finds the workspace free.
    while host::alive(info.pid) && home.lock(name, "sandfs unmount").is_err() {
        if Instant::now() >= deadline {
            bail!("unmounted, but process {} has not exited", info.pid);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    if let Ok(error) = std::fs::read_to_string(state.join(SAVE_ERROR)) {
        let _ = std::fs::remove_file(state.join(SAVE_ERROR));
        // Still the moved mount.json (or empty) when the reason could not be written.
        let reason = if error.trim().is_empty() || error.starts_with('{') {
            format!("see {}", state.join("mount.log").display())
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
fn serve(_home: &Home, _name: &str, _dir: &Path) -> Result<()> {
    bail!(NO_MOUNT)
}

#[cfg(not(mount))]
fn unmount(_home: &Home, _name: &str, _info: &MountInfo) -> Result<()> {
    bail!(NO_MOUNT)
}
