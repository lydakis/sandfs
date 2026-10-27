//! Linked worktrees in a folder a workspace is over. A worktree's `.git` is a file that
//! names its repository by full path (`gitdir: /Users/me/repo/.git/worktrees/x`), so
//! git in a mount of the folder would read and write that repository on the host:
//! a commit there lands in the real repository while the files it was made from stay
//! in the workspace, and the real worktree looks changed. So a mount shows such a file
//! with the path its repository has in the mount: relative, when the repository is in
//! the folder too, as a link into the folder is (see `DirSource`); and otherwise as a
//! path that isn't there, so git stops with "not a git repository" and says why
//! instead of writing outside the workspace. A copy of the folder (`fork`) gets the
//! same file.

use std::path::{Component, Path, PathBuf};

use bytes::Bytes;
use sandfs_core::{DirSource, Kind, Listing, Result, Source, SourceEntry, SourceKey};

/// A `.git` file is a line or two; anything longer is not one to rewrite.
const MAX_GIT_FILE: u64 = 4096;

/// Where git is sent for a repository outside the folder: a relative path, so it is
/// looked for in the mount, where nothing has this name, and git says it isn't there.
const OUTSIDE: &str = ".sandfs: this worktree's repository is outside this folder, at ";

/// What a `.git` file at `at` (its folder, relative to the folder `top`) should say in
/// a mount or copy of `top`, or `None` to show it as it is: not a `gitdir:` line, or
/// one naming its repository by a relative path already.
pub fn rewrite(contents: &[u8], at: &Path, top: &Path) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(contents).ok()?;
    let named = text.trim_end_matches(['\n', '\r']).strip_prefix("gitdir:")?.trim();
    let repo = Path::new(named);
    if !repo.is_absolute() || named.contains('\n') {
        return None;
    }
    let inside = inside(repo, top).filter(|rel| rel.components().all(|part| matches!(part, Component::Normal(_))));
    Some(
        match inside {
            Some(rel) => format!("gitdir: {}\n", relative(at, &rel).display()),
            None => format!("gitdir: {OUTSIDE}{named}\n"),
        }
        .into_bytes(),
    )
}

/// `repo` relative to `top`, under any of the names they may have: as written, with
/// links resolved, and on macOS with or without `/private` before `/tmp` and `/var`.
fn inside(repo: &Path, top: &Path) -> Option<PathBuf> {
    let names = |path: &Path| {
        let mut names = vec![path.to_path_buf()];
        if let Ok(real) = path.canonicalize() {
            names.push(real);
        }
        let more: Vec<PathBuf> = names
            .iter()
            .flat_map(|name| match name.strip_prefix("/private") {
                Ok(rest) => Some(Path::new("/").join(rest)),
                Err(_) => Some(Path::new("/private").join(name.strip_prefix("/").ok()?)),
            })
            .collect();
        names.extend(more);
        names
    };
    let tops = names(top);
    names(repo)
        .into_iter()
        .find_map(|repo| tops.iter().find_map(|top| repo.strip_prefix(top).ok().map(Path::to_path_buf)))
}

/// The relative path from the folder `from` to `to`, both relative to one top.
fn relative(from: &Path, to: &Path) -> PathBuf {
    let (from, to): (Vec<_>, Vec<_>) = (from.components().collect(), to.components().collect());
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut path: PathBuf = from[common..].iter().map(|_| Component::ParentDir).collect();
    path.extend(&to[common..]);
    if path.as_os_str().is_empty() { PathBuf::from(".") } else { path }
}

/// A folder on disk as a mount shows it: [`DirSource`], with its linked worktrees'
/// `.git` files rewritten (see [`rewrite`]).
#[derive(Debug)]
pub struct FolderSource {
    files: DirSource,
    top: PathBuf,
}

impl FolderSource {
    pub fn new(top: &Path) -> Result<Self> {
        let files = DirSource::new(top)?;
        Ok(FolderSource { files, top: top.canonicalize().unwrap_or_else(|_| top.to_path_buf()) })
    }

    /// What the `.git` file at `key` shows instead of its own bytes, if anything.
    fn git_file(&self, key: &str, entry: Option<&SourceEntry>) -> Result<Option<Bytes>> {
        let path = Path::new(key);
        if path.file_name().is_none_or(|name| name != ".git") {
            return Ok(None);
        }
        if let Some(entry) = entry
            && (!matches!(entry.kind, Kind::File { .. }) || entry.size.is_some_and(|size| size > MAX_GIT_FILE))
        {
            return Ok(None);
        }
        let contents = self.files.read_range(key, 0, MAX_GIT_FILE + 1)?;
        if contents.len() as u64 > MAX_GIT_FILE {
            return Ok(None);
        }
        let at = path.parent().unwrap_or(Path::new(""));
        Ok(rewrite(&contents, at, &self.top).map(Bytes::from))
    }

    /// `entry`, with the size its file shows if it is a rewritten `.git` file.
    fn sized(&self, mut entry: SourceEntry) -> Result<SourceEntry> {
        if let Some(shown) = self.git_file(&entry.key.clone(), Some(&entry))? {
            entry.size = Some(shown.len() as u64);
        }
        Ok(entry)
    }
}

impl Source for FolderSource {
    fn root(&self) -> Result<SourceKey> {
        self.files.root()
    }

    fn read_dir(&self, key: &str) -> Result<Listing> {
        let mut listing = self.files.read_dir(key)?;
        if let Some(entry) = listing.remove(".git") {
            listing.insert(".git".to_owned(), self.sized(entry)?);
        }
        Ok(listing)
    }

    fn read_file(&self, key: &str) -> Result<Bytes> {
        match self.git_file(key, None)? {
            Some(shown) => Ok(shown),
            None => self.files.read_file(key),
        }
    }

    fn read_range(&self, key: &str, offset: u64, len: u64) -> Result<Bytes> {
        match self.git_file(key, None)? {
            Some(shown) => {
                let start = usize::try_from(offset).unwrap_or(usize::MAX).min(shown.len());
                let end = start.saturating_add(usize::try_from(len).unwrap_or(usize::MAX)).min(shown.len());
                Ok(shown.slice(start..end))
            }
            None => self.files.read_range(key, offset, len),
        }
    }

    fn prefetch(&self, key: &str, recursive: bool) {
        self.files.prefetch(key, recursive);
    }

    fn is_local(&self, key: &str) -> bool {
        self.files.is_local(key)
    }

    fn entry_now(&self, key: &str) -> Result<Option<SourceEntry>> {
        self.files.entry_now(key)?.map(|entry| self.sized(entry)).transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_repository_in_the_folder_is_named_from_the_worktree() {
        let top = Path::new("/work/proj");
        let said = rewrite(b"gitdir: /work/proj/repo/.git/worktrees/wt\n", Path::new("repo/.worktrees/wt"), top);
        assert_eq!(said.unwrap(), b"gitdir: ../../.git/worktrees/wt\n");
    }

    #[test]
    fn a_repository_elsewhere_is_named_where_nothing_is() {
        let said = rewrite(b"gitdir: /elsewhere/repo/.git/worktrees/wt\n", Path::new("wt"), Path::new("/work/proj"));
        let said = String::from_utf8(said.unwrap()).unwrap();
        assert_eq!(said, format!("gitdir: {OUTSIDE}/elsewhere/repo/.git/worktrees/wt\n"));
        // A neighbour whose name starts the same is elsewhere too.
        let said = rewrite(b"gitdir: /work/proj2/.git", Path::new(""), Path::new("/work/proj"));
        assert!(String::from_utf8(said.unwrap()).unwrap().contains(OUTSIDE));
    }

    #[test]
    fn relative_and_other_files_are_shown_as_they_are() {
        let top = Path::new("/work/proj");
        assert_eq!(rewrite(b"gitdir: ../.git/modules/lib\n", Path::new("lib"), top), None);
        assert_eq!(rewrite(b"not a git file", Path::new(""), top), None);
        assert_eq!(rewrite(&[0xff, 0xfe], Path::new(""), top), None);
    }

    #[test]
    fn macos_names_tmp_with_or_without_private() {
        let said =
            rewrite(b"gitdir: /private/tmp/proj/repo/.git/worktrees/wt", Path::new("wt"), Path::new("/tmp/proj"));
        assert_eq!(said.unwrap(), b"gitdir: ../repo/.git/worktrees/wt\n");
    }
}
