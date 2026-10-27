//! Git commits as sources: `git:<path or url>[@rev]`.
//!
//! Everything git-specific in the CLI lives here, so the rest of it only knows host
//! folders. Remote repositories become bare partial clones under `<home>/repos` that
//! hold only the commits that were mounted, without their history, so a mount costs
//! one request for the commit and its folder listings. File contents are fetched only
//! when a directory or file is first read.
//!
//! A git mount also shows a `.git` folder ([`GitDir`]), so git itself works inside it:
//! status, add, commit, branch and push. It borrows the local copy's objects without
//! writing to it, and tells git which files changed (`sandfs git-fsmonitor`), so git
//! never has to read the rest.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use sandfs_core::{Kind, Listing, Source, SourceEntry, SourceKey, VPath, Workspace};
use sandfs_git::{GitSource, ObjectIds};

use crate::github;
use crate::persist::{PathState, Restored, Snapshot};
use crate::store::{Base, Home, ROOT, expand};

/// Resolve `spec` (what follows `git:`) to a local repository and a commit id,
/// cloning or fetching remote repositories into `repos`.
pub fn resolve(repos: &Path, spec: &str) -> Result<(PathBuf, String)> {
    let (location, rev) = split_rev(spec);
    if location.is_empty() || rev.as_deref() == Some("") {
        bail!("incomplete source \"git:{spec}\"");
    }
    let rev = rev.as_deref().unwrap_or("HEAD");
    if rev.starts_with('-') {
        bail!("invalid revision {rev:?}");
    }
    if is_url(location) {
        return fetch_commit(repos, location, rev);
    }
    let repo = expand(location).canonicalize().with_context(|| format!("no repository at {location}"))?;
    Ok((repo.clone(), commit_of(&repo, rev)?))
}

/// The repository on this machine that `spec` (what follows `git:`) names, if it names
/// one rather than a URL.
pub fn local_repo(spec: &str) -> Option<PathBuf> {
    let (location, _) = split_rev(spec);
    (!location.is_empty() && !is_url(location)).then(|| expand(location).canonicalize().ok()).flatten()
}

fn commit_of(repo: &Path, rev: &str) -> Result<String> {
    git(repo, &["rev-parse", "--verify", "--end-of-options", &format!("{rev}^{{commit}}")])
        .with_context(|| format!("{rev:?} is not a commit in {}", repo.display()))
}

/// Like [`resolve`], and also the branch the commit was mounted as: the branch a
/// remote's default `HEAD` points to, or a revision that names a branch, when that
/// branch is at the commit. A commit id or a tag gives none.
pub fn resolve_branch(repos: &Path, spec: &str) -> Result<(PathBuf, String, Option<String>)> {
    let (location, rev) = split_rev(spec);
    let rev = rev.unwrap_or_else(|| "HEAD".to_owned());
    if !is_url(location) {
        let (repo, commit) = resolve(repos, spec)?;
        let branch = local_branch(&repo, &rev)
            .filter(|branch| commit_of(&repo, &format!("refs/heads/{branch}")).is_ok_and(|tip| tip == commit));
        return Ok((repo, commit, branch));
    }
    // Asking the server which branch it is takes a request of its own, so it runs
    // while the commit is fetched rather than after.
    std::thread::scope(|scope| {
        let lookup = (!is_commit_id(&rev)).then(|| scope.spawn(|| remote_branch(location, &rev)));
        let (repo, commit) = resolve(repos, spec)?;
        let branch = lookup
            .and_then(|lookup| lookup.join().ok().flatten())
            .and_then(|(branch, tip)| (tip == commit).then_some(branch));
        Ok((repo, commit, branch))
    })
}

/// The repository whose working folder is `top`, its `HEAD` commit, and the branch
/// checked out there, taking `top` as a path, never as a source with an `@revision`.
pub fn resolve_head(top: &Path) -> Result<(PathBuf, String, Option<String>)> {
    let repo = top.canonicalize().with_context(|| format!("no repository at {}", top.display()))?;
    // One git for both: the commit, then the full name `HEAD` points to, or `HEAD`
    // itself when it is detached. (`--short` gives `heads/main` when a tag is also
    // called `main`.)
    let out = git(&repo, &["rev-parse", "HEAD^{commit}", "--symbolic-full-name", "HEAD"])
        .with_context(|| format!("\"HEAD\" is not a commit in {}", repo.display()))?;
    let mut lines = out.lines();
    let commit = lines.next().filter(|id| is_commit_id(id)).context("git rev-parse printed no commit")?.to_owned();
    let branch = lines.next().and_then(|head| head.strip_prefix("refs/heads/")).map(str::to_owned);
    Ok((repo, commit, branch))
}

/// The branch `rev` names in a local repository, if any.
fn local_branch(repo: &Path, rev: &str) -> Option<String> {
    if rev == "HEAD" {
        // The full name: `--short` gives `heads/main` when a tag is also called `main`.
        let head = git(repo, &["symbolic-ref", "-q", "HEAD"]).ok()?;
        return head.strip_prefix("refs/heads/").map(str::to_owned);
    }
    let name = rev.strip_prefix("refs/heads/").unwrap_or(rev);
    git(repo, &["show-ref", "--verify", "-q", &format!("refs/heads/{name}")]).ok().map(|_| name.to_owned())
}

/// The branch `rev` names on the server at `url`, and the commit it is at.
fn remote_branch(url: &str, rev: &str) -> Option<(String, String)> {
    if rev == "HEAD" {
        // `ref: refs/heads/main\tHEAD`, then `<commit>\tHEAD`.
        let out = git_outside(&["ls-remote", "--symref", url, "HEAD"]).ok()?;
        let branch = out.lines().find_map(|line| line.strip_prefix("ref: refs/heads/")?.strip_suffix("\tHEAD"))?;
        let tip = out.lines().find_map(|line| line.strip_suffix("\tHEAD").filter(|id| is_commit_id(id)))?;
        return Some((branch.to_owned(), tip.to_owned()));
    }
    let name = rev.strip_prefix("refs/heads/").unwrap_or(rev);
    let full = format!("refs/heads/{name}");
    let out = git_outside(&["ls-remote", url, &full]).ok()?;
    let tip = out.lines().find_map(|line| line.strip_suffix(&format!("\t{full}")))?;
    Some((name.to_owned(), tip.to_owned()))
}

/// The source for `commit` of `repo`, and the `.git` folder that goes inside it, for
/// workspaces kept in the home at `home`.
pub fn open(
    repo: &Path,
    commit: &str,
    branch: Option<&str>,
    home: &Path,
) -> Result<(Arc<dyn Source>, Arc<dyn Source>)> {
    // Sizes let FUSE answer `stat` without reading whole files.
    let files = GitSource::open(repo, commit)?.with_sizes()?;
    let sizes = github::known_sizes(repo, files.tree_id());
    let files = Arc::new(files.with_known_sizes(sizes));
    let dir = GitDir::new(repo, commit, branch, home, Some(Arc::clone(&files)))?;
    Ok((files, Arc::new(dir)))
}

/// Make the new git workspace `name`, over the `HEAD` of a repository on this machine,
/// a copy of the repository's working folder (see `fork`): the files the folder has
/// changed since that commit, copied in once, and with `branch`, git on a branch of its
/// own at the commit.
///
/// Both are written into the workspace like any change, so the repository is only
/// read, and `changes` lists the copied files from the start. Returns the workspace as
/// saved, for whatever is made from it next.
pub fn start(home: &Home, name: &str, branch: Option<&str>) -> Result<Workspace> {
    let record = home.record(name)?;
    let Base::Git { repo, commit, .. } = &record.base else {
        bail!("{} is not a git repository", record.source);
    };
    // Checked before anything is written, so a refusal leaves nothing to undo.
    // A fork of a checkout is over the repository at its working folder (see `fork`).
    // Each git run costs its start-up, so the branch is checked while the files are read.
    let (top, changed) = std::thread::scope(|scope| {
        let checked = branch.map(|branch| scope.spawn(move || check_new_branch(repo, branch)));
        let changed = changed_files(repo, commit);
        if let Some(checked) = checked {
            checked.join().map_err(|_| anyhow!("checking the branch name failed"))??;
        }
        anyhow::Ok((repo.clone(), changed?))
    })?;

    let _writer = home.lock(name, "sandfs fork, preparing the workspace")?;
    let mut opened = home.open(name)?;
    let ws = &mut opened.ws;
    // Read-only paths keep programs from changing the copy, not the copy from being
    // made; the mount applies them again from the record.
    for path in &record.read_only {
        ws.set_read_only(&format!("{ROOT}/{path}"), false)?;
    }
    let (mut times, mut dirs) = (BTreeMap::new(), BTreeSet::new());
    if let Some(branch) = branch {
        // The branch the commit was mounted as stays, as it is in the repository.
        ws.write(&format!("{ROOT}/.git/HEAD"), format!("ref: refs/heads/{branch}\n"))?;
        ws.write(&format!("{ROOT}/.git/refs/heads/{branch}"), format!("{commit}\n"))?;
    }
    copy_changed(ws, &top, &changed, &mut times, &mut dirs)?;
    let snapshot = Snapshot { ws: ws.fork(), times, dirs };
    opened.saved.save(&snapshot, &VPath::parse(ROOT)?, true)?;
    home.keep_start(name)?;
    Ok(snapshot.ws)
}

/// The top of the git working folder that holds `folder`, if any.
pub fn working_folder(folder: &Path) -> Option<PathBuf> {
    let top = git(folder, &["rev-parse", "--show-toplevel"]).ok().filter(|top| !top.is_empty())?;
    Some(PathBuf::from(top))
}

/// Put git in workspace `name`, just forked from a git workspace, on a new branch
/// `branch` at the commit it is on, as `git switch -c` would: files and the index stay.
pub fn switch_new(home: &Home, name: &str, branch: &str) -> Result<()> {
    let record = home.record(name)?;
    let Base::Git { repo, .. } = &record.base else {
        bail!("--branch works with a git: source, and {} is a folder", record.source);
    };
    check_new_branch(repo, branch)?;

    let _writer = home.lock(name, "sandfs fork, preparing the workspace")?;
    let mut opened = home.open(name)?;
    let ws = &mut opened.ws;
    let head = ws.read_text(&format!("{ROOT}/.git/HEAD")).context("reading the fork's .git/HEAD")?;
    let commit = match head.trim().strip_prefix("ref: ") {
        Some(full) => ref_in(ws, full)?.with_context(|| format!("{full} has no commit yet"))?,
        None => head.trim().to_owned(),
    };
    let full = format!("refs/heads/{branch}");
    if ref_in(ws, &full)?.is_some() {
        bail!("the workspace already has a branch {branch}; pick another name");
    }
    for path in &record.read_only {
        ws.set_read_only(&format!("{ROOT}/{path}"), false)?;
    }
    ws.write(&format!("{ROOT}/.git/HEAD"), format!("ref: {full}\n"))?;
    ws.write(&format!("{ROOT}/.git/{full}"), format!("{commit}\n"))?;
    // Saved again in full, so the times the fork's files were saved with go along.
    let Restored { times, dirs } = std::mem::take(&mut opened.restored);
    let snapshot = Snapshot { ws: ws.fork(), times, dirs };
    opened.saved.save(&snapshot, &VPath::parse(ROOT)?, true)
}

/// The commit ref `full` (such as `refs/heads/main`) names in the `.git` of `ws`: its own
/// file, else its line in `packed-refs`.
fn ref_in(ws: &Workspace, full: &str) -> Result<Option<String>> {
    if let Ok(commit) = ws.read_text(&format!("{ROOT}/.git/{full}")) {
        return Ok(Some(commit.trim().to_owned()));
    }
    let Ok(packed) = ws.read_text(&format!("{ROOT}/.git/packed-refs")) else { return Ok(None) };
    Ok(packed.lines().find_map(|line| {
        let (commit, name) = line.split_once(' ')?;
        (name == full && !commit.starts_with('#')).then(|| commit.to_owned())
    }))
}

/// The files that differ between commits `from` and `to` of `repo`: changed, added or
/// deleted, as paths in the commits. Only folder listings are compared, so nothing is
/// fetched for a partial clone.
pub fn changed_between(repo: &Path, from: &str, to: &str) -> Result<BTreeSet<String>> {
    let out =
        git_raw(repo, &["diff-tree", "-r", "-z", "--name-only", "--no-renames", "--end-of-options", from, to], None)?;
    out.split(|&byte| byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| String::from_utf8(path.to_vec()).context("a changed file's name is not UTF-8"))
        .collect()
}

/// What `rel` is in `commit` of `repo`: its mode and object id as git lists them,
/// or none when the commit has nothing there. Only folder listings are read.
pub fn entry_at(repo: &Path, commit: &str, rel: &str) -> Result<Option<(String, String)>> {
    let out = git_raw(repo, &["ls-tree", "-z", "--full-tree", "--end-of-options", commit, "--", rel], None)?;
    // `<mode> <type> <id>\t<path>\0`, for the path itself only.
    let line = out.split(|&byte| byte == 0).next().unwrap_or_default();
    let line = String::from_utf8_lossy(line);
    let Some((meta, path)) = line.split_once('\t') else { return Ok(None) };
    let mut fields = meta.split(' ');
    match (fields.next(), fields.nth(1)) {
        (Some(mode), Some(id)) if path == rel => Ok(Some((mode.to_owned(), id.to_owned()))),
        _ => Ok(None),
    }
}

/// The id git gives `data` as the contents of a file of `repo`.
pub fn blob_id(repo: &Path, data: &[u8]) -> Result<String> {
    let out = git_raw(repo, &["hash-object", "--stdin"], Some(data))?;
    Ok(String::from_utf8_lossy(&out).trim().to_owned())
}

/// The branches in a `packed-refs` file: each one's commit and name.
fn packed_heads(text: &str) -> impl Iterator<Item = (&str, &str)> {
    text.lines().filter_map(|line| {
        let (id, name) = line.split_once(' ')?;
        let name = name.strip_prefix("refs/heads/")?;
        (!id.starts_with(['#', '^'])).then_some((id, name))
    })
}

/// Whether commit `id` comes before `to` in `repo`. A repository's local copy of a URL
/// holds only the commits mounted, so when it can't tell and `may_fetch` says the
/// copy is sandfs's own, it fetches more of the history before `to`: commits only, a
/// few times deeper, before saying no. Someone's own repository is only read.
fn comes_before(repo: &Path, id: &str, to: &str, may_fetch: bool) -> Result<bool> {
    let before = || git(repo, &["merge-base", "--is-ancestor", "--end-of-options", id, to]).is_ok();
    if !is_commit_id(id) {
        return Ok(false);
    }
    if before() {
        return Ok(true);
    }
    if !may_fetch || git(repo, &["rev-parse", "--is-shallow-repository"]).ok().as_deref() != Some("true") {
        return Ok(false);
    }
    static FETCHES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    for depth in [100, 1000, 10000] {
        let n = FETCHES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let fetched = format!("refs/sandfs/deepen-{}-{n}", std::process::id());
        let depth = format!("--depth={depth}");
        let refspec = format!("+{to}:{fetched}");
        let args =
            ["fetch", "-q", &depth, "--filter=tree:0", "--no-tags", "--recurse-submodules=no", "origin", &refspec];
        let mut waited = 0;
        let fetch = loop {
            match git(repo, &args) {
                Err(err) if waited < 300 && is_shallow_race(&format!("{err:#}")) => {
                    waited += 1;
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                result => break result,
            }
        };
        let _ = git(repo, &["update-ref", "-d", &fetched]);
        fetch.with_context(|| format!("fetching the history before {to} to see where {id} is in it"))?;
        if before() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The revision a local repository source such as `git:../repo@main` names (none for
/// its `HEAD`), or none at all for a URL.
pub fn local_rev(source: &str) -> Option<Option<String>> {
    let spec = source.strip_prefix("git:").unwrap_or(source);
    let (location, rev) = split_rev(spec);
    (!is_url(location)).then_some(rev)
}

/// A git workspace moving from commit `from` to `to` of `repo`. `branch` is the
/// branch the new commit is mounted as, which the mount's `.git` serves. `may_fetch`:
/// `repo` is sandfs's own copy of a URL, which may fetch history it lacks.
pub struct Move<'a> {
    pub repo: &'a Path,
    pub from: &'a str,
    pub to: &'a str,
    pub branch: Option<&'a str>,
    pub may_fetch: bool,
}

/// What happens to a file of the workspace's own `.git` (see [`Move::file`]).
#[derive(Debug, PartialEq, Eq)]
pub enum Moved {
    Keep,
    /// The mount's `.git` serves it for the new commit, so the saved one goes.
    Forget,
    /// Write these contents instead: a branch that was at `from` follows to `to`.
    Write(String),
}

/// Where a commit a branch or `HEAD` names stands, seen from the move.
#[derive(Debug, PartialEq, Eq)]
enum Place {
    /// The old commit, the new one, or one between them, as after `git pull`.
    Follows,
    /// One before the old commit, as for a branch left behind long ago.
    Older,
    /// One the new commit doesn't come after: commits made in the workspace.
    Own,
}

impl Move<'_> {
    fn place(&self, id: &str) -> Result<Place> {
        if id == self.from || id == self.to {
            return Ok(Place::Follows);
        }
        if !comes_before(self.repo, id, self.to, self.may_fetch)? {
            return Ok(Place::Own);
        }
        let older = git(self.repo, &["merge-base", "--is-ancestor", "--end-of-options", id, self.from]).is_ok();
        Ok(if older { Place::Older } else { Place::Follows })
    }

    /// The commit between the old and the new one that `HEAD` in `ws` is at, as after
    /// `git pull`: the files the pull checked out came from upstream, not the workspace.
    /// Another branch at such a commit says nothing about the files in the workspace.
    pub fn pulled(&self, ws: &Workspace) -> Result<Option<String>> {
        let Ok(head) = ws.read_text(&format!("{ROOT}/.git/HEAD")) else { return Ok(None) };
        let head = head.trim();
        let id = match head.strip_prefix("ref: ") {
            Some(full) => match ref_in(ws, full)? {
                Some(id) => id,
                None => return Ok(None),
            },
            None => head.to_owned(),
        };
        if id == self.from || id == self.to || self.place(&id)? != Place::Follows {
            return Ok(None);
        }
        Ok(Some(id))
    }

    /// What becomes of the file `rel` of the workspace's `.git`, with `data` its
    /// contents. The index goes, to be built again for the new commit. A `HEAD` or a
    /// branch that follows, follows to the new commit; one before the old commit stays
    /// where it is; one at commits made in the workspace, which moving would leave
    /// behind, is refused. `loose` says whether a branch has a file of its own, which
    /// git reads instead of its line in `packed-refs`.
    pub fn file(&self, rel: &str, data: &[u8], loose: &dyn Fn(&str) -> bool) -> Result<Moved> {
        let text = String::from_utf8_lossy(data).trim().to_owned();
        let short = |id: &str| id[..id.len().min(12)].to_owned();
        let own_branch = |name: &str, id: &str| {
            anyhow!(
                "branch {name} in the workspace has commits of its own (at {}), which moving would leave \
                 behind; push them first, or move a fresh workspace",
                short(id)
            )
        };
        if rel == ".git/index" {
            return Ok(Moved::Forget);
        }
        if rel == ".git/HEAD" {
            if text.starts_with("ref: ") {
                return Ok(Moved::Keep);
            }
            return match self.place(&text)? {
                Place::Follows => Ok(Moved::Forget),
                Place::Older => Ok(Moved::Keep),
                Place::Own => bail!(
                    "git in the workspace is at a commit of its own ({}), which moving would leave behind; \
                     push it to a branch first",
                    short(&text)
                ),
            };
        }
        if rel == ".git/packed-refs" {
            // Branches packed by `git pack-refs` or `git gc`, held to the same rules,
            // except one a file of its own has replaced since: git no longer reads it.
            let text = String::from_utf8_lossy(data);
            let (mut moved, mut lines) = (false, Vec::new());
            for line in text.lines() {
                match packed_heads(line).next().filter(|(_, name)| !loose(name)) {
                    Some((id, name)) => match self.place(id)? {
                        Place::Own => return Err(own_branch(name, id)),
                        Place::Follows if id != self.to => {
                            moved = true;
                            lines.push(format!("{} refs/heads/{name}", self.to));
                        }
                        _ => lines.push(line.to_owned()),
                    },
                    None => lines.push(line.to_owned()),
                }
            }
            return Ok(if moved {
                Moved::Write(lines.iter().map(|line| format!("{line}\n")).collect())
            } else {
                Moved::Keep
            });
        }
        let Some(name) = rel.strip_prefix(".git/refs/heads/") else { return Ok(Moved::Keep) };
        match self.place(&text)? {
            Place::Own => Err(own_branch(name, &text)),
            Place::Older => Ok(Moved::Keep),
            Place::Follows => Ok(match self.branch {
                Some(branch) if branch == name => Moved::Forget,
                _ if text == self.to => Moved::Keep,
                _ => Moved::Write(format!("{}\n", self.to)),
            }),
        }
    }
}

/// The branch a `HEAD` file `data` points to, if it points to one.
pub fn head_branch(data: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(data);
    text.trim().strip_prefix("ref: refs/heads/").map(str::to_owned)
}

/// The names of the branches a `packed-refs` file `data` holds.
pub fn packed_branches(data: &[u8]) -> Vec<String> {
    packed_heads(&String::from_utf8_lossy(data)).map(|(_, name)| name.to_owned()).collect()
}

/// The files in the working folder `top` that differ from `commit`, its `HEAD` when it
/// was read: changed, added or deleted, staged or not, and new files git doesn't
/// ignore. Should `HEAD` have moved since, these are still what makes `commit` the
/// folder. Deleted files come first, so a folder that replaced a file is made after
/// the file is gone.
fn changed_files(top: &Path, commit: &str) -> Result<Vec<String>> {
    let split = |out: Vec<u8>| -> Result<Vec<String>> {
        out.split(|&byte| byte == 0)
            .filter(|path| !path.is_empty())
            .map(|path| String::from_utf8(path.to_vec()).context("a changed file's name is not UTF-8"))
            .collect()
    };
    // Optional locks would let git refresh the repository's index: it is only read.
    // The two run at once, as each git run costs its start-up.
    let (tracked, untracked) = std::thread::scope(|scope| {
        let untracked = scope
            .spawn(|| git_raw(top, &["--no-optional-locks", "ls-files", "-z", "--others", "--exclude-standard"], None));
        let tracked = git_raw(
            top,
            &[
                "--no-optional-locks",
                "diff",
                "--name-only",
                "-z",
                "--no-renames",
                "--ignore-submodules=all",
                commit,
                "--",
            ],
            None,
        );
        let untracked = untracked.join().map_err(|_| anyhow!("listing new files failed"))?;
        anyhow::Ok((split(tracked?)?, split(untracked?)?))
    })?;
    let (mut gone, mut present) = (Vec::new(), Vec::new());
    // A repository inside the folder is listed as its folder, which a mount doesn't show.
    for path in tracked.into_iter().chain(untracked.into_iter().filter(|path| !path.ends_with('/'))) {
        match in_folder(top, &path)? {
            Some(_) => present.push(path),
            None => gone.push(path),
        }
    }
    gone.sort();
    present.sort();
    present.dedup();
    gone.append(&mut present);
    Ok(gone)
}

/// What the file `rel` (`/`-separated) is in the folder `top`, without following links:
/// none when it is gone, or when one of the folders on the way to it is gone or was
/// replaced by a file or a link, which git also counts as the file's deletion.
pub(crate) fn in_folder(top: &Path, rel: &str) -> Result<Option<std::fs::Metadata>> {
    let mut at = top.to_path_buf();
    let mut parts = rel.split('/').peekable();
    while let Some(part) = parts.next() {
        at.push(part);
        let meta = match std::fs::symlink_metadata(&at) {
            Ok(meta) => meta,
            Err(err) if matches!(err.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) => {
                return Ok(None);
            }
            Err(err) => return Err(err).with_context(|| format!("reading {}", at.display())),
        };
        if parts.peek().is_none() {
            return Ok(Some(meta));
        }
        if !meta.is_dir() {
            return Ok(None);
        }
    }
    Ok(None)
}

/// Make each of `changed` in `ws` what it is in the folder `top`, recording the times
/// and folders a save needs.
pub(crate) fn copy_changed(
    ws: &mut Workspace,
    top: &Path,
    changed: &[String],
    times: &mut BTreeMap<String, std::time::SystemTime>,
    dirs: &mut BTreeSet<String>,
) -> Result<()> {
    let root = VPath::parse(ROOT)?;
    for rel in changed {
        let path = VPath::parse(&format!("{ROOT}/{rel}")).with_context(|| format!("copying {rel}"))?;
        let at = path.as_str();
        let host = top.join(rel);
        let meta = match in_folder(top, rel)? {
            // A folder that replaced a file, which the files listed inside it go into.
            Some(meta) if meta.is_dir() => {
                if ws.stat(at).is_ok_and(|existing| !existing.kind.is_dir()) {
                    ws.remove(at, false)?;
                }
                continue;
            }
            Some(meta) => meta,
            None => {
                if ws.exists(at) {
                    ws.remove(at, true)?;
                }
                // Git keeps no folders, so one left empty in the repository's folder is gone there.
                let mut parent = path.parent();
                while let Some(dir) = parent.filter(|dir| *dir != root) {
                    let rel = dir.strip_prefix(&root).unwrap_or_default().join("/");
                    if top.join(&rel).exists() || !ws.list(dir.as_str()).is_ok_and(|entries| entries.is_empty()) {
                        break;
                    }
                    ws.remove(dir.as_str(), false)?;
                    dirs.insert(dir.to_string());
                    parent = dir.parent();
                }
                continue;
            }
        };
        if !meta.is_file() && !meta.file_type().is_symlink() {
            // Reading a pipe would wait for a writer, and a device has no contents to copy.
            eprintln!("sandfs: left out {rel}, which is not a file, folder or link");
            continue;
        }
        // Made afresh, so nothing of what was there (a folder, a link, a mode) stays.
        if ws.exists(at) {
            ws.remove(at, true)?;
        }
        if meta.file_type().is_symlink() {
            let target = std::fs::read_link(&host)?;
            let target = target.to_str().with_context(|| format!("{} points to a non-UTF-8 path", host.display()))?;
            ws.symlink(at, target)?;
        } else {
            ws.write(at, std::fs::read(&host).with_context(|| format!("reading {}", host.display()))?)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if meta.permissions().mode() & 0o111 != 0 {
                    ws.set_executable(at, true)?;
                }
            }
        }
        if let Ok(modified) = meta.modified() {
            times.insert(at.to_owned(), modified);
        }
        for dir in path.ancestors().into_iter().filter(|dir| dir.starts_with(&root) && *dir != root) {
            dirs.insert(dir.to_string());
        }
    }
    Ok(())
}

/// Give the fork kept as the real folder `dir` (see `folder`) its `.git`, from the
/// workspace `ws` over `commit` of `repo`, whose files `dir` already holds: the files
/// of the `.git` a mount shows, the same commit, branches and origin, but settings
/// without sandfs's file monitor, and an index of the commit's files that knows them
/// as they are in `dir`, so git reads none of them.
///
/// The index starts from `index_from`, the index of the folder the files were cloned
/// from, keeping what it knows of each file that is the same in the commit. Git then
/// only reads the files that are new to it or changed since.
///
/// Also makes `private`, a repository of sandfs's own over the same files, which
/// [`folder_changes`] asks git in. Git runs programs the settings of a repository name
/// (a file monitor, filters), and whatever works in `dir` can change those of its
/// `.git`, but not these.
pub fn make_folder(
    ws: &Workspace,
    (repo, commit, branch): (&Path, &str, Option<&str>),
    dir: &Path,
    private: &Path,
    index_from: Option<&Path>,
) -> Result<()> {
    let settings = Settings::opened_or_read(repo)?;
    let git_root = VPath::parse(&format!("{ROOT}/.git"))?;
    let mut folders = vec![git_root.clone()];
    while let Some(at) = folders.pop() {
        let rel = at.strip_prefix(&git_root).unwrap_or_default().join("/");
        std::fs::create_dir_all(dir.join(".git").join(&rel))?;
        for entry in ws.list(at.as_str())? {
            let path = at.join(&entry.name)?;
            let rel = path.strip_prefix(&git_root).unwrap_or_default().join("/");
            match entry.kind {
                Kind::Dir if rel != OBJECTS => folders.push(path),
                Kind::File { .. } if rel != INDEX && rel != "config" => {
                    std::fs::write(dir.join(".git").join(&rel), ws.read(path.as_str())?)?;
                }
                _ => {}
            }
        }
    }
    let config = settings.config(branch, None);
    std::fs::write(dir.join(".git/config"), &config)?;

    std::fs::create_dir_all(private.join("objects/info"))?;
    std::fs::create_dir_all(private.join("refs"))?;
    let objects = settings.common.join("objects");
    let objects = objects.to_str().context("non-UTF-8 repository path")?;
    std::fs::write(private.join("objects/info/alternates"), format!("{objects}\n"))?;
    std::fs::write(private.join("HEAD"), format!("{commit}\n"))?;
    let mut own =
        "[core]\n\trepositoryformatversion = 1\n\tbare = false\n\tcheckStat = minimal\n\ttrustctime = false\n"
            .to_owned();
    if settings.format != "sha1" {
        own.push_str(&format!("[extensions]\n\tobjectFormat = {}\n", config_value(&settings.format)));
    }
    std::fs::write(private.join("config"), own)?;
    std::fs::write(private.join(CHECKOUT_CONFIG), &config)?;

    let index = private.join(INDEX);
    let kept = index_from.is_some_and(|from| {
        std::fs::copy(from, &index).is_ok() && private_git(private, dir, &["read-tree", "-m", commit], None).is_ok()
    });
    if !kept {
        let _ = std::fs::remove_file(&index);
        private_git(private, dir, &["read-tree", commit], None)?;
    }
    std::fs::copy(&index, dir.join(".git").join(INDEX)).context("writing the folder's git index")?;
    Ok(())
}

/// The file in the private repository of a fork kept as a folder holding the settings
/// its `.git` was made with, to tell when they change (see [`make_folder`]).
pub const CHECKOUT_CONFIG: &str = "checkout-config";

/// The files in the folder `dir` of a fork kept as a folder that differ from the commit
/// it is over: changed, deleted or new, ignored files left out, as git finds them with
/// the repository [`make_folder`] made at `private`.
pub fn folder_changes(private: &Path, dir: &Path) -> Result<Vec<String>> {
    let args = ["status", "--porcelain", "-z", "--untracked-files=all", "--no-renames", "--ignore-submodules=all"];
    let out = private_git(private, dir, &args, None)?;
    // `XY path`, with nothing staged.
    out.split(|&byte| byte == 0)
        .filter_map(|record| record.get(3..))
        // A repository inside the folder is listed as its folder.
        .filter(|path| !path.is_empty() && !path.ends_with(b"/"))
        .map(|path| String::from_utf8(path.to_vec()).context("a changed file's name is not UTF-8"))
        .collect()
}

/// The index file of the git working folder `top`, for [`make_folder`].
pub fn index_of(top: &Path) -> Option<PathBuf> {
    // A clone's own `.git`, without asking git; a worktree's `.git` names its own.
    if top.join(".git").is_dir() && std::env::var_os("GIT_INDEX_FILE").is_none() {
        return Some(top.join(".git").join(INDEX));
    }
    git(top, &["rev-parse", "--git-path", INDEX]).ok().map(|path| top.join(path))
}

/// Run git in the private repository `private` of the fork kept as the folder `dir`.
fn private_git(private: &Path, dir: &Path, args: &[&str], input: Option<&[u8]>) -> Result<Vec<u8>> {
    let mut cmd = Command::new("git");
    cmd.arg("--git-dir").arg(private).arg("--work-tree").arg(dir);
    for name in
        ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_OBJECT_DIRECTORY", "GIT_ALTERNATE_OBJECT_DIRECTORIES"]
    {
        cmd.env_remove(name);
    }
    // Whatever settings the user's own may hold: no monitor, and an index git reads whole.
    cmd.args(["-c", "core.fsmonitor=false", "-c", "core.untrackedCache=false", "-c", "core.splitIndex=false"])
        .args(["-c", "index.sparse=false", "-c", "core.sparseCheckout=false", "-c", "maintenance.auto=false"])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .current_dir(dir);
    run_with_input(cmd, input).with_context(|| format!("git {}", args.join(" ")))
}

/// Refuse a name git wouldn't take for a new branch, or one the mount's `origin` already
/// has, which a push back would then overwrite or be refused. That is the repository
/// itself, or for a partial clone (sandfs's copy of a remote among them) the remote it
/// borrows from, as [`GitDir`] sets it.
fn check_new_branch(repo: &Path, branch: &str) -> Result<()> {
    if branch.starts_with('-') {
        bail!("{branch:?} is not a name git takes for a branch");
    }
    let full = format!("refs/heads/{branch}");
    // All three asked at once, as each git run costs its start-up; whether the
    // repository has the branch matters only when it isn't a partial clone.
    let (checked, promisor, taken) = std::thread::scope(|scope| {
        let checked = scope.spawn(|| git(repo, &["check-ref-format", "--branch", branch]).ok());
        let taken = scope.spawn(|| git(repo, &["show-ref", "--verify", "-q", &full]).is_ok());
        let promisor = git(repo, &["config", "--get", "remote.origin.promisor"]).is_ok_and(|value| value == "true");
        (checked.join().ok().flatten(), promisor, taken.join().unwrap_or(false))
    });
    if checked.as_deref() != Some(branch) {
        bail!("{branch:?} is not a name git takes for a branch");
    }
    if promisor {
        // A partial clone's own branches needn't be the remote's: sandfs's copies keep
        // what they fetched under refs of their own.
        let url = origin(repo).context("the partial clone names no remote")?;
        if url.starts_with('-') {
            bail!("the partial clone's remote {url:?} is not a URL");
        }
        // A path relative to the repository, as git reads one there.
        let remote = if is_url(&url) { url.clone() } else { repo.join(&url).to_string_lossy().into_owned() };
        // Asked from outside the repository, whose settings could make git run a program
        // to reach the remote (an `ext::` URL, a URL rewritten to one, an ssh command).
        if !git_outside(&["ls-remote", &remote, &full]).context("asking the remote for its branches")?.is_empty() {
            bail!("{url} already has a branch {branch}; pick another name");
        }
    } else if taken {
        bail!("{} already has a branch {branch}; pick another name", repo.display());
    }
    Ok(())
}

/// The `.git` folder of a git mount: a repository whose checkout is the mount itself.
///
/// It holds only what makes the mount a checkout of the commit: `HEAD` (on the branch
/// the commit was mounted as, else at the commit), the branch, a config with the
/// origin, and an index listing the commit's files. Objects come from the local copy
/// through `objects/info/alternates`, so git reads them there and writes new ones
/// beside this folder's own files, into the workspace. A remote's local copy is a
/// partial clone, and so is this repository: git fetches file contents it needs and
/// doesn't have, as the local copy does.
///
/// The index is built when first read, so a mount nobody runs git in never pays for
/// it. Its entries carry no file times or sizes, which git would otherwise compare
/// by reading every file. Instead every entry is marked unchanged for git's file
/// monitor, which asks `sandfs git-fsmonitor` what changed since; the same answers
/// keep git's cache of untracked files current, so it lists folders only once.
/// Submodules, which a mount doesn't show, are marked as outside the checkout, so git
/// doesn't take them for deleted.
///
/// Before every commit, git in a partial clone checks that it has the contents of every
/// file the index lists, and fetches whichever it lacks: on a fresh mount, all of them.
/// So the files the local copy lacks are also served here, as loose objects in
/// [`OBJECTS`] that git finds by listing those folders, without reading them. Git then
/// fetches nothing, and reads only what it needs: the other files of each folder a
/// commit changes, which it compares. Those are fetched into the local copy like any
/// file read, a whole folder at once from its second.
#[derive(Debug)]
pub struct GitDir {
    dirs: BTreeMap<String, Listing>,
    files: BTreeMap<String, Bytes>,
    /// The repository the index is built from.
    common: PathBuf,
    /// Whether that is sandfs's own copy of a remote, which the index build may fetch
    /// into. A repository of the user's own is never written to.
    managed: bool,
    commit: String,
    index: OnceLock<std::result::Result<Bytes, String>>,
    /// The files the local copy lacks, served as loose objects; only for a partial
    /// clone of sandfs's own.
    objects: Option<Objects>,
}

/// The folder of [`GitDir`] holding the files the local copy lacks, as loose objects in
/// folders named for the first two digits of their ids, as git keeps them.
const OBJECTS: &str = "sandfs-objects";

/// The files [`GitDir`] serves from [`OBJECTS`].
struct Objects {
    source: Arc<GitSource>,
    /// Which folder each missing file is in, found when git first reads one.
    folders: OnceLock<Folders>,
    /// How many files of each folder (an index into `Folders::trees`) git has read here.
    reads: Mutex<HashMap<u32, u32>>,
}

impl std::fmt::Debug for Objects {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Objects").field("source", &self.source).finish_non_exhaustive()
    }
}

/// The folders of the files a commit's local copy lacked.
#[derive(Default)]
struct Folders {
    /// The ids of the commit's folders.
    trees: Vec<String>,
    /// For each missing file, in the order of [`ObjectIds::index_of`], the index of its
    /// folder in `trees`, or [`NO_FOLDER`].
    of: Vec<u32>,
}

const NO_FOLDER: u32 = u32::MAX;

/// The file of [`GitDir`] built when first read.
const INDEX: &str = "index";

/// Where `sandfs git-fsmonitor` keeps every path it has reported (see [`fsmonitor`]).
const TOUCHED: &str = ".git/sandfs-touched";

/// What a checkout of a commit of a repository, a mount's or a folder's (see
/// [`make_folder`]), takes from the repository's settings.
#[derive(Clone)]
struct Settings {
    /// Where the repository keeps its objects and settings.
    common: PathBuf,
    /// `sha1` or `sha256`.
    format: String,
    /// The repository's origin, which pushes from the checkout go to.
    origin: String,
    /// For a partial clone, the filter it fetches with.
    promisor: Option<String>,
}

/// The settings each repository's `.git` was last made with in this process (see
/// [`GitDir::new`]), so a folder made from the same workspace right after needn't
/// start git twice to read them again.
static OPENED: Mutex<Option<HashMap<PathBuf, Settings>>> = Mutex::new(None);

impl Settings {
    /// The settings of `repo`, as this process last read them, else read now.
    fn opened_or_read(repo: &Path) -> Result<Self> {
        let opened = OPENED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        match opened.as_ref().and_then(|opened| opened.get(repo)) {
            Some(settings) => Ok(settings.clone()),
            None => Self::read(repo),
        }
    }

    fn read(repo: &Path) -> Result<Self> {
        let info = git(repo, &["rev-parse", "--git-common-dir", "--show-object-format"])?;
        let mut lines = info.lines();
        let common = repo.join(lines.next().context("git rev-parse printed nothing")?);
        let format = lines.next().unwrap_or("sha1").to_owned();
        // One git for every setting read here, since starting it costs the most.
        let settings =
            git(&common, &["config", "--get-regexp", r"^remote\.origin\.(promisor|url|partialclonefilter)$"])
                .unwrap_or_default();
        let get = |key: &str| {
            settings
                .lines()
                .rev()
                .find_map(|line| line.strip_prefix(&format!("remote.origin.{key} ")))
                .map(str::to_owned)
        };
        Self::new(repo, &common, format, get)
    }

    /// The settings `files`, the source of a commit of `repo`, read when it was opened.
    fn of_source(repo: &Path, files: &GitSource) -> Result<Self> {
        let get = |key: &str| files.remote_setting("origin", key).map(str::to_owned);
        Self::new(repo, files.common_dir(), files.object_format().to_owned(), get)
    }

    /// The settings of `repo`, whose common folder is `common`, from its object `format`
    /// and `origin`, its origin's setting of each key.
    fn new(repo: &Path, common: &Path, format: String, origin: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let common = common.canonicalize().with_context(|| format!("no repository at {}", common.display()))?;
        let common = without_verbatim_prefix(common);
        // A partial clone (every remote's local copy) borrows its origin; a local
        // repository is the origin itself.
        let promisor = origin("promisor")
            .filter(|value| value == "true")
            .map(|_| origin("partialclonefilter").unwrap_or_else(|| "blob:none".to_owned()));
        let origin = match origin("url") {
            Some(url) if promisor.is_some() => url,
            _ => repo.to_str().context("non-UTF-8 repository path")?.to_owned(),
        };
        Ok(Settings { common, format, origin, promisor })
    }

    /// The checkout's `.git/config`, on `branch` if any, with `monitor` as git's file
    /// monitor.
    fn config(&self, branch: Option<&str>, monitor: Option<&str>) -> String {
        let mut config = "[core]\n\trepositoryformatversion = 1\n\tfilemode = true\n\tbare = false\n\
                          \tlogallrefupdates = true\n"
            .to_owned();
        match monitor {
            // Newer git turns its file monitor off on network and FUSE file systems, a
            // mount among them, unless `fsmonitor.allowRemote` says otherwise. And it
            // starts a maintenance process after commits and fetches, to pack loose
            // objects, which a workspace's few aren't worth.
            Some(monitor) => config.push_str(&format!(
                "\tfsmonitor = {}\n\tfsmonitorHookVersion = 2\n\tuntrackedCache = true\n\
                 [fsmonitor]\n\tallowRemote = true\n\
                 [maintenance]\n\tauto = false\n",
                config_value(monitor)
            )),
            // The folder's files are clones, with the times and sizes of the files the
            // index was made for but not their inode numbers or change times, which git
            // would otherwise take for changes and read every file over.
            None => config.push_str("\tcheckStat = minimal\n\ttrustctime = false\n"),
        }
        config.push_str(&format!(
            "[remote \"origin\"]\n\turl = {}\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n",
            config_value(&self.origin)
        ));
        if let Some(filter) = &self.promisor {
            config.push_str(&format!("\tpromisor = true\n\tpartialclonefilter = {}\n", config_value(filter)));
        }
        if let Some(branch) = branch {
            config.push_str(&format!(
                "[branch {}]\n\tremote = origin\n\tmerge = refs/heads/{branch}\n",
                config_value(branch)
            ));
        }
        if self.promisor.is_some() || self.format != "sha1" {
            config.push_str("[extensions]\n");
            if self.promisor.is_some() {
                config.push_str("\tpartialClone = origin\n");
            }
            if self.format != "sha1" {
                config.push_str(&format!("\tobjectFormat = {}\n", config_value(&self.format)));
            }
        }
        config
    }
}

impl GitDir {
    /// The `.git` folder for `commit` of `repo`, for workspaces kept in the home at
    /// `home`. With `files`, the source of the commit's files, it serves the files the
    /// local copy lacks (see [`GitDir`]).
    pub fn new(
        repo: &Path,
        commit: &str,
        branch: Option<&str>,
        home: &Path,
        files: Option<Arc<GitSource>>,
    ) -> Result<Self> {
        let settings = match &files {
            Some(files) => Settings::of_source(repo, files)?,
            None => Settings::read(repo)?,
        };
        let mut opened = OPENED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        opened.get_or_insert_with(HashMap::new).insert(repo.to_path_buf(), settings.clone());
        drop(opened);
        let Settings { common, promisor, .. } = &settings;
        let common = common.clone();
        let promisor = promisor.is_some();
        let managed = repo.starts_with(home.join("repos"));
        let objects = files.filter(|_| promisor && managed).map(|source| Objects {
            source,
            folders: OnceLock::new(),
            reads: Mutex::default(),
        });

        // Git runs the monitor with the environment of whoever runs git, which may not
        // name the same home.
        let exe = crate::relaunch::program()?;
        let mut hook = format!(
            "SANDFS_HOME={} {}",
            shell_quote(home.to_str().context("non-UTF-8 home directory")?),
            shell_quote(exe.to_str().context("non-UTF-8 path to this program")?)
        );
        for word in crate::relaunch::prefix() {
            hook.push(' ');
            hook.push_str(&shell_quote(word.to_str().context("non-UTF-8 relaunch prefix")?));
        }
        let config = settings.config(branch, Some(&format!("{hook} git-fsmonitor")));

        let mut files: Vec<(String, Bytes)> = Vec::new();
        let mut dirs: Vec<String> =
            ["objects/info", "objects/pack", "refs/heads", "refs/tags"].map(String::from).into();
        match branch {
            Some(branch) => {
                files.push(("HEAD".into(), format!("ref: refs/heads/{branch}\n").into()));
                files.push((format!("refs/heads/{branch}"), format!("{commit}\n").into()));
                files.push((format!("refs/remotes/origin/{branch}"), format!("{commit}\n").into()));
            }
            None => files.push(("HEAD".into(), format!("{commit}\n").into())),
        }
        files.push(("config".into(), config.into()));
        let common_objects = common.join("objects");
        let common_objects = common_objects.to_str().context("non-UTF-8 repository path")?;
        // A relative alternate is found from the `objects` folder.
        let lazy = if objects.is_some() { format!("../{OBJECTS}\n") } else { String::new() };
        files.push(("objects/info/alternates".into(), format!("{common_objects}\n{lazy}").into()));
        // The commits the local copy has without their parents, this one among them.
        if let Ok(shallow) = std::fs::read(common.join("shallow")) {
            files.push(("shallow".into(), shallow.into()));
        }

        let mut dir = GitDir {
            dirs: BTreeMap::from([(String::new(), Listing::new())]),
            files: BTreeMap::new(),
            common,
            managed,
            commit: commit.to_owned(),
            index: OnceLock::new(),
            objects,
        };
        if dir.objects.is_some() {
            dirs.extend((0..=255).map(|first| format!("{OBJECTS}/{first:02x}")));
        }
        dirs.sort();
        for path in &dirs {
            dir.add_dir(path);
        }
        for (path, data) in files {
            let (parent, name) = path.rsplit_once('/').unwrap_or(("", &path));
            dir.add_dir(parent);
            let entry = SourceEntry {
                kind: Kind::File { executable: false },
                key: path.as_str().into(),
                size: Some(data.len() as u64),
                modified: None,
            };
            dir.dirs.get_mut(parent).expect("parent added").insert(name.to_owned(), entry);
            dir.files.insert(path, data);
        }
        // Its size is unknown until it is built.
        let entry =
            SourceEntry { kind: Kind::File { executable: false }, key: INDEX.into(), size: None, modified: None };
        dir.dirs.get_mut("").expect("root").insert(INDEX.to_owned(), entry);
        Ok(dir)
    }

    /// Add the directory `path` and its parents.
    fn add_dir(&mut self, path: &str) {
        let mut key = String::new();
        for part in path.split('/').filter(|part| !part.is_empty()) {
            let child = if key.is_empty() { part.to_owned() } else { format!("{key}/{part}") };
            let entry = SourceEntry { kind: Kind::Dir, key: child.as_str().into(), size: None, modified: None };
            self.dirs.get_mut(&key).expect("parent added").entry(part.to_owned()).or_insert(entry);
            self.dirs.entry(child.clone()).or_default();
            key = child;
        }
    }

    /// The index of the commit's files, every one marked unchanged (see [`GitDir`]).
    fn build_index(&self) -> Result<Bytes> {
        static BUILDS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = BUILDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = std::env::temp_dir().join(format!("sandfs-index-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        // Marking entries needs a checkout to run in, though it looks at no files.
        std::fs::create_dir_all(tmp.join("checkout")).context("creating a temporary folder")?;
        std::fs::create_dir_all(tmp.join("hooks")).context("creating a temporary folder")?;
        let built = (|| {
            let index = tmp.join(INDEX);
            let common = self.common.as_path();
            let run = |args: &[&str], input: Option<&[u8]>| -> Result<Vec<u8>> {
                let mut cmd = Command::new("git");
                cmd.arg("--git-dir")
                    .arg(common)
                    .arg("--work-tree")
                    .arg(tmp.join("checkout"))
                    // A failing monitor (`exit 1`): git asks none while marking entries.
                    .args(["-c", "core.fsmonitor=exit 1", "-c", "core.fsmonitorHookVersion=2"])
                    // The repository's own settings must not change the index or run its
                    // hooks: a split or sparse index would point at files the mount lacks.
                    .args(["-c", "core.splitIndex=false", "-c", "index.sparse=false"])
                    .args(["-c", "core.sparseCheckout=false"])
                    .arg("-c")
                    .arg(format!("core.hooksPath={}", tmp.join("hooks").display()))
                    .args(args)
                    .env("GIT_INDEX_FILE", &index)
                    .env("GIT_TERMINAL_PROMPT", "0");
                run_with_input(cmd, input)
            };
            run(&["read-tree", &self.commit], None)?;
            let listed = run(&["ls-files", "-s", "-z"], None)?;
            let (mut all, mut submodules, mut rules) = (Vec::new(), Vec::new(), Vec::new());
            // `<mode> <id> <stage>\t<path>`
            for record in listed.split(|&b| b == 0).filter(|record| !record.is_empty()) {
                let Some(tab) = record.iter().position(|&b| b == b'\t') else { continue };
                let path = &record[tab + 1..];
                if record.starts_with(b"160000 ") {
                    submodules.extend_from_slice(path);
                    submodules.push(0);
                }
                let name = path.rsplit(|&b| b == b'/').next().unwrap_or(path);
                if name == b".gitignore" || name == b".gitattributes" {
                    rules.extend(record.get(7..tab).and_then(|rest| rest.split(|&b| b == b' ').next()));
                }
                all.extend_from_slice(path);
                all.push(0);
            }
            self.fetch_missing(&rules);
            // Outside the checkout first: a path marked both ways would get only that.
            if !submodules.is_empty() {
                run(&["update-index", "-z", "--skip-worktree", "--stdin"], Some(&submodules))?;
            }
            run(&["update-index", "-z", "--fsmonitor", "--fsmonitor-valid", "--stdin"], Some(&all))?;
            // Marking entries also forgot the folder ids `read-tree` put in the index, so
            // a commit would compare every file, fetching each the local copy lacks.
            // Writing the commit's folders again finds them again without reading a file;
            // the monitor's empty answer keeps every entry marked. It rewrites folders
            // the local copy already has, so a repository of the user's own, which holds
            // every file, is left alone.
            if self.managed {
                let _ = run(&["-c", "core.fsmonitor=printf 'sandfs\\0'", "write-tree", "--missing-ok"], None);
            }
            Ok(Bytes::from(std::fs::read(&index).context("reading the built index")?))
        })();
        let _ = std::fs::remove_dir_all(&tmp);
        built
    }
}

impl GitDir {
    /// Fetch whichever of these blobs a partial clone lacks, in one request. Git reads
    /// every `.gitignore` and `.gitattributes` it passes, and would otherwise fetch
    /// each in a request of its own. Best effort: git fetches what this misses.
    fn fetch_missing(&self, blobs: &[&[u8]]) {
        if !self.managed
            || blobs.is_empty()
            || git(&self.common, &["config", "--get", "remote.origin.promisor"]).is_err()
        {
            return;
        }
        // Trees are local, so finding the missing blobs fetches nothing.
        let Ok(objects) = git(&self.common, &["rev-list", "--objects", "--missing=print", &self.commit]) else {
            return;
        };
        let missing: BTreeSet<&[u8]> =
            objects.lines().filter_map(|line| line.strip_prefix('?')).map(str::as_bytes).collect();
        let wanted: Vec<u8> = blobs
            .iter()
            .filter(|blob| missing.contains(*blob))
            .flat_map(|blob| blob.iter().copied().chain(*b"\n"))
            .collect();
        if wanted.is_empty() {
            return;
        }
        let mut cmd = Command::new("git");
        cmd.arg("-C")
            .arg(&self.common)
            .args(["-c", "fetch.negotiationAlgorithm=noop", "fetch", "origin", "--no-tags", "--no-write-fetch-head"])
            .args(["--recurse-submodules=no", "--filter=blob:none", "--stdin"])
            .env("GIT_TERMINAL_PROMPT", "0");
        if run_with_input(cmd, Some(&wanted)).is_ok()
            && let Some(objects) = &self.objects
        {
            // Reading them later is then no reason for a background fetch to go on.
            let ids = wanted.split(|&b| b == b'\n').filter_map(|id| std::str::from_utf8(id).ok());
            objects.source.fetched_elsewhere(ids.filter(|id| !id.is_empty()));
        }
    }
}

impl GitDir {
    /// The loose objects of [`OBJECTS`] whose ids start with the hex digits `first`.
    fn list_objects(&self, objects: &Objects, first: &str) -> Option<Listing> {
        let first = u8::from_str_radix(first, 16).ok().filter(|_| first.len() == 2)?;
        let mut listing = Listing::new();
        for id in objects.source.missing_files().into_iter().flat_map(|missing| missing.starting_with(first)) {
            let key = format!("{OBJECTS}/{}/{}", &id[..2], &id[2..]);
            // Known from the file's size alone, so stat-ing it, as `find` does, fetches nothing.
            let size = objects.source.known_size(&id).map(|size| loose_object_size("blob", size));
            listing.insert(
                id[2..].to_owned(),
                SourceEntry { kind: Kind::File { executable: false }, key: key.into(), size, modified: None },
            );
        }
        Some(listing)
    }

    /// The loose object of the missing file `id`.
    fn read_object(&self, objects: &Objects, id: &str) -> sandfs_core::Result<Bytes> {
        // Git reads a folder's files one after another when it compares them, so from the
        // second, fetch the rest of the folder with it.
        if let Some(missing) = objects.source.missing_files()
            && let Some(index) = missing.index_of(id)
        {
            let folders = objects.folders.get_or_init(|| self.folders(objects.source.tree_id(), missing));
            let tree = folders.of.get(index).copied().unwrap_or(NO_FOLDER);
            let second = tree != NO_FOLDER
                && objects.reads.lock().is_ok_and(|mut reads| {
                    let count = reads.entry(tree).or_insert(0);
                    *count += 1;
                    *count == 2
                });
            if second {
                objects.source.prefetch(&folders.trees[tree as usize], false);
            }
        }
        Ok(loose_object("blob", &objects.source.read_file(id)?))
    }

    /// The folder of each of the `missing` files of the commit, whose tree is `root`.
    fn folders(&self, root: &str, missing: &ObjectIds) -> Folders {
        let mut folders = Folders { trees: vec![root.to_owned()], of: vec![NO_FOLDER; missing.len()] };
        // Trees are local, so listing them fetches nothing; `-t` lists each folder before
        // what is in it.
        let child = Command::new("git")
            .arg("-C")
            .arg(&self.common)
            .args(["ls-tree", "-r", "-t", "-z", "--full-tree", &self.commit])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let Ok(mut child) = child else { return folders };
        let Some(stdout) = child.stdout.take() else { return folders };
        let mut trees: HashMap<Vec<u8>, u32> = HashMap::from([(Vec::new(), 0)]);
        // `<mode> <type> <id>\t<path>`
        for record in BufReader::new(stdout).split(0).map_while(std::result::Result::ok) {
            let Some(tab) = record.iter().position(|&b| b == b'\t') else { continue };
            let (info, path) = (&record[..tab], &record[tab + 1..]);
            let mut fields = info.split(|&b| b == b' ').skip(1);
            let (Some(kind), Some(id)) = (fields.next(), fields.next().and_then(|id| std::str::from_utf8(id).ok()))
            else {
                continue;
            };
            if kind == b"tree" {
                trees.insert(path.to_vec(), folders.trees.len() as u32);
                folders.trees.push(id.to_owned());
            } else if let Some(index) = missing.index_of(id) {
                let parent = path.iter().rposition(|&b| b == b'/').map_or(&path[..0], |slash| &path[..slash]);
                folders.of[index] = trees.get(parent).copied().unwrap_or(NO_FOLDER);
            }
        }
        let _ = child.wait();
        folders
    }
}

/// `data` as a loose object of `kind`, as git keeps one in a file: `<kind> <length>\0`
/// and the data, compressed with zlib. This stores them uncompressed, in zlib's stored
/// blocks, which cost nothing to make; git reads them as it reads any.
fn loose_object(kind: &str, data: &[u8]) -> Bytes {
    let mut content = format!("{kind} {}\0", data.len()).into_bytes();
    content.extend_from_slice(data);
    let mut out = Vec::with_capacity(content.len() + content.len() / 65535 * 5 + 11);
    out.extend_from_slice(&[0x78, 0x01]);
    let mut blocks = content.chunks(65535).peekable();
    while let Some(block) = blocks.next() {
        let len = block.len() as u16;
        out.push(u8::from(blocks.peek().is_none()));
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(block);
    }
    // Adler-32, summed in runs short enough that the sums can't overflow before `%`.
    let (mut a, mut b) = (1u32, 0u32);
    for run in content.chunks(5552) {
        for &byte in run {
            a += u32::from(byte);
            b += a;
        }
        (a, b) = (a % 65521, b % 65521);
    }
    out.extend_from_slice(&((b << 16) | a).to_be_bytes());
    out.into()
}

/// The length of [`loose_object`] for `len` bytes of `kind`.
fn loose_object_size(kind: &str, len: u64) -> u64 {
    let content = format!("{kind} {len}\0").len() as u64 + len;
    // zlib's header, each stored block's own, and the checksum.
    2 + content.div_ceil(65535) * 5 + content + 4
}

impl Source for GitDir {
    fn root(&self) -> sandfs_core::Result<SourceKey> {
        Ok("".into())
    }

    fn read_dir(&self, key: &str) -> sandfs_core::Result<Listing> {
        if let Some(objects) = &self.objects
            && let Some(first) = key.strip_prefix(OBJECTS).and_then(|k| k.strip_prefix('/'))
            && let Some(listing) = self.list_objects(objects, first)
        {
            return Ok(listing);
        }
        self.dirs.get(key).cloned().ok_or_else(|| sandfs_core::Error::Source(format!("no directory {key:?}")))
    }

    fn read_file(&self, key: &str) -> sandfs_core::Result<Bytes> {
        if let Some(objects) = &self.objects
            && let Some((first, rest)) =
                key.strip_prefix(OBJECTS).and_then(|k| k.strip_prefix('/')).and_then(|k| k.split_once('/'))
        {
            return self.read_object(objects, &format!("{first}{rest}"));
        }
        if key == INDEX {
            let built = self.index.get_or_init(|| self.build_index().map_err(|err| format!("{err:#}")));
            return built.clone().map_err(|err| sandfs_core::Error::Source(format!("building the git index: {err}")));
        }
        self.files.get(key).cloned().ok_or_else(|| sandfs_core::Error::Source(format!("no file {key:?}")))
    }
}

/// The object folders git reads a checkout of `repo` from: the local copy's own, and
/// those it borrows from in turn through its `objects/info/alternates`.
pub fn object_folders(repo: &Path) -> Result<Vec<PathBuf>> {
    let common = repo.join(git(repo, &["rev-parse", "--git-common-dir"])?);
    let mut found: Vec<PathBuf> = Vec::new();
    let mut next = vec![common.join("objects")];
    // Git follows at most five levels of alternates.
    for _ in 0..6 {
        let mut borrowed = Vec::new();
        for objects in next {
            let Ok(objects) = objects.canonicalize().map(without_verbatim_prefix) else { continue };
            if found.contains(&objects) {
                continue;
            }
            let alternates = std::fs::read_to_string(objects.join("info/alternates")).unwrap_or_default();
            let lines = alternates.lines().map(str::trim).filter(|line| !line.is_empty() && !line.starts_with('#'));
            borrowed.extend(lines.map(|line| objects.join(line)));
            found.push(objects);
        }
        next = borrowed;
    }
    Ok(found)
}

/// Answer git's file monitor (hook version 2) for the mounted workspace whose saved
/// changes are `saved`, from the mount's root.
///
/// Git asks which files changed since a token it got from an earlier answer, and
/// checks only those against its index. Every path the workspace ever changed is
/// reported, not only those that differ now: a file changed and then put back is no
/// longer a change, but git may have recorded it while it was different. The paths
/// reported before are kept in [`TOUCHED`], inside the mount.
pub fn fsmonitor(saved: &[(String, PathState)], version: &str) -> Result<()> {
    check_fsmonitor_version(version)?;
    let previous = std::fs::read(TOUCHED).unwrap_or_default();
    let mut touched: BTreeSet<&[u8]> = previous.split(|&b| b == 0).filter(|path| !path.is_empty()).collect();
    let before = touched.len();
    let mut current = Vec::new();
    for (path, state) in saved {
        if path == ".git" || path.starts_with(".git/") {
            continue;
        }
        // A trailing slash stands for everything below a directory. A directory may
        // have replaced a file of the same name, which git checks only by the bare path.
        match state {
            PathState::File => current.push(path.clone()),
            PathState::Dir | PathState::Absent => current.extend([path.clone(), format!("{path}/")]),
        }
    }
    touched.extend(current.iter().map(|path| path.as_bytes()));
    if touched.len() > before {
        let mut list = Vec::new();
        for path in &touched {
            list.extend_from_slice(path);
            list.push(0);
        }
        std::fs::write(TOUCHED, &list).with_context(|| format!("writing {TOUCHED}"))?;
    }
    fsmonitor_answer(touched)
}

/// Answer git's file monitor (hook version 2) that any file may have changed, as in a
/// folder that is no mount, so git checks every file itself.
pub fn fsmonitor_all(version: &str) -> Result<()> {
    check_fsmonitor_version(version)?;
    fsmonitor_answer([b"/".as_slice()])
}

fn check_fsmonitor_version(version: &str) -> Result<()> {
    if version != "2" {
        bail!("unsupported file monitor version {version}; set core.fsmonitorHookVersion = 2");
    }
    Ok(())
}

fn fsmonitor_answer<'a>(paths: impl IntoIterator<Item = &'a [u8]>) -> Result<()> {
    // Any new token will do: the answer never depends on it.
    let token = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
    let mut out = std::io::stdout().lock();
    write!(out, "sandfs:{token}\0")?;
    for path in paths {
        out.write_all(path)?;
        out.write_all(b"\0")?;
    }
    out.flush()?;
    Ok(())
}

/// `value` as a quoted git config value.
fn config_value(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n"))
}

/// `path` without the `\\?\` that canonical Windows paths start with, which git can't
/// open as a repository.
fn without_verbatim_prefix(path: PathBuf) -> PathBuf {
    match path.to_str() {
        Some(s) if s.starts_with(r"\\?\UNC\") => PathBuf::from(format!(r"\\{}", &s[8..])),
        Some(s) if s.starts_with(r"\\?\") => PathBuf::from(&s[4..]),
        _ => path,
    }
}

/// `word` quoted for `sh`, which runs git's file monitor.
fn shell_quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// Run `cmd`, feeding it `input`, and return what it printed.
fn run_with_input(mut cmd: Command, input: Option<&[u8]>) -> Result<Vec<u8>> {
    cmd.stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().context("running git")?;
    if let Some(input) = input {
        let mut stdin = child.stdin.take().context("git's input")?;
        // Written from a thread, so a full output pipe can't block the write.
        let input = input.to_vec();
        std::thread::spawn(move || stdin.write_all(&input));
    }
    let output = child.wait_with_output().context("running git")?;
    if !output.status.success() {
        bail!("git: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(output.stdout)
}

/// Fetch `rev` of `url` into its local partial clone, creating the clone on first use,
/// and return the clone and the commit id.
///
/// Only the commit itself and its folder listings are fetched (depth 1, no file
/// contents), which takes about a second even for a repository with years of history.
/// A full commit id that was mounted before is not looked up again, so it mounts
/// offline; a branch or tag name is looked up on the server every time.
fn fetch_commit(repos: &Path, url: &str, rev: &str) -> Result<(PathBuf, String)> {
    let dest = repos.join(cache_path(url)?);
    let existed = dest.join("HEAD").exists();
    if !existed {
        eprintln!("sandfs: fetching {url} (file contents are fetched when first read)");
        create_clone(&dest, url)?;
    } else if let Some(commit) = pinned(&dest, rev) {
        // Nothing to fetch, but later file contents come from `origin`: switch it to a
        // different URL given now only if that URL answers, so working offline still works.
        if origin(&dest).as_deref() != Some(url) && git(&dest, &["ls-remote", url, "HEAD"]).is_ok() {
            git(&dest, &["remote", "set-url", "origin", url])?;
        }
        return Ok((dest, commit));
    }
    // A ref of this fetch's own, so concurrent mounts of one repository don't race on
    // FETCH_HEAD or on each other's results.
    static FETCHES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = FETCHES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let fetched = format!("refs/sandfs/fetch-{}-{n}", std::process::id());
    let refspec = format!("+{rev}:{fetched}");
    let args =
        ["fetch", "-q", "--depth=1", "--filter=blob:none", "--no-tags", "--recurse-submodules=no", url, &refspec];
    // A shallow fetch rewrites the local copy's list of shallow commits, and git refuses
    // to while another fetch is doing so ("shallow.lock: File exists", "shallow file has
    // changed since we read it"). Of two mounts fetching at once, one waits and retries.
    let mut waited = 0;
    let mut fetch = || loop {
        match git(&dest, &args) {
            Err(err) if waited < 300 && is_shallow_race(&format!("{err:#}")) => {
                waited += 1;
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            result => break result,
        }
    };
    // The host's list of file sizes (see `github`) takes a request of its own, so it
    // runs while the commit is fetched.
    let (fetch, sized) = std::thread::scope(|scope| {
        let sizes = github::plain_rev(rev).then(|| scope.spawn(|| github::fetch_sizes(url, rev)));
        (fetch(), sizes.and_then(|sizes| sizes.join().ok().flatten()))
    });
    let commit = match fetch {
        Ok(_) => {
            // One local copy serves every URL that names the repository (https or ssh,
            // with or without a user), and file contents come from `origin`. Fetching from
            // `url` itself and only then making it `origin` replaces an earlier URL that
            // didn't work, but never one that did with one that doesn't.
            if origin(&dest).as_deref() != Some(url) {
                git(&dest, &["remote", "set-url", "origin", url])?;
            }
            let commit = commit_of(&dest, &fetched);
            let _ = git(&dest, &["update-ref", "-d", &fetched]);
            let commit = commit?;
            if let Ok(tree) = git(&dest, &["rev-parse", &format!("{commit}^{{tree}}")])
                && !github::has_sizes(&dest, &tree)
            {
                // A revision that can't go in a request, or a branch that moved between
                // the two requests: ask again, for the tree itself.
                let sized = sized.filter(|(named, _)| *named == commit || *named == tree);
                if let Some((_, sizes)) = sized.or_else(|| github::fetch_sizes(url, &tree)) {
                    let _ = github::keep_sizes(&dest, &tree, &sizes);
                }
            }
            commit
        }
        // Offline, or a revision the server can't name (`main~3`). A commit id mounted
        // before has already returned above, so only the server could answer this one.
        Err(err) => {
            let hint = if is_commit_id(rev) {
                ""
            } else {
                " (a branch or tag name is looked up on the server; offline, only a full commit id that was mounted before mounts)"
            };
            return Err(err).with_context(|| format!("could not fetch {rev:?} from {url}{hint}"));
        }
    };
    // Keep every mounted commit reachable, so git's maintenance never removes one a
    // workspace still reads from.
    git(&dest, &["update-ref", &format!("refs/sandfs/commits/{commit}"), &commit])?;
    Ok((dest, commit))
}

/// An empty bare repository whose `origin` is `url`, set up as a partial clone.
fn create_clone(dest: &Path, url: &str) -> Result<()> {
    let parent = dest.parent().context("bad cache path")?;
    std::fs::create_dir_all(parent)?;
    // Set up beside the destination and rename, so an interrupted setup leaves nothing behind.
    let tmp = parent.join(format!(
        ".{}.tmp-{}",
        dest.file_name().and_then(|n| n.to_str()).unwrap_or("repo"),
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    let tmp_str = tmp.to_str().context("non-UTF-8 home directory")?;
    let setup = git_outside(&["init", "-q", "--bare", tmp_str])
        .and_then(|_| git(&tmp, &["remote", "add", "origin", url]))
        .and_then(|_| git(&tmp, &["config", "remote.origin.promisor", "true"]))
        .and_then(|_| git(&tmp, &["config", "remote.origin.partialclonefilter", "blob:none"]));
    if let Err(err) = setup {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(err).context("setting up the local copy");
    }
    if let Err(err) = std::fs::rename(&tmp, dest) {
        let _ = std::fs::remove_dir_all(&tmp);
        // Another mount of the same URL may have finished first.
        if !dest.join("HEAD").exists() {
            return Err(err).context("moving the local copy into place");
        }
    }
    Ok(())
}

/// sandfs's own local copies of remote repositories under `repos`.
pub fn local_copies(repos: &Path) -> Result<Vec<PathBuf>> {
    let mut copies = Vec::new();
    let mut folders = vec![repos.to_path_buf()];
    while let Some(folder) = folders.pop() {
        let entries = match std::fs::read_dir(&folder) {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            entries => entries.with_context(|| format!("listing {}", folder.display()))?,
        };
        for entry in entries {
            let path = entry?.path();
            // Half-made copies (`.name.tmp-PID`) are hidden, and a copy is never inside another.
            let hidden = path.file_name().and_then(|n| n.to_str()).is_none_or(|n| n.starts_with('.'));
            if hidden || !path.is_dir() {
                continue;
            }
            if path.extension().is_some_and(|ext| ext == "git") && path.join("HEAD").exists() {
                copies.push(path);
            } else {
                folders.push(path);
            }
        }
    }
    copies.sort();
    Ok(copies)
}

/// Drop the file contents a local copy has fetched, keeping the commits in `keep` and
/// their folder listings, so their workspaces still open without the network and fetch
/// a file's contents again when it is next read. Commits not in `keep` are forgotten.
///
/// Nothing may be reading the copy: its workspaces are to be locked by the caller.
pub fn drop_contents(copy: &Path, keep: &BTreeSet<String>) -> Result<()> {
    let pins = git(copy, &["for-each-ref", "--format=%(refname)", "refs/sandfs/"])?;
    for pin in pins.lines() {
        let kept = pin.strip_prefix("refs/sandfs/commits/").is_some_and(|commit| keep.contains(commit));
        if !kept {
            git(copy, &["update-ref", "-d", pin])?;
        }
    }
    let packs = copy.join("objects/pack");
    let old: Vec<PathBuf> = std::fs::read_dir(&packs)
        .with_context(|| format!("listing {}", packs.display()))?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<_>>()?;
    // Commits and folder listings only. In a partial clone, every file left out is one
    // its origin can send again.
    let objects =
        git_raw(copy, &["rev-list", "--objects", "--all", "--filter=blob:none", "--missing=allow-promisor"], None)?;
    let pack = git_raw(copy, &["pack-objects", "-q", "objects/pack/pack"], Some(&objects))?;
    let pack = String::from_utf8_lossy(&pack).trim().to_owned();
    if pack.is_empty() {
        bail!("git pack-objects named no pack");
    }
    // Marked as from the promisor, as fetched packs are, so git fetches what it lacks.
    let stem = format!("pack-{pack}");
    std::fs::write(packs.join(format!("{stem}.promisor")), "")?;
    for path in old {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        if !name.starts_with(&format!("{stem}.")) {
            std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        }
    }
    // Loose objects, and indexes that may name objects or packs now gone.
    let objects = copy.join("objects");
    for entry in std::fs::read_dir(&objects)? {
        let path = entry?.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        if name.len() == 2 && name.bytes().all(|b| b.is_ascii_hexdigit()) {
            std::fs::remove_dir_all(&path)?;
        }
    }
    for index in ["info/commit-graph", "info/commit-graphs"] {
        let path = objects.join(index);
        if path.is_dir() {
            std::fs::remove_dir_all(&path)?;
        } else if path.exists() {
            std::fs::remove_file(&path)?;
        }
    }
    Ok(())
}

/// The commit `rev` names if it is a full commit id that was mounted before.
///
/// This checks the pin ref rather than the object: in a partial clone, asking git
/// about a commit it lacks makes it fetch that commit with all of its history.
fn pinned(repo: &Path, rev: &str) -> Option<String> {
    let id = rev.to_ascii_lowercase();
    let pin = format!("refs/sandfs/commits/{id}");
    (is_commit_id(&id) && git(repo, &["show-ref", "--verify", "--quiet", &pin]).is_ok()).then_some(id)
}

/// The URL the local copy fetches file contents from.
fn origin(repo: &Path) -> Option<String> {
    git(repo, &["config", "remote.origin.url"]).ok()
}

/// Whether git refused a fetch only because another fetch was rewriting the local copy's
/// list of shallow commits at the same time.
fn is_shallow_race(error: &str) -> bool {
    error.contains("shallow.lock") || error.contains("shallow file has changed since we read it")
}

/// A full commit id, which names the same commit forever.
fn is_commit_id(rev: &str) -> bool {
    matches!(rev.len(), 40 | 64) && rev.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Split a trailing `@rev`, which may contain slashes (`@feature/x`) or a reflog
/// selector (`@HEAD@{1}`). The `@` of a URL's user (`git@host:repo`,
/// `https://me@host/repo`) is left alone.
fn split_rev(spec: &str) -> (&str, Option<String>) {
    let Some(at) = spec.match_indices('@').map(|(i, _)| i).rfind(|&i| !spec[i + 1..].starts_with('{')) else {
        return (spec, None);
    };
    let (location, rev) = (&spec[..at], &spec[at + 1..]);
    // Ref names cannot contain ':', which rules out the host of `user@host:repo`, and
    // an https user is followed by a host with no path before it (`file://C:\repo` on
    // Windows has one).
    let user_of_url = location.split_once("://").is_some_and(|(_, rest)| !rest.contains(['/', '\\']));
    if at == 0 || rev.contains(':') || user_of_url { (spec, None) } else { (location, Some(rev.to_owned())) }
}

/// A URL, or scp-style `[user@]host:path`, as git reads it: a ':' before any '/'.
fn is_url(location: &str) -> bool {
    location.contains("://")
        || location.find(':').is_some_and(|colon| !location[..colon].contains(['/', '\\']) && !drive_letter(location))
}

/// `C:\repo` or `C:/repo`: a local path on Windows, as git reads it there.
fn drive_letter(location: &str) -> bool {
    let bytes = location.as_bytes();
    cfg!(windows) && bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

/// `https://github.com/owner/repo.git` becomes `github.com/owner/repo.git`.
fn cache_path(url: &str) -> Result<PathBuf> {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let rest = rest.rsplit_once('@').map_or(rest, |(_, host)| host).replacen(':', "/", 1);
    let rest = rest.trim_end_matches('/').trim_end_matches(".git");
    let mut path = PathBuf::new();
    for part in rest.split('/').filter(|part| !part.is_empty()) {
        let clean: String =
            part.chars().map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '_' }).collect();
        if clean.starts_with('.') {
            bail!("cannot cache {url:?}");
        }
        path.push(clean);
    }
    if path.as_os_str().is_empty() {
        bail!("cannot cache {url:?}");
    }
    path.set_extension("git");
    Ok(path)
}

/// Run git in `repo` with `input`, and return what it printed, untrimmed.
fn git_raw(repo: &Path, args: &[&str], input: Option<&[u8]>) -> Result<Vec<u8>> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(repo).args(["-c", "maintenance.auto=false", "-c", "gc.auto=0"]).args(args);
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    run_with_input(cmd, input).with_context(|| format!("git {}", args.join(" ")))
}

fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(repo);
    run_git(cmd, args)
}

/// Run git in no repository, for what needs none, such as asking a server for its
/// branches. Run where sandfs was started, git would take the settings of a repository
/// holding that folder, which can name programs to run: a mounted folder's `.git` is
/// one, and whatever writes to the mount can change it. An empty folder, which git
/// looks no further up from, holds none.
fn git_outside(args: &[&str]) -> Result<String> {
    let dir = tempfile::tempdir().context("creating a temporary folder")?;
    let mut cmd = Command::new("git");
    cmd.current_dir(dir.path()).env_remove("GIT_DIR").env_remove("GIT_WORK_TREE");
    if let Some(parent) = dir.path().parent() {
        cmd.env("GIT_CEILING_DIRECTORIES", parent);
    }
    run_git(cmd, args)
}

fn run_git(mut cmd: Command, args: &[&str]) -> Result<String> {
    let output = cmd.args(args).env("GIT_TERMINAL_PROMPT", "0").stdin(Stdio::null()).output().context("running git")?;
    if !output.status.success() {
        bail!("git {}: {}", args.join(" "), String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revisions_split_off_paths_and_urls() {
        let split = |spec| {
            let (location, rev) = split_rev(spec);
            (location.to_owned(), rev)
        };
        assert_eq!(split("~/src/sandfs@v1.0"), ("~/src/sandfs".into(), Some("v1.0".into())));
        assert_eq!(split("git@github.com:o/r.git"), ("git@github.com:o/r.git".into(), None));
        assert_eq!(split("git@github.com:o/r.git@dev"), ("git@github.com:o/r.git".into(), Some("dev".into())));
        assert_eq!(split("https://u@host/r"), ("https://u@host/r".into(), None));
        assert_eq!(split("git@host:repo"), ("git@host:repo".into(), None));
        assert_eq!(split("/srv/repo@feature/foo"), ("/srv/repo".into(), Some("feature/foo".into())));
        assert_eq!(split("https://u@host/r@release/1.0"), ("https://u@host/r".into(), Some("release/1.0".into())));
        assert_eq!(split("/srv/repo@HEAD@{1}"), ("/srv/repo".into(), Some("HEAD@{1}".into())));
        assert_eq!(split(r"file://C:\src\repo@v1"), (r"file://C:\src\repo".into(), Some("v1".into())));
        assert_eq!(split("/srv/repo@main@{2.days.ago}"), ("/srv/repo".into(), Some("main@{2.days.ago}".into())));
        assert!(resolve(Path::new("/nonexistent"), "repo@").is_err());
        let plain = |path: &str| without_verbatim_prefix(path.into());
        assert_eq!(plain(r"\\?\C:\src\repo"), PathBuf::from(r"C:\src\repo"));
        assert_eq!(plain(r"\\?\UNC\host\share\repo"), PathBuf::from(r"\\host\share\repo"));
        assert_eq!(plain("/srv/repo"), PathBuf::from("/srv/repo"));
    }

    #[test]
    fn object_folders_follow_borrowed_objects() {
        let tmp = tempfile::tempdir().unwrap();
        let first = tmp.path().join("first");
        std::fs::create_dir(&first).unwrap();
        run(&first, &["init", "-q", "-b", "main"]);
        std::fs::write(first.join("a.txt"), "a\n").unwrap();
        run(&first, &["add", "."]);
        run(&first, &["commit", "-q", "-m", "a"]);
        run(tmp.path(), &["clone", "-q", "--shared", "first", "second"]);
        run(tmp.path(), &["clone", "-q", "--shared", "second", "third"]);
        let objects =
            |name: &str| without_verbatim_prefix(tmp.path().join(name).join(".git/objects").canonicalize().unwrap());
        assert_eq!(
            object_folders(&tmp.path().join("third")).unwrap(),
            [objects("third"), objects("second"), objects("first")]
        );
    }

    #[test]
    fn building_the_index_leaves_a_local_repository_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        run(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("a.txt"), "a\n").unwrap();
        run(&repo, &["add", "."]);
        run(&repo, &["commit", "-q", "-m", "a"]);
        let commit = run(&repo, &["rev-parse", "HEAD"]);
        run(&repo, &["tag", "main"]);
        assert_eq!(local_branch(&repo, "HEAD").as_deref(), Some("main"), "a tag of the same name aside");
        // Settings of the repository's own that would otherwise apply to the build.
        run(&repo, &["config", "core.splitIndex", "true"]);
        let marker = tmp.path().join("hook-ran");
        std::fs::write(repo.join(".git/hooks/post-index-change"), format!("#!/bin/sh\ntouch '{}'\n", marker.display()))
            .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let hook = repo.join(".git/hooks/post-index-change");
            std::fs::set_permissions(hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let shared = || {
            std::fs::read_dir(repo.join(".git"))
                .unwrap()
                .flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with("sharedindex."))
        };
        let before = shared();

        let dir = GitDir::new(&repo, &commit, Some("main"), &tmp.path().join("home"), None).unwrap();
        assert!(!dir.common.to_string_lossy().starts_with(r"\\?\"), "a path git can open");
        let index = dir.read_file(INDEX).unwrap();
        assert!(!index.windows(4).any(|w| w == b"link"), "the index stands alone, with no shared part");
        assert_eq!(shared(), before, "nothing written into the repository");
        assert!(!marker.exists(), "the repository's hooks don't run");
    }

    #[test]
    fn branch_checks_ask_the_remote_without_the_repositorys_settings() {
        let tmp = tempfile::tempdir().unwrap();
        let remote = tmp.path().join("remote");
        std::fs::create_dir(&remote).unwrap();
        run(&remote, &["init", "-q", "-b", "main"]);
        run(&remote, &["config", "uploadpack.allowFilter", "true"]);
        std::fs::write(remote.join("a.txt"), "one").unwrap();
        run(&remote, &["add", "a.txt"]);
        run(&remote, &["commit", "-q", "-m", "first"]);
        run(&remote, &["branch", "taken"]);
        let url = format!("file://{}", remote.display());
        run(tmp.path(), &["clone", "-q", "--filter=blob:none", &url, "partial"]);
        let partial = tmp.path().join("partial");
        // Settings anything that writes to the repository could add: `origin` rewritten
        // to a program that leaves a marker.
        let marker = tmp.path().join("ran");
        run(&partial, &["config", "protocol.ext.allow", "always"]);
        let program = format!("ext::sh -c touch% {}% >&2;false", marker.display());
        run(&partial, &["config", &format!("url.{program}.insteadOf"), &url]);

        assert!(check_new_branch(&partial, "free").is_ok());
        let taken = check_new_branch(&partial, "taken").unwrap_err();
        assert!(taken.to_string().contains("already has a branch taken"), "{taken:#}");
        assert!(!marker.exists(), "the repository's settings named the program git ran");
    }

    /// Run git in `dir` for a test, with an identity for commits.
    fn run(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-C"])
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    #[test]
    fn moving_keeps_only_git_state_the_new_commit_cannot_serve() {
        let repo = tempfile::tempdir().unwrap();
        let repo = repo.path();
        run(repo, &["init", "-q", "-b", "main"]);
        let commit = |message: &str| {
            run(repo, &["commit", "-q", "--allow-empty", "-m", message]);
            run(repo, &["rev-parse", "HEAD"])
        };
        let (older, from, between, to) = (commit("zero"), commit("one"), commit("two"), commit("three"));
        run(repo, &["checkout", "-q", "-b", "side", &from]);
        let own = commit("side");
        let mv = Move { repo, from: &from, to: &to, branch: Some("main"), may_fetch: false };
        let moved = |rel: &str, data: &str| mv.file(rel, data.as_bytes(), &|_| false);

        assert_eq!(moved(".git/index", "DIRC").unwrap(), Moved::Forget);
        assert_eq!(moved(".git/HEAD", "ref: refs/heads/task\n").unwrap(), Moved::Keep);
        assert_eq!(moved(".git/HEAD", &format!("{from}\n")).unwrap(), Moved::Forget);
        // The branch the new commit is mounted as is served; another one follows.
        assert_eq!(moved(".git/refs/heads/main", &between).unwrap(), Moved::Forget);
        assert_eq!(moved(".git/refs/heads/task", &from).unwrap(), Moved::Write(format!("{to}\n")));
        assert_eq!(moved(".git/refs/heads/task", &to).unwrap(), Moved::Keep);
        assert_eq!(moved(".git/config", "[core]").unwrap(), Moved::Keep);
        // A branch or HEAD left at an older commit stays there.
        assert_eq!(moved(".git/refs/heads/old", &older).unwrap(), Moved::Keep);
        assert_eq!(moved(".git/HEAD", &older).unwrap(), Moved::Keep);
        // Packed branches go by the same rules.
        let packed = format!("# pack-refs with: peeled\n{between} refs/heads/task\n{older} refs/heads/old\n");
        let moved_packed = format!("# pack-refs with: peeled\n{to} refs/heads/task\n{older} refs/heads/old\n");
        assert_eq!(moved(".git/packed-refs", &packed).unwrap(), Moved::Write(moved_packed));
        // Commits the new one doesn't come after would be left behind.
        let err = moved(".git/refs/heads/task", &own).unwrap_err().to_string();
        assert!(err.contains("commits of its own"), "{err}");
        assert!(moved(".git/HEAD", &own).is_err());
        let packed_own = format!("{own} refs/heads/side\n");
        assert!(moved(".git/packed-refs", &packed_own).is_err());
        // Unless a file of the branch's own has replaced its packed line since.
        assert_eq!(mv.file(".git/packed-refs", packed_own.as_bytes(), &|name| name == "side").unwrap(), Moved::Keep);

        let changed = changed_between(repo, &from, &to).unwrap();
        assert!(changed.is_empty(), "empty commits change no files: {changed:?}");
    }

    #[test]
    fn remote_mounts_fetch_only_the_mounted_commit() {
        // The cache refuses URL parts that start with a dot, as tempdir names do.
        let tmp = tempfile::Builder::new().prefix("sandfs-git").tempdir().unwrap();
        let remote = tmp.path().join("remote");
        std::fs::create_dir(&remote).unwrap();
        run(&remote, &["init", "-q", "-b", "main"]);
        run(&remote, &["config", "uploadpack.allowFilter", "true"]);
        std::fs::write(remote.join("a.txt"), "one").unwrap();
        run(&remote, &["add", "a.txt"]);
        run(&remote, &["commit", "-q", "-m", "first"]);
        let first = run(&remote, &["rev-parse", "HEAD"]);
        std::fs::write(remote.join("a.txt"), "two").unwrap();
        run(&remote, &["commit", "-q", "-am", "second"]);
        let second = run(&remote, &["rev-parse", "HEAD"]);
        std::fs::write(remote.join("a.txt"), "three").unwrap();
        run(&remote, &["commit", "-q", "-am", "third"]);
        let third = run(&remote, &["rev-parse", "HEAD"]);
        let url = format!("file://{}", remote.display());
        let repos = tmp.path().join("repos");

        let (repo, commit) = resolve(&repos, &url).unwrap();
        assert_eq!(commit, third);
        // A URL that failed doesn't stick to the local copy, and a URL that fails doesn't
        // replace one that works. `tmp:sandfs-gitX/remote`, an ssh host that doesn't exist,
        // shares the local copy of `file:///tmp/sandfs-gitX/remote`.
        run(&repo, &["remote", "set-url", "origin", "file:///nowhere"]);
        assert_eq!(resolve(&repos, &format!("{url}@main")).unwrap().1, third);
        assert_eq!(run(&repo, &["config", "remote.origin.url"]), url);
        // (A Windows path has no '/' to turn into an ssh host.)
        if cfg!(unix) {
            let unreachable = remote.to_str().unwrap().trim_start_matches('/').replacen('/', ":", 1);
            assert_eq!(repos.join(cache_path(&unreachable).unwrap()), repo);
            assert!(fetch_commit(&repos, &unreachable, "main").is_err());
            assert_eq!(run(&repo, &["config", "remote.origin.url"]), url);
            // The same holds for a commit mounted before, which needs no fetch.
            assert!(fetch_commit(&repos, &unreachable, &third).is_ok());
            assert_eq!(run(&repo, &["config", "remote.origin.url"]), url);
        }
        run(&repo, &["remote", "set-url", "origin", "file:///nowhere"]);
        assert_eq!(resolve(&repos, &format!("{url}@{third}")).unwrap().1, third);
        assert_eq!(run(&repo, &["config", "remote.origin.url"]), url);
        // One commit and its listings: no history, and no file contents until read.
        assert_eq!(run(&repo, &["rev-list", "--count", "--all"]), "1");
        let objects = run(&repo, &["rev-list", "--objects", "--missing=print", &commit]);
        assert!(objects.lines().any(|line| line.starts_with('?')), "{objects}");

        // Older commits, branches and tags are fetched the same way when asked for, still
        // without the history behind them.
        assert_eq!(resolve(&repos, &format!("{url}@{second}")).unwrap().1, second);
        let commits = run(&repo, &["cat-file", "--batch-all-objects", "--batch-check=%(objecttype)"]);
        assert_eq!(commits.lines().filter(|kind| *kind == "commit").count(), 2, "{commits}");
        assert_eq!(resolve(&repos, &format!("{url}@main")).unwrap().1, third);
        let mut ws = sandfs_core::Workspace::new();
        ws.mount("/old", open(&repo, &second, None, tmp.path()).unwrap().0).unwrap();
        assert_eq!(&ws.read("/old/a.txt").unwrap()[..], b"two");

        // A commit id already fetched mounts with no request, so it works offline.
        std::fs::remove_dir_all(&remote).unwrap();
        assert_eq!(resolve(&repos, &format!("{url}@{second}")).unwrap().1, second);
        let mut ws = sandfs_core::Workspace::new();
        ws.mount("/old", open(&repo, &second, None, tmp.path()).unwrap().0).unwrap();
        assert_eq!(&ws.read("/old/a.txt").unwrap()[..], b"two");
        assert!(resolve(&repos, &format!("{url}@{first}")).is_err());
        // A name needs the server, so it fails offline instead of mounting something stale.
        let err = format!("{:#}", resolve(&repos, &format!("{url}@main")).unwrap_err());
        assert!(err.contains("only a full commit id that was mounted before mounts"), "{err}");
    }

    #[test]
    fn concurrent_mounts_of_one_remote_wait_for_each_other() {
        let tmp = tempfile::Builder::new().prefix("sandfs-git").tempdir().unwrap();
        let remote = tmp.path().join("remote");
        std::fs::create_dir(&remote).unwrap();
        run(&remote, &["init", "-q", "-b", "main"]);
        run(&remote, &["config", "uploadpack.allowFilter", "true"]);
        let commits: Vec<String> = (0..8)
            .map(|i| {
                std::fs::write(remote.join("a.txt"), i.to_string()).unwrap();
                run(&remote, &["add", "a.txt"]);
                run(&remote, &["commit", "-q", "-m", &i.to_string()]);
                run(&remote, &["rev-parse", "HEAD"])
            })
            .collect();
        let url = format!("file://{}", remote.display());
        let repos = tmp.path().join("repos");

        resolve(&repos, &url).unwrap();
        // Every fetch rewrites the local copy's shallow list, so unserialized ones collide.
        std::thread::scope(|scope| {
            let mounts: Vec<_> = commits
                .iter()
                .map(|commit| {
                    let (repos, spec) = (&repos, format!("{url}@{commit}"));
                    scope.spawn(move || resolve(repos, &spec).map(|(_, id)| id))
                })
                .collect();
            for (mount, commit) in mounts.into_iter().zip(&commits) {
                assert_eq!(&mount.join().unwrap().unwrap(), commit);
            }
        });
    }

    #[test]
    fn git_mounts_hold_a_checkout_of_the_mounted_commit() {
        let tmp = tempfile::Builder::new().prefix("sandfs-git").tempdir().unwrap();
        let remote = tmp.path().join("remote");
        std::fs::create_dir_all(remote.join("docs")).unwrap();
        run(&remote, &["init", "-q", "-b", "main"]);
        run(&remote, &["config", "uploadpack.allowFilter", "true"]);
        std::fs::write(remote.join("a.txt"), "one").unwrap();
        std::fs::write(remote.join("docs/.gitignore"), "*.tmp\n").unwrap();
        run(&remote, &["add", "-A"]);
        // A symbolic link, which a mount shows, and a submodule, which it doesn't.
        let blob = run(&remote, &["hash-object", "-w", "a.txt"]);
        run(&remote, &["update-index", "--add", "--cacheinfo", &format!("120000,{blob},link")]);
        run(&remote, &["commit", "-q", "-m", "first"]);
        let first = run(&remote, &["rev-parse", "HEAD"]);
        run(&remote, &["tag", "v1"]);
        run(&remote, &["update-index", "--add", "--cacheinfo", &format!("160000,{first},sub")]);
        run(&remote, &["commit", "-q", "-m", "second"]);
        let commit = run(&remote, &["rev-parse", "HEAD"]);
        let url = format!("file://{}", remote.display());
        let repos = tmp.path().join("repos");

        // The branch a mount starts on: the remote's default, or the one named.
        let (repo, mounted, branch) = resolve_branch(&repos, &url).unwrap();
        assert_eq!((mounted.as_str(), branch.as_deref()), (commit.as_str(), Some("main")));
        assert_eq!(resolve_branch(&repos, &format!("{url}@main")).unwrap().2.as_deref(), Some("main"));
        assert_eq!(resolve_branch(&repos, &format!("{url}@{first}")).unwrap().2, None);
        assert_eq!(resolve_branch(&repos, &format!("{url}@v1")).unwrap().2, None, "a tag is no branch");
        // (A Windows path is canonicalized to one git may not take.)
        if cfg!(unix) {
            assert_eq!(resolve_branch(&repos, remote.to_str().unwrap()).unwrap().2.as_deref(), Some("main"));
        }

        let home = tmp.path().to_path_buf();
        let dir = GitDir::new(&repo, &commit, Some("main"), &home, None).unwrap();
        let read = |key: &str| String::from_utf8(dir.read_file(key).unwrap().to_vec()).unwrap();
        assert_eq!(read("HEAD"), "ref: refs/heads/main\n");
        assert_eq!(read("refs/heads/main"), format!("{commit}\n"));
        assert_eq!(read("refs/remotes/origin/main"), format!("{commit}\n"));
        let alternates = PathBuf::from(read("objects/info/alternates").trim_end());
        assert_eq!(alternates.canonicalize().unwrap(), repo.join("objects").canonicalize().unwrap());
        assert!(read("shallow").contains(&commit), "the mounted commit has no parents here");
        let config = read("config");
        assert!(config.contains(&format!("url = {}", config_value(&url))), "{config}");
        assert!(config.contains("partialClone = origin"), "{config}");
        let quoted_home = config_value(&shell_quote(home.to_str().unwrap()));
        assert!(config.contains("git-fsmonitor") && config.contains(quoted_home.trim_matches('"')), "{config}");
        assert!(config.contains("[branch \"main\"]\n\tremote = origin\n\tmerge = refs/heads/main"), "{config}");
        let names: Vec<String> = dir.read_dir("").unwrap().into_keys().collect();
        assert_eq!(names, ["HEAD", "config", "index", "objects", "refs", "shallow"]);
        let detached = GitDir::new(&repo, &first, None, &home, None).unwrap();
        assert_eq!(&detached.read_file("HEAD").unwrap()[..], format!("{first}\n").as_bytes());
        assert!(!String::from_utf8_lossy(&detached.read_file("config").unwrap()).contains("[branch"));

        // The index lists every file and link as unchanged for git's file monitor, and
        // keeps the submodule out of the checkout.
        let missing = || run(&repo, &["rev-list", "--objects", "--missing=print", &commit]);
        let rules = run(&remote, &["rev-parse", "HEAD:docs/.gitignore"]);
        assert!(missing().contains(&format!("?{rules}")), "nothing is fetched before the index is read");
        // A repository outside sandfs's copies, as `git:/path` mounts, is never fetched into.
        let elsewhere = GitDir::new(&repo, &commit, Some("main"), &tmp.path().join("elsewhere"), None).unwrap();
        elsewhere.read_file("index").unwrap();
        assert!(missing().contains(&format!("?{rules}")), "a repository of the user's own is left alone");
        let index = tmp.path().join("index");
        let built = dir.read_file("index").unwrap();
        // Each folder's id is recorded (`-1` marks one that isn't), so a commit compares
        // only the files of the folders it changes.
        let tree = built.windows(4).position(|w| w == b"TREE").expect("folder ids");
        assert!(!built[tree..].windows(3).any(|w| w == b"\0-1"), "every folder id is known");
        std::fs::write(&index, &built).unwrap();
        // Git runs a file monitor with `sh`, which Windows may not have.
        if cfg!(unix) {
            index_marks_files_unchanged(&repo, &index, tmp.path());
        }
        // Git reads every rule file, so building the index fetched them, and only them.
        let missing = missing();
        let absent =
            |path: &str| missing.contains(&format!("?{}", run(&remote, &["rev-parse", &format!("HEAD:{path}")])));
        assert!(!absent("docs/.gitignore") && absent("a.txt"), "{missing}");
    }

    #[test]
    fn git_mounts_serve_the_files_the_local_copy_lacks() {
        let tmp = tempfile::Builder::new().prefix("sandfs-git").tempdir().unwrap();
        let remote = tmp.path().join("remote");
        for folder in ["a", "b"] {
            std::fs::create_dir_all(remote.join(folder)).unwrap();
            for i in 0..3 {
                std::fs::write(remote.join(format!("{folder}/{i}.txt")), format!("{folder}{i}")).unwrap();
            }
        }
        // Larger than one of zlib's stored blocks, and empty.
        std::fs::write(remote.join("a/big"), "x".repeat(200_000)).unwrap();
        std::fs::write(remote.join("empty"), "").unwrap();
        run(&remote, &["init", "-q", "-b", "main"]);
        run(&remote, &["config", "uploadpack.allowFilter", "true"]);
        run(&remote, &["add", "-A"]);
        run(&remote, &["commit", "-q", "-m", "first"]);
        let url = format!("file://{}", remote.display());
        let home = tmp.path().to_path_buf();
        let (repo, commit) = resolve(&home.join("repos"), &url).unwrap();
        let id = |path: &str| run(&remote, &["rev-parse", &format!("HEAD:{path}")]);
        let missing = || run(&repo, &["rev-list", "--objects", "--missing=print", &commit]);
        let absent = |path: &str| missing().contains(&format!("?{}", id(path)));

        let source = Arc::new(GitSource::open(&repo, &commit).unwrap());
        let dir = GitDir::new(&repo, &commit, Some("main"), &home, Some(source)).unwrap();
        let alternates = String::from_utf8(dir.read_file("objects/info/alternates").unwrap().to_vec()).unwrap();
        assert_eq!(alternates.lines().nth(1), Some("../sandfs-objects"), "{alternates}");
        assert_eq!(dir.read_dir("sandfs-objects").unwrap().len(), 256);
        let key = |path: &str| {
            let id = id(path);
            let listing = dir.read_dir(&format!("sandfs-objects/{}", &id[..2])).unwrap();
            assert_eq!(listing[&id[2..]].size, None, "listed without being read");
            listing[&id[2..]].key.clone()
        };
        assert!(absent("a/0.txt") && absent("b/0.txt"), "listing fetched nothing");

        // Each read is a loose object git can read, of a file fetched into the local copy.
        let objects = tmp.path().join("objects");
        let check = |path: &str, contents: &str| {
            let id = id(path);
            std::fs::create_dir_all(objects.join(&id[..2])).unwrap();
            std::fs::write(objects.join(&id[..2]).join(&id[2..]), dir.read_file(&key(path)).unwrap()).unwrap();
            let checkout = tmp.path().join("checkout");
            std::fs::create_dir_all(&checkout).unwrap();
            run(&checkout, &["init", "-q"]);
            let alternates = checkout.join(".git/objects/info/alternates");
            std::fs::write(alternates, format!("{}\n", objects.display())).unwrap();
            assert_eq!(run(&checkout, &["cat-file", "blob", &id]), contents.trim_end(), "{path}");
        };
        check("a/0.txt", "a0");
        assert!(!absent("a/0.txt") && absent("a/1.txt"), "one file of the folder");
        // The second file of a folder fetches the rest of it, but no other folder.
        check("a/1.txt", "a1");
        assert!(!absent("a/2.txt") && !absent("a/big") && absent("b/0.txt"));
        check("a/big", &"x".repeat(200_000));
        check("empty", "");

        // Given the files' sizes, each object is listed at the length a read gives.
        let b0 = id("b/0.txt");
        let sized = GitSource::open(&repo, &commit).unwrap().with_known_sizes([(b0.as_str().into(), 2)].into());
        let dir = GitDir::new(&repo, &commit, Some("main"), &home, Some(Arc::new(sized))).unwrap();
        let listing = dir.read_dir(&format!("sandfs-objects/{}", &b0[..2])).unwrap();
        let entry = &listing[&b0[2..]];
        assert!(absent("b/0.txt"), "listed without being read");
        assert_eq!(entry.size, Some(dir.read_file(&entry.key).unwrap().len() as u64));
    }

    #[test]
    fn loose_object_sizes_are_known_from_the_file_size() {
        // Around the edges of zlib's stored blocks, of 65,535 bytes each.
        for len in (0..10).chain(65520..65540).chain(131060..131080).chain([200_000]) {
            let data = vec![b'x'; len];
            assert_eq!(loose_object_size("blob", len as u64), loose_object("blob", &data).len() as u64, "{len}");
        }
    }

    /// Check the index at `index` of `repo`, as [`GitDir`] builds it.
    fn index_marks_files_unchanged(repo: &Path, index: &Path, checkout: &Path) {
        let output = Command::new("git")
            .arg("-C")
            .arg(checkout)
            .arg("--git-dir")
            .arg(repo)
            .args(["--work-tree", ".", "-c", r"core.fsmonitor=printf 'token\0'", "-c", "core.fsmonitorHookVersion=2"])
            .args(["ls-files", "-t", "-f"])
            .env("GIT_INDEX_FILE", index)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let listed = String::from_utf8(output.stdout).unwrap();
        let tags: BTreeMap<&str, char> = listed
            .lines()
            .filter_map(|line| line.split_once(' '))
            .map(|(tag, path)| (path, tag.chars().next().unwrap().to_ascii_lowercase()))
            .collect();
        assert_eq!(tags, BTreeMap::from([("a.txt", 'h'), ("docs/.gitignore", 'h'), ("link", 'h'), ("sub", 's')]));
        assert!(listed.contains("h a.txt"), "files are marked unchanged: {listed}");
    }

    #[test]
    fn remote_repos_cache_by_host_and_path() {
        assert_eq!(
            cache_path("https://github.com/lydakis/sandfs").unwrap(),
            Path::new("github.com/lydakis/sandfs.git")
        );
        assert_eq!(cache_path("git@github.com:o/r.git").unwrap(), Path::new("github.com/o/r.git"));
        assert!(cache_path("https://host/../x").is_err());
        assert!(is_url("https://github.com/o/r") && is_url("git@github.com:o/r") && is_url("github.com:o/r"));
        assert!(!is_url("/home/me/repo") && !is_url("~/src/repo") && !is_url("./a:b"));
        assert!(!is_url(r".\a:b"));
        assert_eq!(is_url(r"C:\src\repo"), cfg!(unix));
        assert_eq!(is_url("C:/src/repo"), cfg!(unix));
    }
}
