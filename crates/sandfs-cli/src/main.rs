//! `sandfs`: mount copy-on-write workspaces over read-only sources, such as a host
//! folder.
//!
//! The commands are the core's primitives. `mount` creates a workspace over a source
//! and serves it with FUSE, `fork` mounts a copy of one without copying its source, and `changes` lists the
//! files a workspace changed. A working copy on disk is just a fork mounted at a path.
//! `serve --mcp` hands a workspace to an agent as MCP file tools instead of a mount.

mod fs;
mod git;
mod mcp;
mod persist;
mod store;

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use sandfs_core::{ChangeKind, VPath};

use crate::fs::{Inner, SandFs, Shared};
use crate::persist::Saved;
use crate::store::{Base, Home, MountInfo, ROOT, check_name, is_mounted};

#[derive(Parser)]
#[command(name = "sandfs", version, about = "Lazy, copy-on-write workspaces mounted with FUSE")]
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
    /// Serve an unmounted workspace to an agent as MCP file tools over stdin and stdout.
    ///
    /// The agent gets read, list, glob, grep, write, edit, move, delete and changes,
    /// with the source's files under /mnt. Every change is saved before its reply, so
    /// `changes`, `fork` and `mount` see it afterwards.
    Serve {
        /// Speak MCP (the only protocol so far).
        #[arg(long)]
        mcp: bool,
        /// A workspace name.
        workspace: String,
    },
    /// Serve a workspace's FUSE mount in this process (used by `mount` and `fork`).
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
                let _ = home.remove(&name);
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
            unmount(&info, &home.workspace_dir(&name).join("mount.json"))
        }
        Cmd::Rm { name } => {
            if home.attached_mount(&name).is_some() {
                bail!("{name} is mounted; unmount it first");
            }
            home.record(&name)?;
            home.remove(&name)
        }
        Cmd::Serve { mcp, workspace } => {
            if !mcp {
                bail!("say what to serve: sandfs serve --mcp {workspace}");
            }
            mcp::serve(&home, &home.find(&workspace)?)
        }
        Cmd::Daemon { name, dir } => serve(&home, &name, &dir),
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

/// Ask a mounted workspace to save everything written so far. `fsync` on the mount's
/// root directory makes the serving process save before it returns.
fn sync(home: &Home, name: &str) -> Result<()> {
    if let Some(info) = home.mount_info(name) {
        std::fs::File::open(&info.mountpoint)
            .and_then(|dir| dir.sync_all())
            .with_context(|| format!("saving {name} before reading it"))?;
    }
    Ok(())
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
        let _ = home.remove(name);
    }
    result
}

/// Mount `name` at `dir`: serve it here with `foreground`, otherwise start a
/// background process and wait until the mount is up.
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
        std::thread::sleep(Duration::from_millis(50));
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
fn serve(home: &Home, name: &str, dir: &Path) -> Result<()> {
    let mut opened = home.open(name)?;
    opened.saved.collect_garbage();
    let owner = std::os::unix::fs::MetadataExt::uid(&std::fs::metadata(dir)?);
    let group = std::os::unix::fs::MetadataExt::gid(&std::fs::metadata(dir)?);
    let base_time = UNIX_EPOCH + Duration::from_secs(opened.record.created);
    let root = VPath::parse(ROOT)?;
    let mut inner =
        Inner::new(opened.ws, &root, opened.restored.times, opened.restored.dirs, base_time, (owner, group));
    inner.ranged_reads = matches!(opened.record.base, Base::Dir { .. });
    let shared = Shared::new(inner, opened.saved, root);
    let saver = shared.spawn_saver();

    let mut config = fuser::Config::default();
    config.mount_options =
        vec![fuser::MountOption::FSName(format!("sandfs:{name}")), fuser::MountOption::Subtype("sandfs".to_owned())];
    let session = fuser::Session::new(SandFs(shared.clone()), dir, &config).context("mounting with FUSE")?;
    home.set_mount_info(name, Some(&MountInfo { pid: std::process::id(), mountpoint: dir.to_path_buf() }))?;
    eprintln!("sandfs: serving {name} at {} since {:?}", dir.display(), SystemTime::now());
    let result = session.run();

    shared.stop();
    let _ = saver.join();
    let saved = shared.save(true);
    home.set_mount_info(name, None)?;
    result.context("serving the mount")?;
    saved.context("saving changes at unmount")
}

/// Unmount and wait for the serving process to save, which it signals by removing
/// `mount_file`.
fn unmount(info: &MountInfo, mount_file: &Path) -> Result<()> {
    let ran = ["fusermount3", "fusermount"]
        .iter()
        .find_map(|tool| Command::new(tool).arg("-u").arg(&info.mountpoint).status().ok());
    match ran {
        Some(status) if status.success() => {}
        Some(status) => bail!("unmounting {} failed ({status})", info.mountpoint.display()),
        None => bail!("fusermount3 is not installed; install fuse3"),
    }
    let proc = PathBuf::from(format!("/proc/{}", info.pid));
    let deadline = Instant::now() + Duration::from_secs(60);
    while mount_file.exists() && proc.exists() {
        if Instant::now() >= deadline {
            bail!("unmounted, but process {} is still saving the workspace's changes", info.pid);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if mount_file.exists() {
        // The serving process died without cleaning up; its last save is what remains.
        eprintln!("sandfs: the mount's process had exited; changes since its last save are lost");
        let _ = std::fs::remove_file(mount_file);
    }
    Ok(())
}
