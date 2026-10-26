//! Moving a workspace onto a newer version of its source: `sandfs update`.
//!
//! A workspace's saved changes are entries over its base (see `persist`). Moving it
//! points its record at the new base and goes over those entries. One the new base
//! didn't change stays as it is. One the new base did change goes if the workspace's
//! version is the new base's own, so it stops counting as a change, and otherwise
//! stays, reported as changed on both sides. Nothing is copied: saved files keep
//! their objects, and forks keep the base they had.

use std::collections::BTreeSet;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use sandfs_core::{Error, Kind, Metadata, VPath, Workspace};
use serde::{Deserialize, Serialize};

use crate::git;
use crate::persist::{PathState, Snapshot};
use crate::store::{Base, Home, ROOT, Record, open_base};

/// What [`crate::Home::update`] did.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Updated {
    /// The base before, for display: a commit or a folder.
    pub from: String,
    /// The base after, for display.
    pub to: String,
    /// Paths, relative to the source, that both the workspace and the newer source
    /// changed. The workspace's version stays.
    pub both: Vec<String>,
    /// How many files the newer source changed, when it can say (a commit can).
    pub upstream: Option<usize>,
    /// Saved changes that matched the newer source, and so are changes no longer.
    pub matched: usize,
    /// The paths, relative to the source, that may read differently now, when the
    /// source can say: what it changed, and the workspace's changes that went. A mount
    /// tells the kernel to forget what it holds of them.
    #[serde(skip)]
    pub(crate) changed: Option<BTreeSet<String>>,
}

/// Move the workspace `name` onto `source`, or onto the newest version of the source it
/// was made from: another folder, or another commit of the same repository. A mounted
/// workspace is moved by the process serving it, in place, so whatever runs in the
/// mount reads the newer files from then on.
pub fn update(home: &Home, name: &str, source: Option<&str>) -> Result<Updated> {
    crate::ops::not_a_folder(home, name)?;
    let frozen = home.record(name).is_ok_and(|record| is_copy(home, &record));
    let updated = match home.attached_mount(name) {
        Some(_) => move_mounted(home, name, source),
        None => move_onto(home, name, source),
    };
    // The copy of the folder the workspace was over goes once nothing is over it, as
    // does a new one a failed move made. A mount that moved keeps the old copy until
    // it ends, for files it still has open (see `Home::hold_copy`).
    if frozen && let Ok(_creating) = home.repos_lock(true) {
        let _ = home.remove_unused_copies();
    }
    updated
}

/// Whether the workspace of `record` is over a copy of a folder as it was (see `fork`).
fn is_copy(home: &Home, record: &Record) -> bool {
    matches!(&record.base, Base::Dir { path } if home.holds_copy(path))
}

fn move_onto(home: &Home, name: &str, source: Option<&str>) -> Result<Updated> {
    let _writer = home.lock(name, "sandfs update")?;
    // Reading the sources: `sandfs clean` waits.
    let _repos = home.repos_lock(false)?;
    let next = next_source(home, &home.record(name)?, source)?;
    apply(home, name, next, source.is_some())
}

/// Find the newer source here, where a relative path means what it says, and have the
/// process serving the mount move the workspace onto it.
#[cfg(mount)]
fn move_mounted(home: &Home, name: &str, source: Option<&str>) -> Result<Updated> {
    // Held until the mount's record names the new source, so `sandfs clean` can't
    // delete a copy just frozen for it in between.
    let repos = home.repos_lock(false)?;
    let next = next_source(home, &home.record(name)?, source)?;
    let updated = crate::server::update(home, name, &next, source.is_some())?;
    drop(repos);
    // macOS's NFS client asks for a folder's attributes again only once a second has
    // passed since it last did, and only then sees the names that changed.
    #[cfg(nfs)]
    std::thread::sleep(std::time::Duration::from_millis(1050));
    Ok(updated)
}

#[cfg(not(mount))]
fn move_mounted(_home: &Home, name: &str, _source: Option<&str>) -> Result<Updated> {
    bail!("{name} is mounted; unmount it first (sandfs unmount {name}), then update it")
}

/// The source to move the workspace of `record` onto: `source`, or the newest version
/// of the one it was made from.
fn next_source(home: &Home, record: &Record, source: Option<&str>) -> Result<Record> {
    let next = match source {
        Some(source) => home.resolve(source)?,
        None => home.resolve(&recorded_source(record))?,
    };
    // A copy of a folder as it was moves onto a new copy of the folder as it is now.
    Ok(match &next.base {
        Base::Dir { path } if is_copy(home, record) => home.freeze(path)?,
        _ => next,
    })
}

/// Move the workspace `name`, which the caller holds, onto `next`, which names it from
/// now on if `given`, the source having been named. The caller also holds the sources
/// (see [`Home::repos_lock`]).
pub fn apply(home: &Home, name: &str, next: Record, given: bool) -> Result<Updated> {
    let mut record = home.record(name)?;
    // Which files the newer source changed, when the source can say; a folder can't,
    // so its files are compared with the older folder's as needed.
    let (upstream, commits, before) = match (&record.base, &next.base) {
        (Base::Dir { path: old }, Base::Dir { path: new }) => {
            if old == new {
                bail!(
                    "{name} reads {} as it is now, so there is nothing newer to move to; name another folder",
                    old.display()
                );
            }
            (None, None, Some(open_base(&record.base, home.root())?))
        }
        (Base::Git { repo, commit: from, .. }, Base::Git { repo: to_repo, commit: to, branch }) => {
            if repo != to_repo {
                bail!("{} is another repository; a workspace moves only to another commit of its own", next.source);
            }
            let commits = (repo.clone(), from.clone(), to.clone(), branch.clone());
            (Some(git::changed_between(repo, from, to)?), Some(commits), None)
        }
        _ => bail!("a workspace over a folder moves only to another folder, and one over a commit to another commit"),
    };
    let old = home.open(name)?;
    let after = open_base(&next.base, home.root())?;
    let full = |rel: &str| format!("{ROOT}/{rel}");
    // Whether the newer source changed `rel` or anything below it.
    let affected = |rel: &str| -> Result<bool> {
        match (&upstream, &before) {
            (Some(changed), _) => {
                let below = format!("{rel}/");
                Ok(changed.contains(rel)
                    || changed.range(below.clone()..).next().is_some_and(|p| p.starts_with(&below)))
            }
            (None, Some(before)) => differs(before, &after, &full(rel)),
            (None, None) => Ok(true),
        }
    };

    let paths = old.saved.paths()?;
    let mut forget = BTreeSet::new();
    let mut both = Vec::new();
    let mut writes = Vec::new();
    // The commit between the old and the new one that a `git pull` in the workspace
    // checked out: a saved file as it has it came from upstream, not the workspace.
    let mut pulled = None;
    let is_git = |rel: &str| commits.is_some() && (rel == ".git" || rel.starts_with(".git/"));
    if let Some((repo, from, to, branch)) = &commits {
        // A URL's local copy is sandfs's to fetch into; a repository on this machine
        // is only read.
        let may_fetch = repo.starts_with(home.root().join("repos"));
        let mv = git::Move { repo, from, to, branch: branch.as_deref(), may_fetch };
        // The branch a kept `HEAD` points to, and the branches the workspace keeps.
        let (mut head, mut branches) = (None, BTreeSet::new());
        let loose =
            |name: &str| old.ws.stat(&full(&format!(".git/refs/heads/{name}"))).is_ok_and(|meta| !meta.kind.is_dir());
        for (rel, state) in paths.iter().filter(|(rel, _)| is_git(rel)) {
            let path = full(rel);
            if *state != PathState::File || old.ws.stat(&path)?.kind == Kind::Symlink {
                continue;
            }
            let data = old.ws.read(&path)?;
            let moved = mv.file(rel, &data, &loose)?;
            match rel.as_str() {
                ".git/HEAD" => head = git::head_branch(&data),
                ".git/packed-refs" => branches.extend(git::packed_branches(&data)),
                _ => branches.extend(rel.strip_prefix(".git/refs/heads/").map(str::to_owned)),
            }
            match moved {
                git::Moved::Keep => {}
                git::Moved::Forget => {
                    forget.insert(rel.clone());
                }
                git::Moved::Write(data) => writes.push((path, data)),
            }
        }
        // A `HEAD` on the branch the old commit was mounted as, which the workspace has
        // no file of its own for, would point at nothing once the new commit is mounted
        // as another: the new commit's own `HEAD` serves instead.
        if let Some(head) = head
            && branch.as_deref() != Some(head.as_str())
            && !branches.contains(&head)
        {
            forget.insert(".git/HEAD".to_owned());
        }
        pulled = mv.pulled(&old.ws)?;
    }
    // Whether the workspace's version of `rel` is one a pull brought in.
    let from_pull = |rel: &str, mine: Option<&Metadata>| -> Result<bool> {
        let (Some((repo, ..)), Some(commit)) = (&commits, &pulled) else { return Ok(false) };
        let path = full(rel);
        let mine = match mine.map(|meta| meta.kind) {
            None => None,
            Some(Kind::Dir) => Some(("040000", String::new())),
            Some(Kind::Symlink) => Some(("120000", git::blob_id(repo, old.ws.read_link(&path)?.as_bytes())?)),
            Some(Kind::File { executable }) => {
                Some((if executable { "100755" } else { "100644" }, git::blob_id(repo, &old.ws.read(&path)?)?))
            }
        };
        let theirs = git::entry_at(repo, commit, rel)?;
        Ok(match (theirs, mine) {
            // A folder is the pull's if the commit has one there; what is in it is
            // looked at name by name.
            (Some((mode, _)), Some(("040000", _))) => mode == "040000",
            (theirs, mine) => {
                theirs.as_ref().map(|(mode, id)| (mode.as_str(), id)) == mine.as_ref().map(|(mode, id)| (*mode, id))
            }
        })
    };
    // Folders the workspace saved; what becomes of them waits on what is inside them.
    let mut dirs = Vec::new();
    // Files a pull changed that the newer commit has as the older one did: forgotten,
    // they change back, though the newer commit didn't change them.
    let mut reverted = BTreeSet::new();

    for (rel, state) in paths.iter().filter(|(rel, _)| !is_git(rel)) {
        let path = full(rel);
        if !affected(rel)? {
            // A file a pull brought in that the newer commit has as the older one did,
            // as when upstream undid it since, is upstream's all the same.
            let mine = match state {
                PathState::Dir => {
                    dirs.push((rel, false));
                    continue;
                }
                PathState::Absent => None,
                PathState::File => Some(old.ws.stat(&path)?),
            };
            if pulled.is_some() && from_pull(rel, mine.as_ref())? {
                forget.insert(rel.clone());
                reverted.insert(rel.clone());
            }
            continue;
        }
        let now = stat(&after, &path)?;
        let matches = match state {
            PathState::Absent => now.is_none() || from_pull(rel, None)?,
            PathState::Dir => {
                dirs.push((rel, now.is_some_and(|meta| !meta.kind.is_dir())));
                continue;
            }
            PathState::File => {
                let mine = old.ws.stat(&path)?;
                let same_now = match now {
                    Some(theirs) => same(&old.ws, &after, &path, &mine, &theirs)?,
                    None => false,
                };
                same_now || from_pull(rel, Some(&mine))?
            }
        };
        if matches {
            forget.insert(rel.clone());
        } else {
            both.push(rel.clone());
        }
    }
    // A hard link keeps the name it shares a file with, even where the newer source
    // has the same bytes under that name. A name unchanged in the source would share
    // the newer source's file, so a source that changed it is refused.
    let saved: BTreeSet<&str> = paths.iter().map(|(rel, _)| rel.as_str()).collect();
    for (rel, other) in old.saved.links()? {
        forget.remove(&rel);
        if forget.remove(&other) || saved.contains(other.as_str()) {
            continue;
        }
        if affected(&other)? {
            bail!("{rel} is a hard link to {other}, which the newer source changed; remove one of them first");
        }
    }
    // A folder a pull made goes once nothing the workspace keeps is inside it; one of
    // the workspace's own stays a folder, and only a file in its place clashes. The
    // innermost come first, so a folder knows what became of those inside it.
    for (rel, clashes) in dirs.into_iter().rev() {
        let below = format!("{rel}/");
        let kept_inside = saved
            .range(below.as_str()..)
            .take_while(|path| path.starts_with(&below))
            .any(|path| !forget.contains(*path));
        if !kept_inside && from_pull(rel, Some(&old.ws.stat(&full(rel))?))? {
            forget.insert(rel.clone());
        } else if clashes {
            both.push(rel.clone());
        }
    }
    both.sort();
    // A change kept below a folder of the base that the newer source has as a file
    // could not be laid over it, unless the workspace's own entry for a folder above
    // replaces that file.
    for (rel, _) in paths.iter().filter(|(rel, _)| !is_git(rel) && !forget.contains(rel)) {
        let mut above = String::new();
        for part in rel.split('/').collect::<Vec<_>>().split_last().map_or(&[][..], |(_, parents)| parents) {
            if !above.is_empty() {
                above.push('/');
            }
            above.push_str(part);
            if saved.contains(above.as_str()) && !forget.contains(&above) {
                break;
            }
            if stat(&after, &full(&above))?.is_some_and(|meta| !meta.kind.is_dir()) {
                bail!(
                    "{above} is a file in the newer source, but the workspace changed {rel} inside the folder it \
                     was; move or remove {rel} first"
                );
            }
        }
    }
    let matched = forget.iter().filter(|rel| !rel.starts_with(".git/")).count();

    if let Some(changed) = &upstream {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        let now = (now.as_secs(), now.subsec_nanos());
        let is_file = |rel: &str| stat(&after, &full(rel)).map(|meta| meta.is_some_and(|meta| !meta.kind.is_dir()));
        let mut kept = std::mem::take(&mut record.updated);
        let gone = kept.keys().filter(|rel| !changed.contains(*rel)).cloned().collect::<Vec<_>>();
        for rel in gone {
            if !is_file(&rel)? {
                kept.remove(&rel);
            }
        }
        let reverted = reverted.iter().filter(|rel| forget.contains(*rel));
        for rel in changed.iter().chain(reverted) {
            if is_file(rel)? {
                kept.insert(rel.clone(), now);
            } else {
                kept.remove(rel);
            }
        }
        record.updated = kept;
    }
    let changed = upstream.as_ref().map(|changed| {
        let written = writes.iter().filter_map(|(path, _)| path.strip_prefix(&full("")).map(str::to_owned));
        changed.iter().chain(&forget).cloned().chain(written).collect()
    });
    let updated = Updated {
        from: describe(&record.base),
        to: describe(&next.base),
        both,
        upstream: upstream.as_ref().map(BTreeSet::len),
        matched,
        changed,
    };

    // The record goes first: if the rest fails, the saved changes that would have
    // been forgotten match the new base and simply count as changes still.
    record.base = next.base;
    if given {
        record.source = next.source;
    }
    home.set_record(name, &record)?;
    let mut saved = old.saved;
    saved.forget(&forget)?;
    drop(saved);
    if !writes.is_empty() {
        let mut opened = home.open(name)?;
        // Paths made read-only guard against the workspace's own writes, not these;
        // they are guarded again when it is next opened.
        for path in &record.read_only {
            opened.ws.set_read_only(&full(path), false)?;
        }
        for (path, data) in writes {
            opened.ws.write(&path, data)?;
        }
        let snapshot =
            Snapshot { ws: opened.ws.fork(), times: opened.restored.times.clone(), dirs: opened.restored.dirs.clone() };
        opened.saved.save(&snapshot, &VPath::parse(ROOT)?, true)?;
    }
    Ok(updated)
}

/// The source to look for a newer version of, as the workspace was made from it but
/// independent of the folder `update` runs in: a relative `git:../repo@main` becomes
/// the repository's path with the same revision. A folder is the one it was made from.
fn recorded_source(record: &Record) -> String {
    match &record.base {
        Base::Dir { .. } => record.source.clone(),
        Base::Git { repo, .. } => match git::local_rev(&record.source) {
            Some(rev) => format!("git:{}{}", repo.display(), rev.map(|rev| format!("@{rev}")).unwrap_or_default()),
            None => record.source.clone(),
        },
    }
}

fn describe(base: &Base) -> String {
    match base {
        Base::Dir { path } => path.display().to_string(),
        Base::Git { commit, .. } => commit[..commit.len().min(12)].to_owned(),
    }
}

/// What `path` is in `ws`, or none when nothing is there.
fn stat(ws: &Workspace, path: &str) -> Result<Option<Metadata>> {
    match ws.stat(path) {
        Ok(meta) => Ok(Some(meta)),
        Err(Error::NotFound(_) | Error::NotADirectory(_)) => Ok(None),
        Err(err) => Err(err.into()),
    }
}

/// Whether `path`, `mine` in `a` and `theirs` in `b`, is the same file, link or folder
/// in both. A folder's contents aren't compared.
fn same(a: &Workspace, b: &Workspace, path: &str, mine: &Metadata, theirs: &Metadata) -> Result<bool> {
    if mine.kind != theirs.kind {
        return Ok(false);
    }
    Ok(match mine.kind {
        Kind::Dir => true,
        Kind::Symlink => a.read_link(path)? == b.read_link(path)?,
        Kind::File { .. } => mine.size == theirs.size && a.read(path)? == b.read(path)?,
    })
}

/// Whether anything at or below `path` differs between `a` and `b`.
fn differs(a: &Workspace, b: &Workspace, path: &str) -> Result<bool> {
    match (stat(a, path)?, stat(b, path)?) {
        (None, None) => Ok(false),
        (Some(x), Some(y)) if x.kind.is_dir() && y.kind.is_dir() => {
            let names = |ws: &Workspace| -> Result<BTreeSet<String>> {
                Ok(ws.list(path)?.into_iter().map(|entry| entry.name).collect())
            };
            for name in names(a)?.union(&names(b)?) {
                if differs(a, b, &format!("{path}/{name}"))? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        (Some(x), Some(y)) => Ok(!same(a, b, path, &x, &y)?),
        _ => Ok(true),
    }
}
