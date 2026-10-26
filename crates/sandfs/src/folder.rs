//! Forks kept as real folders (see `fork`). On macOS every file operation in a mount
//! is a round trip to its NFS server, which makes work on many small files several
//! times slower than on the disk. So there a fork of a git clone is a plain folder
//! instead: clones of the clone's files, which cost no time or space for their contents
//! on APFS, and a `.git` of its own, as a mount would show it.
//!
//! The workspace stays, and is brought up to date with the folder whenever it is read
//! (`changes`, `fork`), so whatever reads workspaces works on these too. What only a
//! mount can do, a folder can't: keep paths read-only, and confine `sandfs shell`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sandfs_core::{Kind, VPath, Workspace};

use crate::git;
use crate::persist::{Restored, Snapshot};
use crate::store::{self, Base, Home, ROOT, check_name};

/// Names the workspace a folder belongs to, so the folder's path finds it.
const MARKER: &str = ".git/sandfs-workspace";

/// The workspace's own repository over the folder (see [`git::make_folder`]).
const PRIVATE: &str = "folder-git";

/// Keep the new workspace `name`, a fork of the files in `top`, as the real folder
/// `dir`, which must be new or empty. `index_from` is the git index of `top`, if any.
/// Returns the folder, resolved. On failure the folder is removed again, and the
/// caller removes the workspace.
pub fn make(home: &Home, name: &str, top: &Path, dir: &Path, index_from: Option<&Path>) -> Result<PathBuf> {
    let target = crate::resolve_new(dir)?;
    if target.is_dir() && std::fs::read_dir(&target)?.next().is_some() {
        bail!("{} is not empty; a fork kept as a folder needs a new or empty one", target.display());
    }
    let home_dir = crate::resolve_new(home.root())?;
    if home_dir.starts_with(&target) || target.starts_with(&home_dir) {
        bail!("{} overlaps sandfs's own state ({}); pick another folder", target.display(), home_dir.display());
    }
    std::fs::create_dir_all(&target).with_context(|| format!("creating {}", target.display()))?;
    let dir = target.canonicalize()?;
    let made = (|| {
        let mut record = home.record(name)?;
        let Base::Git { repo, commit, branch } = record.base.clone() else {
            bail!("only a fork of a git repository can be kept as a folder");
        };
        record.folder = Some(dir.clone());
        home.set_record(name, &record)?;
        let opened = home.open(name)?;
        clone_files(&opened.ws, top, &dir)?;
        let private = home.workspace_dir(name).join(PRIVATE);
        git::make_folder(&opened.ws, (&repo, &commit, branch.as_deref()), &dir, &private, index_from)?;
        std::fs::write(dir.join(MARKER), name).context("marking the folder")
    })();
    if let Err(err) = made {
        let _ = store::remove_copy(&dir);
        return Err(err);
    }
    Ok(dir)
}

/// Make each file of `ws`, but its `.git`, in the empty folder `dir`: a clone of the
/// same file in `top`, with its time and mode, when that is a file of the same size
/// and mode, else written from `ws`.
fn clone_files(ws: &Workspace, top: &Path, dir: &Path) -> Result<()> {
    let root = VPath::parse(ROOT)?;
    let mut folders = vec![(root.clone(), dir.to_path_buf())];
    while let Some((at, to)) = folders.pop() {
        for entry in ws.list(at.as_str())? {
            if at == root && entry.name == ".git" {
                continue;
            }
            let path = at.join(&entry.name)?;
            let rel = path.strip_prefix(&root).unwrap_or_default().join("/");
            let (host, copy) = (top.join(&rel), to.join(&entry.name));
            match entry.kind {
                Kind::Dir => {
                    std::fs::create_dir(&copy).with_context(|| format!("creating {}", copy.display()))?;
                    folders.push((path, copy));
                }
                Kind::Symlink => store::symlink(Path::new(&ws.read_link(path.as_str())?), &copy)?,
                Kind::File { executable } => {
                    let size = ws.stat(path.as_str())?.size;
                    let same = std::fs::symlink_metadata(&host)
                        .is_ok_and(|meta| meta.is_file() && meta.len() == size && is_executable(&meta) == executable);
                    if same && reflink_copy::reflink_or_copy(&host, &copy).is_ok() {
                        store::keep_times(&host, &copy)?;
                    } else {
                        std::fs::write(&copy, ws.read(path.as_str())?)
                            .with_context(|| format!("writing {}", copy.display()))?;
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::PermissionsExt;
                            let mode = if executable { 0o755 } else { 0o644 };
                            std::fs::set_permissions(&copy, std::fs::Permissions::from_mode(mode))?;
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// Bring the workspace `name` up to date with its folder `dir`: whatever git finds
/// changed there since the commit, and whatever the workspace had changed, is made in
/// the workspace as it is in the folder now, and saved.
pub fn sync(home: &Home, name: &str, dir: &Path) -> Result<()> {
    if find(home, dir).as_deref() != Some(name) {
        bail!("{name}'s folder {} is gone; `sandfs rm {name}` deletes the workspace", dir.display());
    }
    let _writer = home.lock(name, &format!("sandfs, reading the changes in {}", dir.display()))?;
    let listed = git::folder_changes(&home.workspace_dir(name).join(PRIVATE), dir)?;
    let mut opened = home.open(name)?;
    let root = VPath::parse(ROOT)?;
    let git_dir = VPath::parse(&format!("{ROOT}/.git"))?;
    // The workspace's own changes too, so a file changed back is noticed.
    let mut candidates: BTreeSet<String> = listed.into_iter().collect();
    for change in opened.ws.changes()? {
        if !change.path.starts_with(&git_dir) {
            candidates.insert(change.path.strip_prefix(&root).unwrap_or_default().join("/"));
        }
    }
    // Deleted files first, so a folder that replaced a file is made after the file is gone.
    let (mut gone, mut changed) = (Vec::new(), Vec::new());
    for rel in candidates {
        let at = format!("{ROOT}/{rel}");
        match git::in_folder(dir, &rel)? {
            None if opened.ws.exists(&at) => gone.push(rel),
            None => {}
            // Pipes, sockets and devices stay out, as when the fork was made.
            Some(meta) if !meta.is_file() && !meta.is_dir() && !meta.file_type().is_symlink() => {}
            Some(meta) if !same(&opened.ws, &at, &meta, &dir.join(&rel))? => changed.push(rel),
            Some(_) => {}
        }
    }
    if gone.is_empty() && changed.is_empty() {
        return Ok(());
    }
    gone.append(&mut changed);
    let Restored { mut times, mut dirs } = std::mem::take(&mut opened.restored);
    git::copy_changed(&mut opened.ws, dir, &gone, &mut times, &mut dirs)?;
    opened.saved.save(&Snapshot { ws: opened.ws.fork(), times, dirs }, &root, true)
}

/// Whether `at` in `ws` is what `host`, of metadata `meta`, is.
fn same(ws: &Workspace, at: &str, meta: &std::fs::Metadata, host: &Path) -> Result<bool> {
    let Ok(stat) = ws.stat(at) else { return Ok(false) };
    Ok(match stat.kind {
        Kind::Dir => meta.is_dir(),
        Kind::Symlink => {
            meta.file_type().is_symlink() && Some(ws.read_link(at)?.as_str()) == std::fs::read_link(host)?.to_str()
        }
        Kind::File { executable } => {
            meta.is_file()
                && meta.len() == stat.size
                && is_executable(meta) == executable
                && ws.read(at)? == std::fs::read(host)?
        }
    })
}

fn is_executable(meta: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    return std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o111 != 0;
    #[cfg(not(unix))]
    {
        // Windows keeps no executable bit.
        let _ = meta;
        false
    }
}

/// The git index of the folder of workspace `name`, which a fork of it starts from.
pub fn private_index(home: &Home, name: &str) -> PathBuf {
    home.workspace_dir(name).join(PRIVATE).join("index")
}

/// The workspace kept as the folder `what`, if it is one.
pub fn find(home: &Home, what: &Path) -> Option<String> {
    let dir = what.canonicalize().ok()?;
    let name = std::fs::read_to_string(dir.join(MARKER)).ok()?;
    check_name(&name).ok()?;
    // The marker is in the folder, which anything can write to; the record says.
    (home.record(&name).ok()?.folder? == dir).then_some(name)
}

/// The files among the `.git` settings of the folder `dir` of workspace `name` that
/// changed since the fork was made: `.git/config`, and every hook, since git runs the
/// programs they name.
pub fn git_settings(home: &Home, name: &str, dir: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let made = std::fs::read(home.workspace_dir(name).join(PRIVATE).join(git::CHECKOUT_CONFIG)).ok();
    if made.is_none() || made != std::fs::read(dir.join(".git/config")).ok() {
        found.push(".git/config".to_owned());
    }
    let hooks = dir.join(".git/hooks");
    let mut folders = vec![hooks.clone()];
    while let Some(folder) = folders.pop() {
        for entry in std::fs::read_dir(&folder).into_iter().flatten().flatten() {
            let path = entry.path();
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                folders.push(path);
            } else if let Ok(rel) = path.strip_prefix(&hooks) {
                found.push(format!(".git/hooks/{}", rel.to_string_lossy()));
            }
        }
    }
    found.sort();
    found
}

/// Delete the folder `dir` of workspace `name`, if it still is that workspace's.
pub fn remove(home: &Home, name: &str, dir: &Path) -> Result<()> {
    if find(home, dir).as_deref() == Some(name) {
        store::remove_copy(dir)?;
    }
    Ok(())
}
