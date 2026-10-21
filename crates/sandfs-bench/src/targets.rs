//! The filesystems being compared, behind one small interface.
//!
//! A [`Target`] opens a fresh [`View`] of the data set: a new workspace, a new overlay
//! mount, or (for the host) the directory itself or a copy of it. Workloads only use
//! paths relative to the data set's root, so the same workload runs everywhere.

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result, bail};
use sandfs_core::{DirSource, Workspace};
use tempfile::TempDir;

/// Something that can serve the data set.
pub trait Target {
    /// Column name in the results.
    fn name(&self) -> &'static str;
    /// A fresh view of the data set. Writable views may be written and forked.
    fn open(&self, writable: bool) -> Result<Box<dyn View>>;
    /// Processes serving this target's views, whose memory counts toward its own.
    fn helper_pids(&self) -> Vec<u32> {
        Vec::new()
    }
}

/// One view of the data set, addressed by paths relative to its root.
pub trait View: Send {
    /// Read a whole file; returns its length.
    fn read(&self, path: &str) -> Result<usize>;
    /// Read a whole file.
    fn contents(&self, path: &str) -> Result<Vec<u8>>;
    /// The directory this view is, for targets that are real directories.
    fn root_dir(&self) -> Option<&Path> {
        None
    }
    /// Read part of a file; returns the bytes read.
    fn read_range(&self, path: &str, offset: u64, len: u64) -> Result<usize>;
    /// A file's size.
    fn stat(&self, path: &str) -> Result<u64>;
    /// A directory's entries as `(name, is_dir)`.
    fn list(&self, path: &str) -> Result<Vec<(String, bool)>>;
    /// Create or replace a file.
    fn write(&mut self, path: &str, data: &[u8]) -> Result<()>;
    /// Delete a file.
    fn remove(&mut self, path: &str) -> Result<()>;
    /// Move a file to a new path.
    fn rename(&mut self, from: &str, to: &str) -> Result<()>;
    /// An independent copy that starts with this view's changes.
    fn fork(&mut self) -> Result<Box<dyn View>>;
    /// The number of files added, modified or deleted relative to the data set.
    fn changes(&mut self) -> Result<usize>;
}

// ----------------------------------------------------------------------
// sandfs through the Rust API
// ----------------------------------------------------------------------

/// A [`Workspace`] with the data set mounted from a [`DirSource`] at `/data`.
pub struct Sandfs {
    pub data: PathBuf,
}

/// A workspace with what it reads from mounted at `/data`, and whatever must outlive
/// it, such as the directory its source reads.
struct SandfsView(Workspace, Option<Arc<TempDir>>);

/// A view of `ws`, whose data is mounted at `/data`, keeping `dir` until the view and
/// its forks are gone.
pub fn workspace_view(ws: Workspace, dir: Option<TempDir>) -> Box<dyn View> {
    Box::new(SandfsView(ws, dir.map(Arc::new)))
}

fn ws_path(path: &str) -> String {
    format!("/data/{path}")
}

impl Target for Sandfs {
    fn name(&self) -> &'static str {
        "sandfs"
    }

    fn open(&self, _writable: bool) -> Result<Box<dyn View>> {
        let mut ws = Workspace::new();
        ws.mount("/data", Arc::new(DirSource::new(&self.data)?))?;
        Ok(workspace_view(ws, None))
    }
}

impl View for SandfsView {
    fn read(&self, path: &str) -> Result<usize> {
        Ok(self.0.read(&ws_path(path))?.len())
    }

    fn contents(&self, path: &str) -> Result<Vec<u8>> {
        Ok(self.0.read(&ws_path(path))?.to_vec())
    }

    fn read_range(&self, path: &str, offset: u64, len: u64) -> Result<usize> {
        Ok(self.0.read_range(&ws_path(path), offset, len)?.len())
    }

    fn stat(&self, path: &str) -> Result<u64> {
        Ok(self.0.stat(&ws_path(path))?.size)
    }

    fn list(&self, path: &str) -> Result<Vec<(String, bool)>> {
        Ok(self.0.list(&ws_path(path))?.into_iter().map(|entry| (entry.name, entry.kind.is_dir())).collect())
    }

    fn write(&mut self, path: &str, data: &[u8]) -> Result<()> {
        self.0.write(&ws_path(path), data.to_vec())?;
        Ok(())
    }

    fn remove(&mut self, path: &str) -> Result<()> {
        Ok(self.0.remove(&ws_path(path), false)?)
    }

    fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        Ok(self.0.rename(&ws_path(from), &ws_path(to))?)
    }

    fn fork(&mut self) -> Result<Box<dyn View>> {
        Ok(Box::new(SandfsView(self.0.fork(), self.1.clone())))
    }

    fn changes(&mut self) -> Result<usize> {
        Ok(self.0.changes()?.len())
    }
}

// ----------------------------------------------------------------------
// Targets that are real directories: the host, overlayfs and the FUSE mount
// ----------------------------------------------------------------------

/// The data set read and written directly. Forking copies the tree, and finding the
/// changes compares the copy with the original file by file: what a harness does
/// without an overlay.
pub struct Host {
    pub data: PathBuf,
    pub scratch: PathBuf,
}

impl Target for Host {
    fn name(&self) -> &'static str {
        "host"
    }

    fn open(&self, writable: bool) -> Result<Box<dyn View>> {
        if !writable {
            return Ok(Box::new(DirView { root: self.data.clone(), backend: Backend::HostOriginal }));
        }
        // A writable view is a copy of the whole data set, made untimed.
        let copy = tempfile::Builder::new().prefix("host-").tempdir_in(&self.scratch)?;
        copy_tree(&self.data, copy.path())?;
        Ok(Box::new(DirView {
            root: copy.path().to_owned(),
            backend: Backend::HostCopy { original: self.data.clone(), scratch: self.scratch.clone(), _copy: copy },
        }))
    }
}

/// Linux overlayfs with the data set as its lower layer: the kernel's copy-on-write
/// filesystem. Forking copies the upper layer into a new mount; the changes are the
/// upper layer's files and whiteouts. Needs root.
pub struct Overlay {
    pub data: PathBuf,
    pub scratch: PathBuf,
}

impl Overlay {
    /// True if this process can mount overlayfs.
    pub fn available(scratch: &Path) -> bool {
        let Ok(probe) = tempfile::tempdir_in(scratch) else { return false };
        let lower = probe.path().join("lower");
        if fs::create_dir(&lower).is_err() {
            return false;
        }
        let Ok(dirs) = OverlayDirs::new(probe.path()) else { return false };
        let mounted = dirs.mount(&lower).is_ok();
        if mounted {
            let _ = umount(&dirs.merged);
        }
        mounted
    }
}

struct OverlayDirs {
    upper: PathBuf,
    work: PathBuf,
    merged: PathBuf,
}

impl OverlayDirs {
    fn new(base: &Path) -> Result<Self> {
        let dirs = OverlayDirs { upper: base.join("upper"), work: base.join("work"), merged: base.join("merged") };
        for dir in [&dirs.work, &dirs.merged] {
            fs::create_dir_all(dir)?;
        }
        if !dirs.upper.exists() {
            fs::create_dir(&dirs.upper)?;
        }
        Ok(dirs)
    }

    fn mount(&self, lower: &Path) -> Result<()> {
        let options =
            format!("lowerdir={},upperdir={},workdir={}", lower.display(), self.upper.display(), self.work.display());
        run(Command::new("mount").args(["-t", "overlay", "overlay", "-o", &options]).arg(&self.merged))
    }
}

impl Overlay {
    fn mount_view(&self, upper_from: Option<&Path>) -> Result<Box<dyn View>> {
        let dir = tempfile::Builder::new().prefix("overlay-").tempdir_in(&self.scratch)?;
        if let Some(from) = upper_from {
            // `cp -a` keeps whiteouts (device nodes) and the opaque-directory xattrs.
            run(Command::new("cp").arg("-a").arg(from).arg(dir.path().join("upper")))?;
        }
        let dirs = OverlayDirs::new(dir.path())?;
        dirs.mount(&self.data)?;
        Ok(Box::new(DirView {
            root: dirs.merged.clone(),
            backend: Backend::Overlay {
                upper: dirs.upper,
                data: self.data.clone(),
                scratch: self.scratch.clone(),
                _dir: dir,
            },
        }))
    }
}

impl Target for Overlay {
    fn name(&self) -> &'static str {
        "overlayfs"
    }

    fn open(&self, _writable: bool) -> Result<Box<dyn View>> {
        self.mount_view(None)
    }
}

/// The `sandfs` command's mount over the data set as a host folder: FUSE on Linux, NFS
/// on localhost on macOS. Forking runs `sandfs fork` and the changes come from `sandfs
/// changes`, both as separate processes, the way a harness would call them.
#[derive(Clone)]
pub struct Fuse {
    data: PathBuf,
    scratch: PathBuf,
    bin: PathBuf,
    /// `SANDFS_HOME`, shared by every view; removed when the last one goes.
    home: Arc<TempDir>,
    next: Arc<AtomicUsize>,
}

impl Fuse {
    /// A FUSE target using the `sandfs` binary at `bin`, if FUSE is usable here.
    pub fn new(data: PathBuf, scratch: PathBuf, bin: PathBuf) -> Result<Fuse> {
        // macOS mounts over NFS on localhost and needs nothing installed.
        if cfg!(target_os = "linux") {
            if !Path::new("/dev/fuse").exists() {
                bail!("/dev/fuse is missing");
            }
            if Command::new("fusermount3").arg("-V").stdout(Stdio::null()).status().is_err() {
                bail!("fusermount3 is not installed");
            }
        }
        if !bin.is_file() {
            bail!("{} not found; build it with `cargo build --release -p sandfs-cli`", bin.display());
        }
        let home = tempfile::Builder::new().prefix("sandfs-home-").tempdir_in(&scratch)?;
        let fuse = Fuse { data, scratch, bin, home: Arc::new(home), next: Arc::new(AtomicUsize::new(0)) };
        // Mount once and throw it away: the device and helper can exist while this
        // process is still not allowed to mount.
        let probe = tempfile::Builder::new().prefix("fuse-probe-").tempdir_in(&fuse.scratch)?;
        drop(fuse.open_dir(probe.path()).context("cannot mount here")?);
        Ok(fuse)
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(&self.bin);
        cmd.env("SANDFS_HOME", self.home.path());
        cmd
    }

    fn fresh_mountpoint(&self) -> (String, PathBuf) {
        let name = format!("w{}", self.next.fetch_add(1, Ordering::Relaxed));
        let dir = self.scratch.join(format!("fuse-{}-{name}", std::process::id()));
        (name, dir)
    }

    fn view(&self, name: String, mountpoint: PathBuf) -> Box<dyn View> {
        Box::new(DirView { root: mountpoint.clone(), backend: Backend::Fuse { fuse: self.clone(), name, mountpoint } })
    }

    /// Mount a new workspace over the data set at a fresh mountpoint.
    fn open_dir(&self, source: &Path) -> Result<Box<dyn View>> {
        let (name, dir) = self.fresh_mountpoint();
        run(self.command().arg("mount").arg(source).arg(&dir).args(["--name", &name]))?;
        Ok(self.view(name, dir))
    }
}

impl Target for Fuse {
    fn name(&self) -> &'static str {
        // Named by how the mount is served, so Linux and macOS runs aren't compared.
        if cfg!(target_os = "macos") { "sandfs NFS" } else { "sandfs FUSE" }
    }

    fn open(&self, _writable: bool) -> Result<Box<dyn View>> {
        self.open_dir(&self.data)
    }

    fn helper_pids(&self) -> Vec<u32> {
        // Each mounted workspace records the process serving it in mount.json. One
        // process serves them all, so each is counted once.
        let Ok(dirs) = fs::read_dir(self.home.path().join("workspaces")) else { return Vec::new() };
        let pids: std::collections::BTreeSet<u32> = dirs
            .filter_map(|dir| fs::read(dir.ok()?.path().join("mount.json")).ok())
            .filter_map(|json| serde_json::from_slice::<serde_json::Value>(&json).ok()?["pid"].as_u64())
            .map(|pid| pid as u32)
            .collect();
        pids.into_iter().collect()
    }
}

enum Backend {
    HostOriginal,
    HostCopy { original: PathBuf, scratch: PathBuf, _copy: TempDir },
    Overlay { upper: PathBuf, data: PathBuf, scratch: PathBuf, _dir: TempDir },
    Fuse { fuse: Fuse, name: String, mountpoint: PathBuf },
}

/// A view that is a directory on the host, used through `std::fs`.
struct DirView {
    root: PathBuf,
    backend: Backend,
}

impl View for DirView {
    fn read(&self, path: &str) -> Result<usize> {
        Ok(fs::read(self.root.join(path))?.len())
    }

    fn contents(&self, path: &str) -> Result<Vec<u8>> {
        Ok(fs::read(self.root.join(path))?)
    }

    fn root_dir(&self) -> Option<&Path> {
        Some(&self.root)
    }

    fn read_range(&self, path: &str, offset: u64, len: u64) -> Result<usize> {
        // Opened per call: the other targets also address every read by path.
        let mut file = fs::File::open(self.root.join(path))?;
        file.seek(SeekFrom::Start(offset))?;
        let mut buf = Vec::with_capacity(len as usize);
        file.take(len).read_to_end(&mut buf)?;
        Ok(buf.len())
    }

    fn stat(&self, path: &str) -> Result<u64> {
        Ok(fs::metadata(self.root.join(path))?.len())
    }

    fn list(&self, path: &str) -> Result<Vec<(String, bool)>> {
        list_dir(&self.root.join(path))
    }

    fn write(&mut self, path: &str, data: &[u8]) -> Result<()> {
        Ok(fs::write(self.root.join(path), data)?)
    }

    fn remove(&mut self, path: &str) -> Result<()> {
        Ok(fs::remove_file(self.root.join(path))?)
    }

    fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        Ok(fs::rename(self.root.join(from), self.root.join(to))?)
    }

    fn fork(&mut self) -> Result<Box<dyn View>> {
        match &self.backend {
            Backend::HostOriginal => bail!("read-only view"),
            Backend::HostCopy { original, scratch, .. } => {
                let copy = tempfile::Builder::new().prefix("host-").tempdir_in(scratch)?;
                copy_tree(&self.root, copy.path())?;
                Ok(Box::new(DirView {
                    root: copy.path().to_owned(),
                    backend: Backend::HostCopy { original: original.clone(), scratch: scratch.clone(), _copy: copy },
                }))
            }
            Backend::Overlay { upper, data, scratch, .. } => {
                Overlay { data: data.clone(), scratch: scratch.clone() }.mount_view(Some(upper))
            }
            Backend::Fuse { fuse, mountpoint, .. } => {
                let (name, dir) = fuse.fresh_mountpoint();
                run(fuse.command().arg("fork").arg(mountpoint).arg(&dir).args(["--name", &name]))?;
                Ok(fuse.view(name, dir))
            }
        }
    }

    fn changes(&mut self) -> Result<usize> {
        match &self.backend {
            Backend::HostOriginal => Ok(0),
            Backend::HostCopy { original, .. } => compare_trees(original, &self.root),
            Backend::Overlay { upper, data, .. } => overlay_changes(upper, data),
            Backend::Fuse { fuse, mountpoint, .. } => {
                let output = fuse.command().arg("changes").arg(mountpoint).output()?;
                if !output.status.success() {
                    bail!("sandfs changes failed: {}", String::from_utf8_lossy(&output.stderr));
                }
                Ok(output.stdout.split(|b| *b == b'\n').filter(|line| !line.is_empty()).count())
            }
        }
    }
}

impl Drop for DirView {
    fn drop(&mut self) {
        match &self.backend {
            Backend::Overlay { .. } => {
                let _ = umount(&self.root);
            }
            Backend::Fuse { fuse, name, mountpoint } => {
                // Unmounting keeps the workspace; remove it so later runs do not pile up.
                if run(fuse.command().arg("unmount").arg(mountpoint)).is_ok() {
                    let _ = run(fuse.command().arg("rm").arg(name));
                }
                let _ = fs::remove_dir(mountpoint);
            }
            _ => {}
        }
    }
}

// ----------------------------------------------------------------------
// Helpers
// ----------------------------------------------------------------------

fn run(cmd: &mut Command) -> Result<()> {
    let output = cmd.stdin(Stdio::null()).output().with_context(|| format!("running {cmd:?}"))?;
    if !output.status.success() {
        bail!("{cmd:?} failed: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(())
}

fn umount(dir: &Path) -> Result<()> {
    run(Command::new("umount").arg(dir))
}

fn list_dir(dir: &Path) -> Result<Vec<(String, bool)>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().into_string().map_err(|_| anyhow::anyhow!("non-UTF-8 name"))?;
        out.push((name, entry.file_type()?.is_dir()));
    }
    Ok(out)
}

/// Copy a tree of directories and regular files.
pub fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// Every regular file below `dir`, relative to it.
fn files_under(dir: &Path, rel: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let rel = rel.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            files_under(&entry.path(), &rel, out)?;
        } else {
            out.push(rel);
        }
    }
    Ok(())
}

/// Files that differ between two trees: by size first, then by content.
fn compare_trees(original: &Path, copy: &Path) -> Result<usize> {
    let mut before = Vec::new();
    files_under(original, Path::new(""), &mut before)?;
    let mut after = Vec::new();
    files_under(copy, Path::new(""), &mut after)?;
    let before_set: std::collections::HashSet<&PathBuf> = before.iter().collect();
    let after_set: std::collections::HashSet<&PathBuf> = after.iter().collect();
    let mut changed = before.iter().filter(|path| !after_set.contains(path)).count();
    for path in &after {
        if !before_set.contains(path) {
            changed += 1;
            continue;
        }
        let (a, b) = (original.join(path), copy.join(path));
        if fs::metadata(&a)?.len() != fs::metadata(&b)?.len() || !same_bytes(&a, &b)? {
            changed += 1;
        }
    }
    Ok(changed)
}

/// Compare two files of equal size in chunks, so the large file is never held whole.
fn same_bytes(a: &Path, b: &Path) -> Result<bool> {
    let (mut a, mut b) = (fs::File::open(a)?, fs::File::open(b)?);
    let (mut left, mut right) = (vec![0; 1 << 16], vec![0; 1 << 16]);
    loop {
        let n = a.read(&mut left)?;
        if n == 0 {
            return Ok(true);
        }
        b.read_exact(&mut right[..n])?;
        if left[..n] != right[..n] {
            return Ok(false);
        }
    }
}

/// Files an overlay's upper layer adds, modifies or deletes. A whiteout over a lower
/// directory deletes every file below it. Opaque directories are not handled; the
/// workloads never create them.
fn overlay_changes(upper: &Path, lower: &Path) -> Result<usize> {
    fn walk(upper: &Path, lower: &Path, count: &mut usize) -> Result<()> {
        for entry in fs::read_dir(upper)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            let below = lower.join(entry.file_name());
            if kind.is_dir() {
                walk(&entry.path(), &below, count)?;
            } else if kind.is_char_device() {
                if below.is_dir() {
                    let mut files = Vec::new();
                    files_under(&below, Path::new(""), &mut files)?;
                    *count += files.len();
                } else {
                    *count += 1;
                }
            } else {
                *count += 1;
            }
        }
        Ok(())
    }
    let mut count = 0;
    walk(upper, lower, &mut count)?;
    Ok(count)
}
