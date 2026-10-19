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
//! workspaces/<name>/mount.json      present while it is mounted
//! workspaces/<name>/lock            held by the one process that may save it
//! ```
//!
//! A workspace's base is pinned when it is created: a host folder, or a git commit
//! id. Forks copy the record and the saved changes, never the base.
//!
//! Each process that serves a workspace (a mount or an MCP session) keeps its own copy
//! in memory and saves it over whatever is on disk, so only one may serve it at a time.
//! It holds the workspace's [`Writer`] lock for as long as it serves. Readers (`changes`,
//! `fork`) need no lock.

use std::fs::{File, TryLockError};
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use sandfs_core::{DirSource, Source, VPath, Workspace};
use serde::{Deserialize, Serialize};

use crate::persist::{ObjectGone, Restored, Saved};

/// Where a workspace's source is mounted inside the core workspace. The FUSE mount
/// shows this directory as its root.
pub const ROOT: &str = "/mnt";

/// What a workspace is based on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Base {
    /// A host folder, read lazily and never written to.
    Dir { path: PathBuf },
    /// A commit of a local repository (a partial clone for remote sources).
    Git { repo: PathBuf, commit: String },
}

/// `workspace.json`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    /// The source as the user wrote it, for display.
    pub source: String,
    pub base: Base,
    /// Creation time in seconds; untouched base files report it as their mtime.
    pub created: u64,
}

/// `mount.json`: the process serving a mounted workspace.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MountInfo {
    pub pid: u32,
    pub mountpoint: PathBuf,
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
pub struct Home {
    root: PathBuf,
}

impl Home {
    /// The home directory from the environment.
    pub fn locate() -> Result<Home> {
        let root = if let Some(dir) = std::env::var_os("SANDFS_HOME") {
            PathBuf::from(dir)
        } else if let Some(dir) = std::env::var_os("XDG_DATA_HOME") {
            PathBuf::from(dir).join("sandfs")
        } else if let Some(home) = std::env::var_os("HOME") {
            PathBuf::from(home).join(".local/share/sandfs")
        } else {
            bail!("set SANDFS_HOME or HOME so sandfs knows where to keep workspaces");
        };
        Ok(Home { root })
    }

    /// A home directory at an explicit path.
    #[cfg(test)]
    pub fn at(root: impl Into<PathBuf>) -> Home {
        Home { root: root.into() }
    }

    pub fn workspace_dir(&self, name: &str) -> PathBuf {
        self.root.join("workspaces").join(name)
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
        self.attached_mount(name).filter(|info| Path::new(&format!("/proc/{}", info.pid)).exists())
    }

    /// Where `name` was mounted, if that mount is still attached, even when the process
    /// serving it has died and left it disconnected.
    pub fn attached_mount(&self, name: &str) -> Option<MountInfo> {
        let data = std::fs::read(self.workspace_dir(name).join("mount.json")).ok()?;
        let info: MountInfo = serde_json::from_slice(&data).ok()?;
        // Only this workspace's mount: another filesystem may have taken the path since.
        (mount_source(&info.mountpoint)? == format!("sandfs:{name}")).then_some(info)
    }

    pub fn set_mount_info(&self, name: &str, info: Option<&MountInfo>) -> Result<()> {
        let path = self.workspace_dir(name).join("mount.json");
        match info {
            Some(info) => std::fs::write(path, serde_json::to_vec(info)?)?,
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
        // A mount whose process died cannot be canonicalized (ENOTCONN), but it is
        // still found by its absolute path, so `unmount DIR` can detach it.
        let dir = Path::new(what).canonicalize().or_else(|_| std::path::absolute(what));
        if let Ok(dir) = dir {
            for name in self.names()? {
                if self.attached_mount(&name).is_some_and(|info| info.mountpoint == dir) {
                    return Ok(name);
                }
            }
        }
        bail!("{what:?} is neither a workspace name nor a mounted workspace")
    }

    /// Take the right to save `name`, or fail saying who holds it. `holder` describes
    /// this process to whoever is refused next.
    pub fn lock(&self, name: &str, holder: &str) -> Result<Writer> {
        check_name(name)?;
        let path = self.workspace_dir(name).join("lock");
        // Not truncated on open: until the lock is ours, the text names the current holder.
        let mut file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                let mut held_by = String::new();
                let _ = file.read_to_string(&mut held_by);
                let held_by = if held_by.trim().is_empty() { "another sandfs process" } else { held_by.trim() };
                bail!("{name} is in use by {held_by}");
            }
            Err(TryLockError::Error(err)) => return Err(err).with_context(|| format!("locking {}", path.display())),
        }
        file.set_len(0)?;
        file.rewind()?;
        file.write_all(holder.as_bytes())?;
        Ok(Writer { _file: file })
    }

    /// Open `name`: mount its base and replay its saved changes.
    ///
    /// A process that serves the workspace may be saving meanwhile and deleting the
    /// objects its older saves used; if one goes missing, loading starts over from the
    /// newer index.
    pub fn open(&self, name: &str) -> Result<Opened> {
        let record = self.record(name)?;
        let source: Arc<dyn Source> = match &record.base {
            Base::Dir { path } => Arc::new(DirSource::new(path)?),
            Base::Git { repo, commit } => crate::git::open(repo, commit)?,
        };
        let mut attempts = 0;
        loop {
            let mut ws = Workspace::new();
            ws.mount(ROOT, source.clone())?;
            let mut saved = Saved::new(self.workspace_dir(name));
            match saved.load(&mut ws, &VPath::parse(ROOT)?) {
                Ok(restored) => return Ok(Opened { ws, saved, restored, record }),
                Err(err) if err.downcast_ref::<ObjectGone>().is_some() && attempts < 10 => attempts += 1,
                Err(err) => return Err(err),
            }
        }
    }

    pub fn remove(&self, name: &str) -> Result<()> {
        check_name(name)?;
        std::fs::remove_dir_all(self.workspace_dir(name)).with_context(|| format!("removing workspace {name:?}"))
    }

    /// Turn a source into a record: a host folder, or `git:<path or url>[@rev]`.
    pub fn resolve(&self, source: &str) -> Result<Record> {
        let created = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        let base = if let Some(spec) = source.strip_prefix("git:") {
            let (repo, commit) = crate::git::resolve(&self.root.join("repos"), spec)?;
            Base::Git { repo, commit }
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
        Ok(Record { source: source.to_owned(), base, created })
    }
}

/// Expand a leading `~/`.
pub fn expand(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
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

/// Whether a FUSE filesystem is mounted at `dir`, from `/proc/self/mountinfo`.
pub fn is_mounted(dir: &Path) -> bool {
    mount_source(dir).is_some()
}

/// The source of the topmost FUSE mount at `dir` (`sandfs:<name>` for a workspace).
fn mount_source(dir: &Path) -> Option<String> {
    let table = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    table.lines().rev().find_map(|line| {
        let fields: Vec<&str> = line.split(' ').collect();
        // Field 5 is the mount point; the type and source follow the " - " separator.
        let sep = fields.iter().position(|f| *f == "-")?;
        let (fstype, source) = (fields.get(sep + 1)?, fields.get(sep + 2)?);
        (fields.get(4).is_some_and(|point| Path::new(&unescape(point)) == dir) && fstype.starts_with("fuse"))
            .then(|| unescape(source))
    })
}

/// Undo mountinfo's octal escapes (`\040` for a space).
fn unescape(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() && bytes[i + 1..i + 4].iter().all(u8::is_ascii_digit) {
            let code = std::str::from_utf8(&bytes[i + 1..i + 4]).ok().and_then(|s| u8::from_str_radix(s, 8).ok());
            if let Some(code) = code {
                out.push(code);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
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

    #[test]
    fn mountinfo_escapes_decode() {
        assert_eq!(unescape(r"/home/me/my\040ws"), "/home/me/my ws");
        assert_eq!(unescape(r"/plain"), "/plain");
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
        // reader that loads an index just before two saves finds its object gone.
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for i in 1..=300 {
                    writer.ws.write("/mnt/count.txt", i.to_string()).unwrap();
                    writer.saved.save(&snap(&writer.ws), &root, false).unwrap();
                }
            });
            for _ in 0..300 {
                let read = home.open("ws").unwrap().ws.read_text("/mnt/count.txt").unwrap();
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
