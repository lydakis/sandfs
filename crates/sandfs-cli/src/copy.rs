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
/// Nothing is written through a link in `dir`: the items come from whoever wrote the
/// workspace, and a link they made earlier in the copy must not carry a later write
/// out of `dir`. Nor is anything written into a `.git` folder, where git keeps
/// settings that name programs it runs; those items are left out, and their number
/// returned.
pub fn copy_to(ws: &Workspace, items: &[Item], dir: &Path) -> Result<usize> {
    let (git, items): (Vec<&Item>, Vec<&Item>) = items.iter().partition(|item| item.rel.split('/').any(is_git_dir));
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let staging = tempfile::Builder::new()
        .prefix(".sandfs-copy-")
        .tempdir_in(dir)
        .with_context(|| format!("making room in {}", dir.display()))?;
    let mut staged = Vec::new();
    for item in &items {
        inside(dir, item.rel)?;
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
        let target = inside(dir, item.rel)?;
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

/// `dir` joined with `rel`, refused when a folder on the way is a link.
fn inside(dir: &Path, rel: &str) -> Result<PathBuf> {
    let mut path = dir.to_path_buf();
    let parts: Vec<&str> = rel.split('/').collect();
    for (i, part) in parts.iter().enumerate() {
        // A name this system would read as more than one name (`a\b` or `C:` on
        // Windows) could lead out of `dir`.
        if !matches!(Path::new(part).components().collect::<Vec<_>>()[..], [Component::Normal(name)] if name == *part) {
            bail!("not copying {rel}: {part:?} isn't a file name here");
        }
        path.push(part);
        let last = i + 1 == parts.len();
        if !last && std::fs::symlink_metadata(&path).is_ok_and(|meta| meta.file_type().is_symlink()) {
            bail!("not copying {rel}: {} is a link", path.display());
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
    fn a_file_larger_than_a_chunk_is_copied_whole() {
        let mut ws = Workspace::new();
        ws.mount("/mnt", Arc::new(MemorySource::from_files(Vec::<(&str, &str)>::new()).unwrap())).unwrap();
        let data: Vec<u8> = (0..CHUNK as usize * 2 + 5).map(|i| (i % 251) as u8).collect();
        ws.write("/mnt/big.bin", data.clone()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = VPath::parse("/mnt/big.bin").unwrap();
        copy_to(&ws, &[Item { path: &path, rel: "big.bin", kind: ChangeKind::Added }], dir.path()).unwrap();
        assert!(std::fs::read(dir.path().join("big.bin")).unwrap() == data);
    }
}
