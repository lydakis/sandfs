//! Where the CLI keeps workspaces between commands.
//!
//! Everything lives under one home directory (`$SANDFS_HOME`, else
//! `$XDG_DATA_HOME/sandfs`, else `~/.local/share/sandfs`):
//!
//! ```text
//! repos/github.com/owner/repo.git   partial clones of remote repositories (see `git`)
//! workspaces/<name>/workspace.json  what the workspace is based on
//! workspaces/<name>/changes.json    its saved changes (see `persist`)
//! workspaces/<name>/objects/        bodies of saved files
//! workspaces/<name>/start/           the changes it was made with, if any (`mount --uncommitted`)
//! workspaces/<name>/mount.json      present while it is mounted
//! workspaces/<name>/lock            held by the one process that may save it
//! workspaces/<name>/holder          who holds the lock, for whoever it refuses
//! run/                              the process serving every mount (see `server`)
//! ```
//!
//! A workspace's base is pinned when it is created: a host folder, or a git commit
//! id. Forks copy the record and the saved changes, never the base.
//!
//! Each process that serves a workspace (a mount or an MCP session) keeps its own copy
//! in memory and saves it over whatever is on disk, so only one may serve it at a time.
//! It holds the workspace's [`Writer`] lock for as long as it serves. Readers (`changes`,
//! `fork`) need no lock.

use std::collections::{BTreeMap, HashMap};
use std::fs::{File, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use sandfs_core::{DirSource, VPath, Workspace};
use serde::{Deserialize, Serialize};

use crate::host::mount_source;
use crate::persist::{Restored, Saved, is_object_gone};

/// Where a workspace's source is mounted inside the core workspace. The mount
/// shows this directory as its root.
pub const ROOT: &str = "/mnt";

/// What a workspace is based on.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Base {
    /// A host folder, read lazily and never written to.
    Dir { path: PathBuf },
    /// A commit of a local repository (a partial clone for remote sources), and the
    /// branch it was mounted as, which the mount's `.git` starts on.
    Git {
        repo: PathBuf,
        commit: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branch: Option<String>,
    },
}

/// `workspace.json`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    /// The source as the user wrote it, for display.
    pub source: String,
    pub base: Base,
    /// Creation time in seconds; untouched base files report it as their mtime.
    pub created: u64,
    /// Paths, relative to the source, that the workspace can read but not change.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read_only: Vec<String>,
}

/// `mount.json`: the process serving a mounted workspace.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MountInfo {
    pub pid: u32,
    pub mountpoint: PathBuf,
    /// Where an NFS mount's process answers save requests (see `nfs`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub save_port: Option<u16>,
    /// The secret a save request must carry, so only its owner can ask.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub save_secret: Option<String>,
    /// Tells this mount apart from a later one of the workspace by the same process.
    #[serde(default)]
    pub id: u64,
}

/// A workspace opened from disk: its contents with the saved changes replayed.
pub struct Opened {
    pub ws: Workspace,
    pub saved: Saved,
    pub restored: Restored,
    pub record: Record,
}

/// The right to save a workspace, held until dropped. The operating system releases it
/// when the holder exits, crashed or not, so a dead process never keeps a workspace.
pub struct Writer {
    _file: File,
}

/// The CLI's home directory.
#[derive(Clone)]
pub struct Home {
    root: PathBuf,
}

/// How often, and how long apart, [`Home::lock`] retries a lock that looks held.
const LOCK_RETRIES: u32 = 20;
const LOCK_RETRY_WAIT: Duration = Duration::from_millis(10);

impl Home {
    /// The home directory from the environment.
    pub fn locate() -> Result<Home> {
        let root = if let Some(dir) = std::env::var_os("SANDFS_HOME") {
            PathBuf::from(dir)
        } else if let Some(dir) = std::env::var_os("XDG_DATA_HOME") {
            PathBuf::from(dir).join("sandfs")
        } else if let Some(home) = std::env::home_dir() {
            home.join(".local/share/sandfs")
        } else {
            bail!("set SANDFS_HOME so sandfs knows where to keep workspaces");
        };
        // The process serving mounts runs from `/`, so a relative home must not depend on
        // where it was started.
        Ok(Home { root: std::path::absolute(root)? })
    }

    /// A home directory at an explicit path.
    #[cfg(test)]
    pub fn at(root: impl Into<PathBuf>) -> Home {
        Home { root: root.into() }
    }

    /// The home directory itself, always absolute.
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn workspace_dir(&self, name: &str) -> PathBuf {
        self.root.join("workspaces").join(name)
    }

    /// Where the process serving every mount keeps its socket, lock and log.
    pub fn run_dir(&self) -> PathBuf {
        self.root.join("run")
    }

    /// Create the workspace `name`. Fails if it exists.
    pub fn create(&self, name: &str, record: &Record) -> Result<PathBuf> {
        check_name(name)?;
        let dir = self.workspace_dir(name);
        std::fs::create_dir_all(dir.parent().unwrap_or(&self.root))?;
        match std::fs::create_dir(&dir) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                bail!("a workspace named {name:?} already exists; pick another with --name")
            }
            Err(err) => return Err(err).context("creating the workspace"),
        }
        if let Err(err) = std::fs::write(dir.join("workspace.json"), serde_json::to_vec_pretty(record)?) {
            // Without its record the directory is not a workspace, and would block the name.
            let _ = std::fs::remove_dir_all(&dir);
            return Err(err).context("creating the workspace");
        }
        Ok(dir)
    }

    pub fn exists(&self, name: &str) -> bool {
        check_name(name).is_ok() && self.workspace_dir(name).join("workspace.json").is_file()
    }

    pub fn record(&self, name: &str) -> Result<Record> {
        check_name(name)?;
        let path = self.workspace_dir(name).join("workspace.json");
        let data = std::fs::read(&path).with_context(|| format!("no workspace named {name:?}"))?;
        serde_json::from_slice(&data).with_context(|| format!("reading {}", path.display()))
    }

    /// Every workspace name, sorted.
    pub fn names(&self) -> Result<Vec<String>> {
        let mut names = Vec::new();
        let Ok(entries) = std::fs::read_dir(self.root.join("workspaces")) else { return Ok(names) };
        for entry in entries {
            if let Some(name) = entry?.file_name().to_str()
                && self.exists(name)
            {
                names.push(name.to_owned());
            }
        }
        names.sort();
        Ok(names)
    }

    /// Where `name` is mounted, if a live process is serving it.
    pub fn mount_info(&self, name: &str) -> Option<MountInfo> {
        self.attached_mount(name).filter(|info| crate::host::alive(info.pid))
    }

    /// Where `name` was mounted, if that mount is still attached, even when the process
    /// serving it has died and left it disconnected.
    pub fn attached_mount(&self, name: &str) -> Option<MountInfo> {
        let info = self.recorded_mount(name)?;
        // Only this workspace's mount: another filesystem may have taken the path since.
        (mount_source(&info.mountpoint)? == format!("sandfs:{name}")).then_some(info)
    }

    /// The mount recorded for `name`, attached or not.
    pub fn recorded_mount(&self, name: &str) -> Option<MountInfo> {
        serde_json::from_slice(&std::fs::read(self.workspace_dir(name).join("mount.json")).ok()?).ok()
    }

    pub fn set_mount_info(&self, name: &str, info: Option<&MountInfo>) -> Result<()> {
        let path = self.workspace_dir(name).join("mount.json");
        match info {
            Some(info) => {
                let mut file = File::options();
                file.write(true).create(true).truncate(true);
                // Readable by its owner only, since it holds the save secret.
                #[cfg(unix)]
                std::os::unix::fs::OpenOptionsExt::mode(&mut file, 0o600);
                let mut file = file.open(&path)?;
                // The mode above only applies to a new file; an older record keeps its own.
                #[cfg(unix)]
                file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
                std::io::Write::write_all(&mut file, &serde_json::to_vec(info)?)?;
            }
            None => {
                let _ = std::fs::remove_file(path);
            }
        }
        Ok(())
    }

    /// The workspace a command names: a workspace name, or a directory one is mounted on.
    pub fn find(&self, what: &str) -> Result<String> {
        if self.exists(what) {
            return Ok(what.to_owned());
        }
        // A mount whose process died cannot be canonicalized (ENOTCONN), but its parent
        // can, which turns aliases such as macOS's `/tmp` into the path the mount was
        // recorded under, so `unmount DIR` can detach it.
        let dir = Path::new(what).canonicalize().or_else(|_| {
            let dir = std::path::absolute(what)?;
            Ok::<_, std::io::Error>(match (dir.parent().map(Path::canonicalize), dir.file_name()) {
                (Some(Ok(parent)), Some(name)) => parent.join(name),
                _ => dir,
            })
        });
        if let Ok(dir) = dir {
            for name in self.names()? {
                if self.attached_mount(&name).is_some_and(|info| info.mountpoint == dir) {
                    return Ok(name);
                }
            }
        }
        bail!("{what:?} is neither a workspace name nor a mounted workspace")
    }

    /// Whether process `pid` holds `name` (see [`Home::lock`]): the lock is held, checked
    /// once without waiting, and its holder names that process. A lock that can't be
    /// checked counts as held.
    #[cfg(mount)]
    pub fn locked_by(&self, name: &str, pid: u32) -> bool {
        let dir = self.workspace_dir(name);
        let held = match File::options().write(true).open(dir.join("lock")) {
            Ok(file) => !matches!(file.try_lock(), Ok(())),
            Err(_) => true,
        };
        held && std::fs::read_to_string(dir.join("holder"))
            .is_ok_and(|holder| holder.ends_with(&format!("(process {pid})")))
    }

    /// Wait for the right to use sandfs's local copies of repositories without a
    /// workspace's own lock: shared to create a workspace or read one unmounted,
    /// exclusive for `sandfs clean`, so a copy is never shrunk or deleted between
    /// resolving a source and recording the workspace over it, or while it is read.
    /// Held until the file is dropped.
    pub fn repos_lock(&self, exclusive: bool) -> Result<File> {
        let repos = self.root.join("repos");
        std::fs::create_dir_all(&repos).with_context(|| format!("creating {}", repos.display()))?;
        // Hidden, so it is never taken for a local copy.
        let path = repos.join(".lock");
        let file = File::options()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        if exclusive { file.lock() } else { file.lock_shared() }
            .with_context(|| format!("locking {}", path.display()))?;
        Ok(file)
    }

    /// Take the right to save `name`, or fail saying who holds it. `holder` describes
    /// this process to whoever is refused next.
    pub fn lock(&self, name: &str, holder: &str) -> Result<Writer> {
        check_name(name)?;
        let path = self.workspace_dir(name).join("lock");
        let file = File::options()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        // Who holds the lock lives in a file of its own: on Windows a locked file can't
        // be read, even by the process that is refused.
        let holder_path = self.workspace_dir(name).join("holder");
        // A child process this one is starting holds a copy of the lock until it runs
        // its program, so a lock let go a moment ago can still look held. Retry briefly
        // before calling it in use.
        let mut attempt = 0;
        let locked = loop {
            match file.try_lock() {
                Err(TryLockError::WouldBlock) if attempt < LOCK_RETRIES => {
                    attempt += 1;
                    std::thread::sleep(LOCK_RETRY_WAIT);
                }
                result => break result,
            }
        };
        match locked {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                let held_by = std::fs::read_to_string(&holder_path).unwrap_or_default();
                let held_by = if held_by.trim().is_empty() { "another sandfs process" } else { held_by.trim() };
                bail!("{name} is in use by {held_by}");
            }
            Err(TryLockError::Error(err)) => return Err(err).with_context(|| format!("locking {}", path.display())),
        }
        std::fs::write(&holder_path, holder).with_context(|| format!("writing {}", holder_path.display()))?;
        Ok(Writer { _file: file })
    }

    /// Open `name`: mount its base and replay its saved changes.
    ///
    /// A process that serves the workspace may be saving meanwhile and deleting the
    /// objects its older saves used; if one goes missing, loading starts over from the
    /// newer index.
    pub fn open(&self, name: &str) -> Result<Opened> {
        let record = self.record(name)?;
        let base = open_base(&record.base, &self.root)?;
        self.load(name, record, &base)
    }

    /// Open `name`, and beside it what it started from: its source, with what was written
    /// into it when it was made (see [`Home::keep_start`]).
    pub fn open_with_start(&self, name: &str) -> Result<(Opened, Workspace)> {
        let record = self.record(name)?;
        let base = open_base(&record.base, &self.root)?;
        let mut start = base.fork();
        let dir = self.start_dir(name);
        if dir.join("changes.json").exists() {
            Saved::new(dir).load(&mut start, &VPath::parse(ROOT)?)?;
        }
        Ok((self.load(name, record, &base)?, start))
    }

    /// Keep `name`'s saved changes as what it started from, for `changes --copy-to` to
    /// tell its changes from the ones made where they are copied to since.
    pub fn keep_start(&self, name: &str) -> Result<()> {
        Saved::new(self.workspace_dir(name)).copy_to(&self.start_dir(name))
    }

    /// Copy what `from` started from to `to`, a fork of it.
    pub fn copy_start(&self, from: &str, to: &str) -> Result<()> {
        if !self.start_dir(from).join("changes.json").exists() {
            return Ok(());
        }
        Saved::new(self.start_dir(from)).copy_to(&self.start_dir(to))
    }

    fn start_dir(&self, name: &str) -> PathBuf {
        self.workspace_dir(name).join("start")
    }

    /// Open `name` and `other` over one base, so comparing them skips the source
    /// files they share. Both must have the same source, as a workspace and its forks do.
    pub fn open_pair(&self, name: &str, other: &str) -> Result<(Opened, Opened)> {
        let (record, other_record) = (self.record(name)?, self.record(other)?);
        if record.base != other_record.base {
            bail!("{name} and {other} have different sources ({} and {})", record.source, other_record.source);
        }
        let base = open_base(&record.base, &self.root)?;
        Ok((self.load(name, record, &base)?, self.load(other, other_record, &base)?))
    }

    /// Open `name` over a base `bases` shares with the other workspaces this process
    /// serves. The base stays open until the returned [`Lease`] is dropped.
    #[cfg(mount)]
    pub fn open_shared(&self, name: &str, bases: &Arc<Bases>) -> Result<(Opened, Lease)> {
        let record = self.record(name)?;
        let (base, lease) = bases.lease(&record.base, &self.root)?;
        Ok((self.load(name, record, &base)?, lease))
    }

    /// Replay `name`'s saved changes onto a fork of `base`.
    fn load(&self, name: &str, record: Record, base: &Workspace) -> Result<Opened> {
        let mut attempts = 0;
        loop {
            let mut ws = base.fork();
            let mut saved = Saved::new(self.workspace_dir(name));
            match saved.load(&mut ws, &VPath::parse(ROOT)?) {
                Ok(restored) => {
                    for path in &record.read_only {
                        ws.set_read_only(&format!("{ROOT}/{path}"), true)?;
                    }
                    return Ok(Opened { ws, saved, restored, record });
                }
                Err(err) if is_object_gone(&err) && attempts < 10 => attempts += 1,
                Err(err) => return Err(err),
            }
        }
    }

    pub fn remove(&self, name: &str) -> Result<()> {
        check_name(name)?;
        std::fs::remove_dir_all(self.workspace_dir(name)).with_context(|| format!("removing workspace {name:?}"))
    }

    /// Turn a source into a record: a host folder, or `git:<path or url>[@rev]`. A URL
    /// needs no `git:`, since it can't name a folder.
    pub fn resolve(&self, source: &str) -> Result<Record> {
        let created = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        let spec = source.strip_prefix("git:").or_else(|| source.contains("://").then_some(source));
        let base = if let Some(spec) = spec {
            let (repo, commit, branch) = crate::git::resolve_branch(&self.root.join("repos"), spec)?;
            Base::Git { repo, commit, branch }
        } else {
            let path = expand(source).canonicalize().with_context(|| format!("no folder at {source}"))?;
            if !path.is_dir() {
                bail!("{} is not a folder", path.display());
            }
            // Saving into the source would change the folder that is meant to stay untouched.
            if crate::resolve_new(&self.root)?.starts_with(&path) {
                bail!(
                    "{} contains sandfs's own state ({}); set SANDFS_HOME to a folder outside it",
                    path.display(),
                    self.root.display()
                );
            }
            Base::Dir { path }
        };
        Ok(Record { source: source.to_owned(), base, created, read_only: Vec::new() })
    }

    /// The record for a new workspace over the `HEAD` of the repository whose working
    /// folder is `top` (see `fork`).
    pub fn resolve_checkout(&self, top: &Path) -> Result<Record> {
        let created = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        let (repo, commit, branch) = crate::git::resolve_head(top)?;
        let source = format!("git:{}", repo.display());
        Ok(Record { source, base: Base::Git { repo, commit, branch }, created, read_only: Vec::new() })
    }
}

/// An empty workspace with `base` mounted at [`ROOT`], for the home at `home`.
fn open_base(base: &Base, home: &Path) -> Result<Workspace> {
    let mut ws = Workspace::new();
    match base {
        Base::Dir { path } => ws.mount(ROOT, Arc::new(DirSource::new(path)?))?,
        Base::Git { repo, commit, branch } => {
            let (files, git_dir) = crate::git::open(repo, commit, branch.as_deref(), home)?;
            ws.mount(ROOT, files)?;
            ws.mount(&format!("{ROOT}/.git"), git_dir)?;
        }
    }
    Ok(ws)
}

/// The git commits open in a process that serves several workspaces, each kept as an
/// empty workspace that the workspaces over it are forks of. Their mounts share one
/// source, with its git helpers, and every folder listing and size read from it.
///
/// A host folder is opened afresh for each workspace instead: its listings are cached
/// once read, and a new mount should see the folder as it is now.
#[cfg(mount)]
#[derive(Default)]
pub struct Bases {
    open: Mutex<HashMap<Base, (Workspace, usize)>>,
}

/// One workspace's use of a shared base, which is closed when its last use ends.
#[cfg(mount)]
pub struct Lease {
    bases: Arc<Bases>,
    base: Option<Base>,
}

#[cfg(mount)]
impl Bases {
    /// An empty workspace over `base` to fork, and the lease that keeps it shared.
    fn lease(self: &Arc<Self>, base: &Base, home: &Path) -> Result<(Workspace, Lease)> {
        if !matches!(base, Base::Git { .. }) {
            return Ok((open_base(base, home)?, Lease { bases: self.clone(), base: None }));
        }
        // A lease is made only once its use is counted, so dropping it never uncounts
        // another mount's.
        let lease = || Lease { bases: self.clone(), base: Some(base.clone()) };
        if let Some((ws, uses)) = self.open.lock().expect("bases lock").get_mut(base) {
            *uses += 1;
            return Ok((ws.fork(), lease()));
        }
        // Opening a commit starts git, so other mounts aren't kept waiting meanwhile. Of
        // two mounts opening it at once, the second to finish uses the first one's.
        let opened = open_base(base, home)?;
        let mut open = self.open.lock().expect("bases lock");
        let (ws, uses) = open.entry(base.clone()).or_insert((opened, 0));
        *uses += 1;
        Ok((ws.fork(), lease()))
    }
}

#[cfg(mount)]
impl Drop for Lease {
    fn drop(&mut self) {
        let Some(base) = self.base.take() else { return };
        let mut open = self.bases.open.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some((_, uses)) = open.get_mut(&base) else { return };
        *uses -= 1;
        if *uses == 0 {
            // Closing the source waits for its git helpers to exit; nobody waits on that.
            let closed = open.remove(&base);
            drop(open);
            drop(closed);
        }
    }
}

/// Expand a leading `~/` (or `~\` on Windows) to the user's home.
pub fn expand(path: &str) -> PathBuf {
    let rest = path.strip_prefix("~/").or_else(|| if cfg!(windows) { path.strip_prefix("~\\") } else { None });
    match (rest, std::env::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(path),
    }
}

/// Workspace names become directory names, so keep them plain.
pub fn check_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && !name.starts_with('.')
        && name.len() <= 100
        && name.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c));
    if ok { Ok(()) } else { bail!("invalid workspace name {name:?}: use letters, digits, '.', '_' and '-'") }
}

/// The keys of `map` equal to `prefix` or below it.
pub(crate) fn subtree_keys<T>(map: &BTreeMap<String, T>, prefix: &VPath) -> Vec<String> {
    let p = prefix.as_str();
    let mut keys: Vec<String> = map.contains_key(p).then(|| p.to_owned()).into_iter().collect();
    // '0' is the byte after '/', so this range is exactly the paths below `prefix`.
    keys.extend(map.range(format!("{p}/")..format!("{p}0")).map(|(key, _)| key.clone()));
    keys
}

/// Whether a sandfs workspace or other FUSE filesystem is mounted at `dir`.
pub fn is_mounted(dir: &Path) -> bool {
    mount_source(dir).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_plain() {
        assert!(check_name("fix-search_2.0").is_ok());
        for bad in ["", ".hidden", "a/b", "..", "a b"] {
            assert!(check_name(bad).is_err(), "{bad:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn mount_records_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let (state, data) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let home = Home::at(state.path());
        home.create("ws", &home.resolve(data.path().to_str().unwrap()).unwrap()).unwrap();
        // A record left readable by an older release is made private before the secret goes in.
        let path = home.workspace_dir("ws").join("mount.json");
        std::fs::write(&path, "{}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let info = MountInfo {
            pid: 1,
            mountpoint: data.path().to_path_buf(),
            save_port: Some(1),
            save_secret: Some("00".into()),
            id: 1,
        };
        home.set_mount_info("ws", Some(&info)).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn one_writer_at_a_time() {
        let (state, data) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let home = Home::at(state.path());
        home.create("ws", &home.resolve(data.path().to_str().unwrap()).unwrap()).unwrap();

        let first = home.lock("ws", "the first writer").unwrap();
        let refused = home.lock("ws", "the second writer").err().unwrap();
        assert_eq!(refused.to_string(), "ws is in use by the first writer");
        drop(first);
        let _second = home.lock("ws", "the second writer").unwrap();
        assert!(home.lock("missing", "a writer").is_err(), "locking never creates a workspace");
        assert!(!home.workspace_dir("missing").exists());
    }

    #[test]
    fn readers_load_while_a_writer_keeps_saving() {
        use std::collections::{BTreeMap, BTreeSet};

        use crate::persist::Snapshot;

        let (state, data) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let home = Home::at(state.path());
        home.create("ws", &home.resolve(data.path().to_str().unwrap()).unwrap()).unwrap();
        let root = VPath::parse(ROOT).unwrap();
        let mut writer = home.open("ws").unwrap();
        writer.ws.write("/mnt/count.txt", "0").unwrap();
        let snap = |ws: &Workspace| Snapshot { ws: ws.fork(), times: BTreeMap::new(), dirs: BTreeSet::new() };
        writer.saved.save(&snap(&writer.ws), &root, false).unwrap();

        // Every save replaces the file's object, and deletes the one before last, so a
        // reader that loads an index just before two saves finds its object gone, at
        // load or when it reads the file, and loads again.
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for i in 1..=300 {
                    writer.ws.write("/mnt/count.txt", i.to_string()).unwrap();
                    writer.saved.save(&snap(&writer.ws), &root, false).unwrap();
                }
            });
            for _ in 0..300 {
                let read = loop {
                    match home.open("ws").unwrap().ws.read_text("/mnt/count.txt") {
                        Ok(read) => break read,
                        Err(err) => assert!(crate::persist::is_object_gone(&err.into())),
                    }
                };
                assert!(read.parse::<u32>().is_ok(), "{read:?}");
            }
        });
    }

    #[test]
    fn records_round_trip_through_a_home() {
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        std::fs::write(data.path().join("f.txt"), "f").unwrap();
        let home = Home::at(home.path());
        let record = home.resolve(data.path().to_str().unwrap()).unwrap();
        assert!(home.resolve(data.path().join("f.txt").to_str().unwrap()).is_err(), "a file is not a source");
        home.create("one", &record).unwrap();
        assert!(home.create("one", &record).is_err(), "names are unique");
        assert_eq!(home.names().unwrap(), ["one"]);
        assert_eq!(home.find("one").unwrap(), "one");
        let opened = home.open("one").unwrap();
        assert_eq!(opened.ws.read_text("/mnt/f.txt").unwrap(), "f");
        home.remove("one").unwrap();
        assert!(home.names().unwrap().is_empty());

        // A folder that holds the home itself would have workspaces saved into it.
        let inside = Home::at(data.path().join(".local/share/sandfs"));
        assert!(inside.resolve(data.path().to_str().unwrap()).is_err());
        assert!(!data.path().join(".local").exists(), "nothing is created in the source");
    }
}
