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

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use sandfs_core::{Kind, Listing, Source, SourceEntry, SourceKey};
use sandfs_git::GitSource;

use crate::persist::PathState;
use crate::store::expand;

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

/// The branch `rev` names in a local repository, if any.
fn local_branch(repo: &Path, rev: &str) -> Option<String> {
    if rev == "HEAD" {
        return git(repo, &["symbolic-ref", "-q", "--short", "HEAD"]).ok();
    }
    let name = rev.strip_prefix("refs/heads/").unwrap_or(rev);
    git(repo, &["show-ref", "--verify", "-q", &format!("refs/heads/{name}")]).ok().map(|_| name.to_owned())
}

/// The branch `rev` names on the server at `url`, and the commit it is at.
fn remote_branch(url: &str, rev: &str) -> Option<(String, String)> {
    if rev == "HEAD" {
        // `ref: refs/heads/main\tHEAD`, then `<commit>\tHEAD`.
        let out = git(Path::new("."), &["ls-remote", "--symref", url, "HEAD"]).ok()?;
        let branch = out.lines().find_map(|line| line.strip_prefix("ref: refs/heads/")?.strip_suffix("\tHEAD"))?;
        let tip = out.lines().find_map(|line| line.strip_suffix("\tHEAD").filter(|id| is_commit_id(id)))?;
        return Some((branch.to_owned(), tip.to_owned()));
    }
    let name = rev.strip_prefix("refs/heads/").unwrap_or(rev);
    let full = format!("refs/heads/{name}");
    let out = git(Path::new("."), &["ls-remote", url, &full]).ok()?;
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
    let files = Arc::new(GitSource::open(repo, commit)?.with_sizes()?);
    Ok((files, Arc::new(GitDir::new(repo, commit, branch, home)?)))
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
/// Symbolic links and submodules, which a mount doesn't show, are marked as outside
/// the checkout, so git doesn't take them for deleted.
#[derive(Debug)]
pub struct GitDir {
    dirs: BTreeMap<String, Listing>,
    files: BTreeMap<String, Bytes>,
    /// The repository the index is built from.
    common: PathBuf,
    commit: String,
    index: OnceLock<std::result::Result<Bytes, String>>,
}

/// The file of [`GitDir`] built when first read.
const INDEX: &str = "index";

/// Where `sandfs git-fsmonitor` keeps every path it has reported (see [`fsmonitor`]).
const TOUCHED: &str = ".git/sandfs-touched";

impl GitDir {
    pub fn new(repo: &Path, commit: &str, branch: Option<&str>, home: &Path) -> Result<Self> {
        let info = git(repo, &["rev-parse", "--git-common-dir", "--show-object-format"])?;
        let mut lines = info.lines();
        let common = repo.join(lines.next().context("git rev-parse printed nothing")?);
        let common = common.canonicalize().with_context(|| format!("no repository at {}", common.display()))?;
        let format = lines.next().unwrap_or("sha1");
        let get = |key: &str| git(&common, &["config", "--get", key]).ok();
        // A partial clone (every remote's local copy) borrows its origin; a local
        // repository is the origin itself.
        let promisor = get("remote.origin.promisor").is_some_and(|value| value == "true");
        let origin = match get("remote.origin.url") {
            Some(url) if promisor => url,
            _ => repo.to_str().context("non-UTF-8 repository path")?.to_owned(),
        };

        // Git runs the monitor with the environment of whoever runs git, which may not
        // name the same home.
        let exe = std::env::current_exe().context("finding the sandfs binary")?;
        let hook = format!(
            "SANDFS_HOME={} {}",
            shell_quote(home.to_str().context("non-UTF-8 home directory")?),
            shell_quote(exe.to_str().context("non-UTF-8 path to the sandfs binary")?)
        );
        // Newer git turns its file monitor off on network and FUSE file systems, a
        // mount among them, unless `fsmonitor.allowRemote` says otherwise.
        let mut config = format!(
            "[core]\n\trepositoryformatversion = 1\n\tfilemode = true\n\tbare = false\n\tlogallrefupdates = true\n\
             \tfsmonitor = {}\n\tfsmonitorHookVersion = 2\n\tuntrackedCache = true\n\
             [fsmonitor]\n\tallowRemote = true\n\
             [remote \"origin\"]\n\turl = {}\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n",
            config_value(&format!("{hook} git-fsmonitor")),
            config_value(&origin),
        );
        if promisor {
            let filter = get("remote.origin.partialclonefilter").unwrap_or_else(|| "blob:none".to_owned());
            config.push_str(&format!("\tpromisor = true\n\tpartialclonefilter = {}\n", config_value(&filter)));
        }
        if promisor || format != "sha1" {
            config.push_str("[extensions]\n");
            if promisor {
                config.push_str("\tpartialClone = origin\n");
            }
            if format != "sha1" {
                config.push_str(&format!("\tobjectFormat = {}\n", config_value(format)));
            }
        }

        let mut files: Vec<(String, Bytes)> = Vec::new();
        let mut dirs: Vec<String> =
            ["objects/info", "objects/pack", "refs/heads", "refs/tags"].map(String::from).into();
        match branch {
            Some(branch) => {
                files.push(("HEAD".into(), format!("ref: refs/heads/{branch}\n").into()));
                files.push((format!("refs/heads/{branch}"), format!("{commit}\n").into()));
                files.push((format!("refs/remotes/origin/{branch}"), format!("{commit}\n").into()));
                config.push_str(&format!(
                    "[branch {}]\n\tremote = origin\n\tmerge = refs/heads/{branch}\n",
                    config_value(branch)
                ));
            }
            None => files.push(("HEAD".into(), format!("{commit}\n").into())),
        }
        files.push(("config".into(), config.into()));
        let objects = common.join("objects");
        let objects = objects.to_str().context("non-UTF-8 repository path")?;
        files.push(("objects/info/alternates".into(), format!("{objects}\n").into()));
        // The commits the local copy has without their parents, this one among them.
        if let Ok(shallow) = std::fs::read(common.join("shallow")) {
            files.push(("shallow".into(), shallow.into()));
        }

        let mut dir = GitDir {
            dirs: BTreeMap::from([(String::new(), Listing::new())]),
            files: BTreeMap::new(),
            common,
            commit: commit.to_owned(),
            index: OnceLock::new(),
        };
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
            };
            dir.dirs.get_mut(parent).expect("parent added").insert(name.to_owned(), entry);
            dir.files.insert(path, data);
        }
        // Its size is unknown until it is built.
        let entry = SourceEntry { kind: Kind::File { executable: false }, key: INDEX.into(), size: None };
        dir.dirs.get_mut("").expect("root").insert(INDEX.to_owned(), entry);
        Ok(dir)
    }

    /// Add the directory `path` and its parents.
    fn add_dir(&mut self, path: &str) {
        let mut key = String::new();
        for part in path.split('/').filter(|part| !part.is_empty()) {
            let child = if key.is_empty() { part.to_owned() } else { format!("{key}/{part}") };
            let entry = SourceEntry { kind: Kind::Dir, key: child.as_str().into(), size: None };
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
                    .args(args)
                    .env("GIT_INDEX_FILE", &index)
                    .env("GIT_TERMINAL_PROMPT", "0");
                run_with_input(cmd, input)
            };
            run(&["read-tree", &self.commit], None)?;
            let listed = run(&["ls-files", "-s", "-z"], None)?;
            let (mut all, mut links, mut rules) = (Vec::new(), Vec::new(), Vec::new());
            // `<mode> <id> <stage>\t<path>`
            for record in listed.split(|&b| b == 0).filter(|record| !record.is_empty()) {
                let Some(tab) = record.iter().position(|&b| b == b'\t') else { continue };
                let path = &record[tab + 1..];
                if record.starts_with(b"120000 ") || record.starts_with(b"160000 ") {
                    links.extend_from_slice(path);
                    links.push(0);
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
            if !links.is_empty() {
                run(&["update-index", "-z", "--skip-worktree", "--stdin"], Some(&links))?;
            }
            run(&["update-index", "-z", "--fsmonitor", "--fsmonitor-valid", "--stdin"], Some(&all))?;
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
        if blobs.is_empty() || git(&self.common, &["config", "--get", "remote.origin.promisor"]).is_err() {
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
        let _ = run_with_input(cmd, Some(&wanted));
    }
}

impl Source for GitDir {
    fn root(&self) -> sandfs_core::Result<SourceKey> {
        Ok("".into())
    }

    fn read_dir(&self, key: &str) -> sandfs_core::Result<Listing> {
        self.dirs.get(key).cloned().ok_or_else(|| sandfs_core::Error::Source(format!("no directory {key:?}")))
    }

    fn read_file(&self, key: &str) -> sandfs_core::Result<Bytes> {
        if key == INDEX {
            let built = self.index.get_or_init(|| self.build_index().map_err(|err| format!("{err:#}")));
            return built.clone().map_err(|err| sandfs_core::Error::Source(format!("building the git index: {err}")));
        }
        self.files.get(key).cloned().ok_or_else(|| sandfs_core::Error::Source(format!("no file {key:?}")))
    }
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
    if version != "2" {
        bail!("unsupported file monitor version {version}; set core.fsmonitorHookVersion = 2");
    }
    let previous = std::fs::read(TOUCHED).unwrap_or_default();
    let mut touched: BTreeSet<&[u8]> = previous.split(|&b| b == 0).filter(|path| !path.is_empty()).collect();
    let before = touched.len();
    let mut current = Vec::new();
    for (path, state) in saved {
        if path == ".git" || path.starts_with(".git/") {
            continue;
        }
        // A trailing slash stands for everything below a directory.
        match state {
            PathState::File => current.push(path.clone()),
            PathState::Dir => current.push(format!("{path}/")),
            PathState::Absent => current.extend([path.clone(), format!("{path}/")]),
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
    // Any new token will do: the answer never depends on it.
    let token = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
    let mut out = std::io::stdout().lock();
    write!(out, "sandfs:{token}\0")?;
    for path in touched {
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
    let fetch = loop {
        match git(&dest, &args) {
            Err(err) if waited < 300 && is_shallow_race(&format!("{err:#}")) => {
                waited += 1;
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            result => break result,
        }
    };
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
            commit?
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
    let setup = git(Path::new("."), &["init", "-q", "--bare", tmp_str])
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

fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .context("running git")?;
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
        // A symbolic link and a submodule, neither of which a mount shows.
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

        let home = tmp.path().join("home");
        let dir = GitDir::new(&repo, &commit, Some("main"), &home).unwrap();
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
        let detached = GitDir::new(&repo, &first, None, &home).unwrap();
        assert_eq!(&detached.read_file("HEAD").unwrap()[..], format!("{first}\n").as_bytes());
        assert!(!String::from_utf8_lossy(&detached.read_file("config").unwrap()).contains("[branch"));

        // The index lists every file as unchanged for git's file monitor, and keeps the
        // link and the submodule out of the checkout.
        let missing = || run(&repo, &["rev-list", "--objects", "--missing=print", &commit]);
        let rules = run(&remote, &["rev-parse", "HEAD:docs/.gitignore"]);
        assert!(missing().contains(&format!("?{rules}")), "nothing is fetched before the index is read");
        let index = tmp.path().join("index");
        std::fs::write(&index, dir.read_file("index").unwrap()).unwrap();
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
        assert_eq!(tags, BTreeMap::from([("a.txt", 'h'), ("docs/.gitignore", 'h'), ("link", 's'), ("sub", 's')]));
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
