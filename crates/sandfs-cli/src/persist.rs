//! Saving a mounted workspace's changes to disk and replaying them on the next mount.
//!
//! The base of a workspace never needs saving: it is a pinned git commit or a host
//! directory. What is saved is the overlay, as one entry per changed path in
//! `changes.json` plus one file per saved file body under `objects/`. A save rewrites
//! the small index but writes an object only for files whose version changed since the
//! last save, so saving costs the index plus whatever was written in between.
//!
//! The entries come from [`Workspace::changes`] (files) plus the directories the mount
//! created or removed, which `changes` does not report.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use sandfs_core::{ChangeKind, Error, Kind, VPath, Workspace};
use serde::{Deserialize, Serialize};

/// A point in time as `(seconds, nanoseconds)` since the Unix epoch.
type Time = (u64, u32);

/// The saved state of one changed path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "lowercase")]
enum Entry {
    /// A regular file whose bytes are in `objects/<object>`.
    File {
        executable: bool,
        object: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mtime: Option<Time>,
    },
    Dir {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mtime: Option<Time>,
    },
    /// Removed: a deleted base file or directory.
    Absent,
}

/// `changes.json`: entries keyed by path relative to the mount root, so parents sort
/// before their children and replay can go in order.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Changes {
    entries: BTreeMap<String, Entry>,
}

/// What a save needs, taken under the filesystem lock. The workspace is a fork, so
/// taking it is O(1) and the save can run while the mount keeps changing.
pub struct Snapshot {
    pub ws: Workspace,
    /// Modification times of touched paths.
    pub times: BTreeMap<String, SystemTime>,
    /// Directories created or removed through the mount.
    pub dirs: BTreeSet<String>,
}

/// What [`Saved::load`] restores besides the workspace contents.
#[derive(Debug, Default)]
pub struct Restored {
    pub times: BTreeMap<String, SystemTime>,
    pub dirs: BTreeSet<String>,
}

/// The saved changes of one workspace, in its state directory.
pub struct Saved {
    dir: PathBuf,
    /// For each saved file: the version it had when saved, and its object.
    objects: HashMap<String, (u64, String)>,
    next_object: u64,
    /// Objects the previous save replaced. They are deleted one save later, so a
    /// reader that loaded the index just before that save still finds them.
    doomed: Vec<String>,
    /// Objects written since the last durable save, not yet flushed to disk.
    unsynced: HashSet<String>,
}

impl Saved {
    /// The saved changes in the state directory `dir`.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Saved { dir: dir.into(), objects: HashMap::new(), next_object: 1, doomed: Vec::new(), unsynced: HashSet::new() }
    }

    fn index_path(&self) -> PathBuf {
        self.dir.join("changes.json")
    }

    fn objects_dir(&self) -> PathBuf {
        self.dir.join("objects")
    }

    fn read_index(&self) -> Result<Changes> {
        match std::fs::read(self.index_path()) {
            Ok(data) => serde_json::from_slice(&data).context("reading changes.json"),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Changes::default()),
            Err(err) => Err(err).context("reading changes.json"),
        }
    }

    /// Replay the saved changes onto `ws`, whose source is mounted at `root`.
    ///
    /// Loading only reads, so any process may load a workspace, even while another
    /// one serves it.
    pub fn load(&mut self, ws: &mut Workspace, root: &VPath) -> Result<Restored> {
        let changes = self.read_index()?;
        let mut restored = Restored::default();
        for (rel, entry) in &changes.entries {
            let path = under(root, rel)?;
            self.apply(ws, &path, entry).with_context(|| format!("restoring {rel}"))?;
            let mtime = match entry {
                Entry::File { mtime, .. } | Entry::Dir { mtime } => *mtime,
                Entry::Absent => None,
            };
            if let Some(mtime) = mtime {
                restored.times.insert(path.to_string(), from_time(mtime));
            }
            match entry {
                Entry::File { object, .. } => {
                    let version = ws.stat(path.as_str())?.version;
                    self.objects.insert(rel.clone(), (version, object.clone()));
                }
                Entry::Dir { .. } | Entry::Absent => {
                    restored.dirs.insert(path.to_string());
                }
            }
        }
        Ok(restored)
    }

    /// Delete objects the loaded index does not use, left by a crash or by the last
    /// session's delayed deletes. Only the process that serves the workspace may do
    /// this: it alone writes objects.
    pub fn collect_garbage(&mut self) {
        let live: HashSet<&str> = self.objects.values().map(|(_, object)| object.as_str()).collect();
        let mut last = 0;
        if let Ok(files) = std::fs::read_dir(self.objects_dir()) {
            for file in files.flatten() {
                let name = file.file_name();
                let name = name.to_str().unwrap_or_default();
                if live.contains(name) {
                    last = last.max(name.parse().unwrap_or(0));
                } else {
                    let _ = std::fs::remove_file(file.path());
                }
            }
        }
        self.next_object = last + 1;
    }

    fn apply(&self, ws: &mut Workspace, path: &VPath, entry: &Entry) -> Result<()> {
        let p = path.as_str();
        let current = match ws.stat(p) {
            Ok(meta) => Some(meta.kind),
            // Below a file that replaced a directory: the file already hides it.
            Err(Error::NotFound(_) | Error::NotADirectory(_)) if *entry == Entry::Absent => None,
            Err(Error::NotFound(_)) => None,
            Err(err) => return Err(err.into()),
        };
        match entry {
            Entry::Absent => {
                if current.is_some() {
                    ws.remove(p, true)?;
                }
            }
            Entry::Dir { .. } => match current {
                Some(Kind::Dir) => {}
                Some(_) => {
                    ws.remove(p, false)?;
                    ws.mkdir(p, true)?;
                }
                None => ws.mkdir(p, true)?,
            },
            Entry::File { executable, object, .. } => {
                check_object(object)?;
                // A directory in the way goes first.
                if current == Some(Kind::Dir) {
                    ws.remove(p, true)?;
                }
                let data = std::fs::read(self.objects_dir().join(object))
                    .with_context(|| format!("reading saved object {object}"))?;
                ws.write(p, data)?;
                ws.set_executable(p, *executable)?;
            }
        }
        Ok(())
    }

    /// Save `snap`, whose source is mounted at `root`. A `durable` save, which answers
    /// `fsync`, also flushes the index and every object it names to disk before returning.
    pub fn save(&mut self, snap: &Snapshot, root: &VPath, durable: bool) -> Result<()> {
        let mut entries = BTreeMap::new();
        let mut kept = HashMap::new();
        for change in snap.ws.changes()? {
            let rel = relative(root, &change.path)?;
            let entry = match change.kind {
                ChangeKind::Deleted => Entry::Absent,
                ChangeKind::Added | ChangeKind::Modified => self.entry(snap, &change.path, &rel, &mut kept)?,
            };
            entries.insert(rel, entry);
        }
        for dir in &snap.dirs {
            let path = VPath::parse(dir)?;
            let rel = relative(root, &path)?;
            let entry = match snap.ws.stat(dir) {
                // A directory that replaced a deleted base file overrides its deletion.
                Ok(meta) if meta.kind.is_dir() => Entry::Dir { mtime: snap.times.get(dir).map(|t| to_time(*t)) },
                _ if entries.contains_key(&rel) => continue,
                Ok(_) => self.entry(snap, &path, &rel, &mut kept)?,
                // Gone, or below a file that replaced one of its parents.
                Err(Error::NotFound(_) | Error::NotADirectory(_)) => Entry::Absent,
                Err(err) => return Err(err.into()),
            };
            entries.insert(rel, entry);
        }

        let index = serde_json::to_vec(&Changes { entries })?;
        let tmp = self.dir.join("changes.json.tmp");
        if durable {
            // Objects reach the disk before the index that names them.
            let live: HashSet<&String> = kept.values().map(|(_, object)| object).collect();
            for object in self.unsynced.iter().filter(|o| live.contains(o)) {
                sync_path(&self.objects_dir().join(object))?;
            }
            if !self.unsynced.is_empty() {
                sync_path(&self.objects_dir())?;
            }
            self.unsynced.clear();
        }
        std::fs::write(&tmp, index).context("writing changes.json")?;
        if durable {
            sync_path(&tmp)?;
        }
        std::fs::rename(&tmp, self.index_path()).context("writing changes.json")?;
        if durable {
            sync_path(&self.dir)?;
        }

        // Objects replaced by the save before this one are now two indexes old.
        for object in std::mem::take(&mut self.doomed) {
            let _ = std::fs::remove_file(self.objects_dir().join(object));
        }
        let live: HashSet<&String> = kept.values().map(|(_, object)| object).collect();
        self.doomed = self.objects.values().map(|(_, object)| object).filter(|o| !live.contains(o)).cloned().collect();
        self.objects = kept;
        Ok(())
    }

    /// The entry for a file or directory, writing its bytes to a new object unless the
    /// last save already holds this version.
    fn entry(
        &mut self,
        snap: &Snapshot,
        path: &VPath,
        rel: &str,
        kept: &mut HashMap<String, (u64, String)>,
    ) -> Result<Entry> {
        let p = path.as_str();
        let meta = snap.ws.stat(p)?;
        let mtime = snap.times.get(p).map(|t| to_time(*t));
        match meta.kind {
            Kind::Dir => Ok(Entry::Dir { mtime }),
            Kind::File { executable } => {
                let object = match self.objects.get(rel) {
                    Some((version, object)) if *version == meta.version => object.clone(),
                    _ => self.write_object(&snap.ws.read(p)?)?,
                };
                kept.insert(rel.to_owned(), (meta.version, object.clone()));
                Ok(Entry::File { executable, object, mtime })
            }
        }
    }

    fn write_object(&mut self, data: &Bytes) -> Result<String> {
        let dir = self.objects_dir();
        std::fs::create_dir_all(&dir).context("creating the objects directory")?;
        let object = self.next_object.to_string();
        self.next_object += 1;
        std::fs::write(dir.join(&object), data).context("writing a saved file")?;
        self.unsynced.insert(object.clone());
        Ok(object)
    }

    /// Copy the saved changes into `to`, the state directory of a fork. Objects are
    /// never modified once written, so they are hard-linked where the disk allows.
    ///
    /// The workspace may be mounted and saving meanwhile; if an object the index
    /// names has already been deleted, the copy starts over from the newer index.
    pub fn copy_to(&self, to: &Path) -> Result<()> {
        let objects = to.join("objects");
        for _ in 0..10 {
            let index = match std::fs::read(self.index_path()) {
                Ok(index) => index,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(err) => return Err(err).context("reading changes.json"),
            };
            let changes: Changes = serde_json::from_slice(&index).context("reading changes.json")?;
            std::fs::create_dir_all(&objects)?;
            let mut complete = true;
            for entry in changes.entries.values() {
                let Entry::File { object, .. } = entry else { continue };
                check_object(object)?;
                let (from, target) = (self.objects_dir().join(object), objects.join(object));
                if target.exists() {
                    continue;
                }
                if std::fs::hard_link(&from, &target).is_err() && std::fs::copy(&from, &target).is_err() {
                    // A failed copy may leave part of the file, which a retry must not reuse.
                    let _ = std::fs::remove_file(&target);
                    complete = false;
                    break;
                }
            }
            if complete {
                std::fs::write(to.join("changes.json"), index)?;
                return Ok(());
            }
        }
        bail!("the workspace kept changing while it was being copied; try again")
    }
}

/// Object names are numbers; refuse anything else rather than follow it on disk.
fn check_object(object: &str) -> Result<()> {
    if object.is_empty() || !object.bytes().all(|b| b.is_ascii_digit()) {
        bail!("invalid object name {object:?} in changes.json");
    }
    Ok(())
}

/// `path` relative to `root`, as stored in the index.
fn relative(root: &VPath, path: &VPath) -> Result<String> {
    match path.strip_prefix(root) {
        Some(parts) if !parts.is_empty() => Ok(parts.join("/")),
        _ => bail!("{path} is not inside the mount"),
    }
}

/// The workspace path of an index key, refusing keys that climb out of the mount.
fn under(root: &VPath, rel: &str) -> Result<VPath> {
    let path = VPath::parse(&format!("{root}/{rel}"))?;
    if path == *root || !path.starts_with(root) {
        bail!("saved path {rel:?} is outside the mount");
    }
    Ok(path)
}

fn to_time(time: SystemTime) -> Time {
    let since = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    (since.as_secs(), since.subsec_nanos())
}

fn from_time((secs, nanos): Time) -> SystemTime {
    UNIX_EPOCH + Duration::new(secs, nanos)
}

/// Flush a file or directory to disk (`fsync`).
fn sync_path(path: &Path) -> Result<()> {
    std::fs::File::open(path).and_then(|file| file.sync_all()).with_context(|| format!("syncing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use sandfs_core::MemorySource;

    use super::*;

    fn base() -> Workspace {
        let source = MemorySource::from_files([("a.txt", "a"), ("dir/b.txt", "b"), ("dir/sub/c.txt", "c")]).unwrap();
        let mut ws = Workspace::new();
        ws.mount("/mnt", Arc::new(source)).unwrap();
        ws
    }

    fn root() -> VPath {
        VPath::parse("/mnt").unwrap()
    }

    fn listing(ws: &Workspace, path: &str) -> Vec<String> {
        ws.list(path).unwrap().into_iter().map(|e| e.name).collect()
    }

    #[test]
    fn changes_survive_a_save_and_load() {
        let state = tempfile::tempdir().unwrap();
        let mut ws = base();
        ws.write("/mnt/a.txt", "changed").unwrap();
        ws.write("/mnt/new/run.sh", "#!/bin/sh").unwrap();
        ws.set_executable("/mnt/new/run.sh", true).unwrap();
        ws.remove("/mnt/dir/sub", true).unwrap();
        ws.mkdir("/mnt/empty", false).unwrap();
        let dirs = ["/mnt/dir/sub", "/mnt/empty", "/mnt/new"].map(str::to_owned).into();
        let mtime = UNIX_EPOCH + Duration::new(1_700_000_000, 5);
        let times = [("/mnt/a.txt".to_owned(), mtime)].into();

        let mut saved = Saved::new(state.path());
        saved.save(&Snapshot { ws: ws.fork(), times, dirs }, &root(), true).unwrap();
        assert!(saved.unsynced.is_empty(), "a durable save flushes every object it wrote");

        let mut loaded = base();
        let restored = Saved::new(state.path()).load(&mut loaded, &root()).unwrap();
        assert_eq!(loaded.read_text("/mnt/a.txt").unwrap(), "changed");
        assert_eq!(loaded.stat("/mnt/new/run.sh").unwrap().kind, Kind::File { executable: true });
        assert_eq!(listing(&loaded, "/mnt/dir"), ["b.txt"], "the removed directory stays removed");
        assert!(loaded.list("/mnt/empty").unwrap().is_empty(), "empty directories are kept");
        assert_eq!(loaded.changes().unwrap(), ws.changes().unwrap());
        assert_eq!(restored.times["/mnt/a.txt"], mtime);
        assert!(restored.dirs.contains("/mnt/empty"));
    }

    #[test]
    fn saves_write_only_files_that_changed_and_drop_stale_objects() {
        let state = tempfile::tempdir().unwrap();
        let mut ws = base();
        ws.write("/mnt/one.txt", "1").unwrap();
        ws.write("/mnt/two.txt", "2").unwrap();
        let mut saved = Saved::new(state.path());
        let snap = |ws: &Workspace| Snapshot { ws: ws.fork(), times: BTreeMap::new(), dirs: BTreeSet::new() };
        saved.save(&snap(&ws), &root(), false).unwrap();
        let objects = |dir: &Path| {
            let mut names: Vec<String> = std::fs::read_dir(dir.join("objects"))
                .unwrap()
                .map(|f| f.unwrap().file_name().into_string().unwrap())
                .collect();
            names.sort();
            names
        };
        assert_eq!(objects(state.path()), ["1", "2"]);

        ws.write("/mnt/two.txt", "2 again").unwrap();
        saved.save(&snap(&ws), &root(), false).unwrap();
        assert_eq!(objects(state.path()), ["1", "2", "3"], "one.txt kept its object; two.txt's old one waits a save");

        ws.remove("/mnt/one.txt", false).unwrap();
        saved.save(&snap(&ws), &root(), false).unwrap();
        assert_eq!(objects(state.path()), ["1", "3"], "2 is gone now; 1 goes after the next save");

        let mut reopened = Saved::new(state.path());
        reopened.load(&mut base(), &root()).unwrap();
        reopened.collect_garbage();
        assert_eq!(objects(state.path()), ["3"], "the serving process clears what the index no longer uses");
        assert_eq!(reopened.next_object, 4);
    }

    #[test]
    fn a_file_that_replaced_a_directory_is_restored() {
        let state = tempfile::tempdir().unwrap();
        let mut ws = base();
        ws.remove("/mnt/dir", true).unwrap();
        ws.write("/mnt/dir", "now a file").unwrap();
        let snap = Snapshot { ws: ws.fork(), times: BTreeMap::new(), dirs: ["/mnt/dir".to_owned()].into() };
        Saved::new(state.path()).save(&snap, &root(), false).unwrap();

        let mut loaded = base();
        Saved::new(state.path()).load(&mut loaded, &root()).unwrap();
        assert_eq!(loaded.read_text("/mnt/dir").unwrap(), "now a file");
        assert_eq!(loaded.changes().unwrap(), ws.changes().unwrap());
    }

    #[test]
    fn replacements_between_files_and_directories_are_saved() {
        let state = tempfile::tempdir().unwrap();
        let mut ws = base();
        // dir/sub is tracked as removed, then dir itself becomes a file.
        ws.remove("/mnt/dir", true).unwrap();
        ws.write("/mnt/dir", "a file").unwrap();
        // a.txt becomes an empty directory.
        ws.remove("/mnt/a.txt", false).unwrap();
        ws.mkdir("/mnt/a.txt", false).unwrap();
        let dirs = ["/mnt/dir", "/mnt/dir/sub", "/mnt/a.txt"].map(str::to_owned).into();
        Saved::new(state.path())
            .save(&Snapshot { ws: ws.fork(), times: BTreeMap::new(), dirs }, &root(), false)
            .unwrap();

        let mut loaded = base();
        Saved::new(state.path()).load(&mut loaded, &root()).unwrap();
        assert_eq!(loaded.read_text("/mnt/dir").unwrap(), "a file");
        assert!(loaded.stat("/mnt/a.txt").unwrap().kind.is_dir(), "the directory that replaced a file is kept");
    }

    #[test]
    fn index_paths_cannot_leave_the_mount() {
        assert!(under(&root(), "../etc/passwd").is_err());
        assert!(under(&root(), "a/../../x").is_err());
        assert_eq!(under(&root(), "a/b").unwrap().as_str(), "/mnt/a/b");
    }
}
