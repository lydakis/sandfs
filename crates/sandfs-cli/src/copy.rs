//! `sandfs changes NAME --copy-to DIR`: write a workspace's changes into a folder on
//! disk, the one way a change leaves a workspace, and only where the caller points.

use std::io::{ErrorKind, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use sandfs_core::{ChangeKind, Kind, VPath, Workspace};

/// One change to copy: its path in the workspace, its path relative to the copy's
/// folder, and what happened to it.
pub struct Item<'a> {
    pub path: &'a VPath,
    pub rel: &'a str,
    pub kind: ChangeKind,
}

/// Make `dir` hold what `ws` holds at each item: added and modified files and links
/// are written, deleted ones removed, along with folders a deletion leaves empty that
/// `ws` no longer has. Deletions go first, so a file may replace a folder it deleted.
///
/// Every file and link is made before `dir` changes, in a folder inside it, so an error
/// while reading (a save replacing what the copy reads, after which the caller
/// retries) leaves `dir` as it was.
///
/// With `start`, what the changes were made to, nothing in `dir` that differs from
/// both its state there and its state in `ws` is replaced or removed: that is a file
/// changed in `dir` since, which the copy would lose. Such files refuse the whole copy
/// before anything is written. A file `dir` lacks is always written.
///
/// Nothing is written through a link in `dir`: the items come from whoever wrote the
/// workspace, and a link they made earlier in the copy must not carry a later write
/// out of `dir`. Nor is anything written into a `.git` folder, where git keeps
/// settings that name programs it runs; those items are left out, and their number
/// returned.
pub fn copy_to(ws: &Workspace, start: Option<&Workspace>, items: &[Item], dir: &Path) -> Result<usize> {
    let (git, items): (Vec<&Item>, Vec<&Item>) = items.iter().partition(|item| item.rel.split('/').any(is_git_dir));
    if let Some(start) = start {
        let mut changed = Vec::new();
        for item in &items {
            let Some(target) = target(dir, item)? else { continue };
            let kept = |from: &Workspace| same(from, item.path, &target);
            if !matches!(on_disk(&target)?, OnDisk::Absent) && !kept(start)? && !kept(ws)? {
                changed.push(item.rel);
            }
        }
        if !changed.is_empty() {
            bail!(
                "not copying: {} changed in {} since the workspace started, and the copy would lose that: {}; \
                 pass --force to copy anyway",
                if changed.len() == 1 { "this file was" } else { "these files were" },
                dir.display(),
                changed.join(", ")
            );
        }
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let staging = tempfile::Builder::new()
        .prefix(".sandfs-copy-")
        .tempdir_in(dir)
        .with_context(|| format!("making room in {}", dir.display()))?;
    let mut staged = Vec::new();
    for item in &items {
        target(dir, item)?;
        if item.kind == ChangeKind::Deleted {
            continue;
        }
        let meta = ws.stat(item.path.as_str())?;
        let file = staging.path().join(staged.len().to_string());
        match meta.kind {
            Kind::Dir => bail!("{} is a folder, which changes never lists", item.path),
            Kind::Symlink if !cfg!(unix) => bail!("{} is a symbolic link, which this system can't copy", item.path),
            Kind::Symlink => symlink(&ws.read_link(item.path.as_str())?, &file)?,
            Kind::File { executable } => {
                write_file(ws, item.path.as_str(), meta.size, &file)
                    .with_context(|| format!("reading {}", item.path))?;
                set_executable(&file, executable)?;
            }
        }
        staged.push((item, file));
    }
    for item in items.iter().filter(|item| item.kind == ChangeKind::Deleted) {
        let Some(target) = target(dir, item)? else { continue };
        // A folder in place of the deleted file replaced it, as in the workspace (the
        // check above made sure), and stays.
        if std::fs::symlink_metadata(&target).is_ok_and(|meta| meta.is_dir()) {
            continue;
        }
        match std::fs::remove_file(&target) {
            Ok(()) => {}
            Err(err) if err.kind() == ErrorKind::NotFound => {}
            Err(err) => return Err(err).with_context(|| format!("removing {}", target.display())),
        }
        let (mut folder, mut path) = (target.parent(), item.path.parent());
        while let (Some(on_disk), Some(in_ws)) = (folder, path) {
            if on_disk == dir || ws.exists(in_ws.as_str()) || std::fs::remove_dir(on_disk).is_err() {
                break;
            }
            (folder, path) = (on_disk.parent(), in_ws.parent());
        }
    }
    for (item, file) in staged {
        let target = inside(dir, item.rel)?;
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        clear(&target)?;
        std::fs::rename(&file, &target).with_context(|| format!("writing {}", target.display()))?;
    }
    Ok(git.len())
}

/// Whether a folder named `name` could be git's own on some disk: `.git` in any case,
/// since a Mac or Windows disk usually ignores case; with the invisible characters a
/// Mac disk ignores; up to a Windows stream name (`.git::$INDEX_ALLOCATION`) and
/// without the dots and spaces Windows drops from a name's end; or as `.git`'s
/// Windows short name, `git~1`. Git's own `core.protectHFS` and `core.protectNTFS`
/// refuse the same names.
fn is_git_dir(name: &str) -> bool {
    let visible: String = name.chars().filter(|c| !is_ignored_by_hfs(*c)).collect();
    let name = visible.split(':').next().unwrap_or_default().trim_end_matches(['.', ' ']).to_ascii_lowercase();
    name == ".git" || name == "git~1"
}

/// The characters HFS+ leaves out when it compares names.
fn is_ignored_by_hfs(c: char) -> bool {
    matches!(c, '\u{200c}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{206a}'..='\u{206f}' | '\u{feff}')
}

/// What is at a path on disk, without following a link there.
enum OnDisk {
    Absent,
    File(std::fs::Metadata),
    Link(PathBuf),
    /// A folder, which only a change that replaces it with a file finds.
    Dir,
    /// Anything else, which is never what a workspace holds.
    Other,
}

fn on_disk(path: &Path) -> Result<OnDisk> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Ok(OnDisk::Link(std::fs::read_link(path)?)),
        Ok(meta) if meta.is_file() => Ok(OnDisk::File(meta)),
        Ok(meta) if meta.is_dir() => Ok(OnDisk::Dir),
        Ok(_) => Ok(OnDisk::Other),
        Err(err) if matches!(err.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => Ok(OnDisk::Absent),
        Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
    }
}

/// Whether `target` on disk holds what `ws` holds at `path`: nothing, the same link,
/// a folder holding nothing `ws` lacks there (each file the change deletes from it is
/// checked on its own), or a file with the same bytes. Modes don't count.
fn same(ws: &Workspace, path: &VPath, target: &Path) -> Result<bool> {
    let meta = match ws.stat(path.as_str()) {
        Ok(meta) => meta,
        Err(sandfs_core::Error::NotFound(_) | sandfs_core::Error::NotADirectory(_)) => {
            return Ok(matches!(on_disk(target)?, OnDisk::Absent));
        }
        Err(err) => return Err(err.into()),
    };
    Ok(match (meta.kind, on_disk(target)?) {
        (Kind::Dir, OnDisk::Dir) => nothing_more(ws, path, target)?,
        (Kind::Symlink, OnDisk::Link(link)) => link.to_str() == Some(ws.read_link(path.as_str())?.as_str()),
        (Kind::File { .. }, OnDisk::File(disk)) if disk.len() == meta.size => {
            let mut file = std::io::BufReader::new(std::fs::File::open(target)?);
            let mut offset = 0;
            while offset < meta.size {
                let chunk = ws.read_range(path.as_str(), offset, CHUNK.min(meta.size - offset))?;
                if chunk.is_empty() {
                    break;
                }
                let mut ours = vec![0; chunk.len()];
                std::io::Read::read_exact(&mut file, &mut ours)?;
                if ours != chunk {
                    return Ok(false);
                }
                offset += chunk.len() as u64;
            }
            true
        }
        _ => false,
    })
}

/// Whether everything in the folder `dir` on disk is also in `ws` at `path`, as the
/// same kind of thing: a link in place of a folder, which no deletion names, is a
/// change too.
fn nothing_more(ws: &Workspace, path: &VPath, dir: &Path) -> Result<bool> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else { return Ok(false) };
        let Ok(inner) = path.join(&name) else { return Ok(false) };
        let Ok(meta) = ws.stat(inner.as_str()) else { return Ok(false) };
        let kind = entry.file_type()?;
        let fits = match meta.kind {
            Kind::Dir => kind.is_dir() && nothing_more(ws, &inner, &entry.path())?,
            Kind::Symlink => kind.is_symlink(),
            Kind::File { .. } => kind.is_file(),
        };
        if !fits {
            return Ok(false);
        }
    }
    Ok(true)
}

/// How much of a file is read at once, so a large one is never held whole.
const CHUNK: u64 = 8 << 20;

fn write_file(ws: &Workspace, path: &str, size: u64, target: &Path) -> Result<()> {
    let mut file = std::io::BufWriter::new(std::fs::File::create(target)?);
    let mut offset = 0;
    while offset < size {
        let chunk = ws.read_range(path, offset, CHUNK.min(size - offset))?;
        if chunk.is_empty() {
            break;
        }
        file.write_all(&chunk)?;
        offset += chunk.len() as u64;
    }
    file.flush()?;
    Ok(())
}

/// Where `item` goes in `dir` (see [`inside`]). A file to delete below a link or a file
/// in `dir` is already gone from there, and has none.
fn target(dir: &Path, item: &Item) -> Result<Option<PathBuf>> {
    if item.kind != ChangeKind::Deleted {
        return inside(dir, item.rel).map(Some);
    }
    let path = inside_names(dir, item.rel)?;
    let mut below = path.ancestors().skip(1).take_while(|folder| *folder != dir);
    let blocked = below.any(|folder| std::fs::symlink_metadata(folder).is_ok_and(|meta| !meta.is_dir()));
    Ok((!blocked).then_some(path))
}

/// `dir` joined with `rel`, refused when a part isn't a plain file name.
fn inside_names(dir: &Path, rel: &str) -> Result<PathBuf> {
    let mut path = dir.to_path_buf();
    for part in rel.split('/') {
        // A name this system would read as more than one name (`a\b` or `C:` on
        // Windows) could lead out of `dir`.
        if !matches!(Path::new(part).components().collect::<Vec<_>>()[..], [Component::Normal(name)] if name == part) {
            bail!("not copying {rel}: {part:?} isn't a file name here");
        }
        path.push(part);
    }
    Ok(path)
}

/// `dir` joined with `rel`, refused when a folder on the way is a link.
fn inside(dir: &Path, rel: &str) -> Result<PathBuf> {
    let path = inside_names(dir, rel)?;
    for folder in path.ancestors().skip(1).take_while(|folder| *folder != dir) {
        if std::fs::symlink_metadata(folder).is_ok_and(|meta| meta.file_type().is_symlink()) {
            bail!("not copying {rel}: {} is a link", folder.display());
        }
    }
    Ok(path)
}

/// Make room at `target`: an old file or link goes, and so does an empty folder,
/// which is what a deletion leaves behind a file that replaces a folder.
fn clear(target: &Path) -> Result<()> {
    let Ok(meta) = std::fs::symlink_metadata(target) else { return Ok(()) };
    let removed = if meta.is_dir() { std::fs::remove_dir(target) } else { std::fs::remove_file(target) };
    removed.with_context(|| format!("replacing {}", target.display()))
}

#[cfg(unix)]
fn set_executable(path: &Path, executable: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut perms = std::fs::metadata(path)?.permissions();
    let mode = perms.mode();
    perms.set_mode(if executable { mode | (mode & 0o444) >> 2 } else { mode & !0o111 });
    std::fs::set_permissions(path, perms).with_context(|| format!("setting the mode of {}", path.display()))
}

#[cfg(not(unix))]
fn set_executable(_path: &Path, _executable: bool) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn symlink(target: &str, path: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, path).with_context(|| format!("linking {}", path.display()))
}

#[cfg(not(unix))]
fn symlink(_target: &str, path: &Path) -> Result<()> {
    bail!("{} is a symbolic link, which this system can't copy", path.display())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use sandfs_core::{MemorySource, VPath, Workspace};

    use super::*;

    #[test]
    fn git_folders_are_known_in_every_spelling() {
        for name in
            [".git", ".GIT", ".Git.", ".git ..", "GIT~1", ".g\u{200c}it", "\u{feff}.git", ".git::$INDEX_ALLOCATION"]
        {
            assert!(is_git_dir(name), "{name}");
        }
        for name in ["git", ".github", ".gitignore", "x.git", "~git", "git~backup", "git~", "git~2"] {
            assert!(!is_git_dir(name), "{name}");
        }
    }

    #[test]
    fn files_changed_where_they_are_copied_are_never_lost() {
        let source = MemorySource::from_files([
            ("notes.md", "draft\n"),
            ("todo.md", "a\n"),
            ("old/a.md", "a\n"),
            ("old/sub/b.md", "b\n"),
        ])
        .unwrap();
        let mut start = Workspace::new();
        start.mount("/mnt", Arc::new(source)).unwrap();
        let mut ws = start.fork();
        ws.write("/mnt/notes.md", "final\n").unwrap();
        ws.write("/mnt/todo.md", "b\n").unwrap();
        ws.write("/mnt/new.md", "new\n").unwrap();
        ws.remove("/mnt/old", true).unwrap();
        ws.write("/mnt/old", "a file now\n").unwrap();
        let paths = ["/mnt/new.md", "/mnt/notes.md", "/mnt/old", "/mnt/old/a.md", "/mnt/old/sub/b.md", "/mnt/todo.md"]
            .map(|path| VPath::parse(path).unwrap());
        let kinds = [
            ChangeKind::Added,
            ChangeKind::Modified,
            ChangeKind::Added,
            ChangeKind::Deleted,
            ChangeKind::Deleted,
            ChangeKind::Modified,
        ];
        let items: Vec<_> = paths
            .iter()
            .zip(kinds)
            .map(|(path, kind)| Item { path, rel: path.as_str().strip_prefix("/mnt/").unwrap(), kind })
            .collect();
        let folder = |notes: &str, extra: bool| {
            let dir = tempfile::tempdir().unwrap();
            std::fs::create_dir(dir.path().join("old")).unwrap();
            std::fs::write(dir.path().join("old/a.md"), "a\n").unwrap();
            std::fs::create_dir(dir.path().join("old/sub")).unwrap();
            std::fs::write(dir.path().join("old/sub/b.md"), "b\n").unwrap();
            std::fs::write(dir.path().join("notes.md"), notes).unwrap();
            // Already what the copy makes it.
            std::fs::write(dir.path().join("todo.md"), "b\n").unwrap();
            if extra {
                std::fs::write(dir.path().join("old/mine.md"), "mine\n").unwrap();
            }
            dir
        };

        // Untouched since the start: copied.
        let dir = folder("draft\n", false);
        copy_to(&ws, Some(&start), &items, dir.path()).unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("notes.md")).unwrap(), "final\n");
        assert_eq!(std::fs::read_to_string(dir.path().join("old")).unwrap(), "a file now\n");
        assert_eq!(std::fs::read_to_string(dir.path().join("new.md")).unwrap(), "new\n");

        // Edited there since, or given a file of its own in a folder the copy replaces:
        // refused, with nothing written.
        for (notes, extra, named) in [("mine\n", false, "notes.md"), ("draft\n", true, "old")] {
            let dir = folder(notes, extra);
            let err = copy_to(&ws, Some(&start), &items, dir.path()).unwrap_err().to_string();
            assert!(
                err.ends_with(&format!(
                    "since the workspace started, and the copy would lose that: {named}; pass --force to copy anyway"
                )),
                "{err}"
            );
            assert_eq!(std::fs::read_to_string(dir.path().join("notes.md")).unwrap(), notes);
            assert!(!dir.path().join("new.md").exists());
        }

        // So is a link put there in place of a folder the copy replaces.
        #[cfg(unix)]
        {
            let dir = folder("draft\n", false);
            std::fs::remove_dir_all(dir.path().join("old/sub")).unwrap();
            std::os::unix::fs::symlink("elsewhere", dir.path().join("old/sub")).unwrap();
            assert!(
                copy_to(&ws, Some(&start), &items, dir.path())
                    .unwrap_err()
                    .to_string()
                    .ends_with(": old; pass --force to copy anyway")
            );
            assert!(std::fs::symlink_metadata(dir.path().join("old/sub")).unwrap().file_type().is_symlink());
            assert!(!dir.path().join("new.md").exists());
        }

        // A deleted file edited there is refused too, and forcing copies anyway.
        let dir = folder("draft\n", false);
        std::fs::write(dir.path().join("old/a.md"), "edited\n").unwrap();
        assert!(copy_to(&ws, Some(&start), &items, dir.path()).unwrap_err().to_string().contains(": old/a.md;"));
        let dir = folder("mine\n", false);
        copy_to(&ws, None, &items, dir.path()).unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("notes.md")).unwrap(), "final\n");
    }

    #[test]
    fn a_file_larger_than_a_chunk_is_copied_whole() {
        let mut ws = Workspace::new();
        ws.mount("/mnt", Arc::new(MemorySource::from_files(Vec::<(&str, &str)>::new()).unwrap())).unwrap();
        let data: Vec<u8> = (0..CHUNK as usize * 2 + 5).map(|i| (i % 251) as u8).collect();
        ws.write("/mnt/big.bin", data.clone()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = VPath::parse("/mnt/big.bin").unwrap();
        copy_to(&ws, None, &[Item { path: &path, rel: "big.bin", kind: ChangeKind::Added }], dir.path()).unwrap();
        assert!(std::fs::read(dir.path().join("big.bin")).unwrap() == data);
    }
}
