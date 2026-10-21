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

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use sandfs_core::{Kind, Listing, Source, SourceEntry, SourceKey};
use sandfs_git::{GitSource, ObjectIds};

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
    let dir = GitDir::new(repo, commit, branch, home, Some(Arc::clone(&files)))?;
    Ok((files, Arc::new(dir)))
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
        let info = git(repo, &["rev-parse", "--git-common-dir", "--show-object-format"])?;
        let mut lines = info.lines();
        let common = repo.join(lines.next().context("git rev-parse printed nothing")?);
        let common = common.canonicalize().with_context(|| format!("no repository at {}", common.display()))?;
        let common = without_verbatim_prefix(common);
        let format = lines.next().unwrap_or("sha1");
        let get = |key: &str| git(&common, &["config", "--get", key]).ok();
        // A partial clone (every remote's local copy) borrows its origin; a local
        // repository is the origin itself.
        let promisor = get("remote.origin.promisor").is_some_and(|value| value == "true");
        let origin = match get("remote.origin.url") {
            Some(url) if promisor => url,
            _ => repo.to_str().context("non-UTF-8 repository path")?.to_owned(),
        };
        let managed = repo.starts_with(home.join("repos"));
        let objects = files.filter(|_| promisor && managed).map(|source| Objects {
            source,
            folders: OnceLock::new(),
            reads: Mutex::default(),
        });

        // Git runs the monitor with the environment of whoever runs git, which may not
        // name the same home.
        let exe = std::env::current_exe().context("finding the sandfs binary")?;
        let exe = exe.to_str().context("non-UTF-8 path to the sandfs binary")?;
        // Linux names a binary replaced since it started, as by `cargo install`, with
        // this suffix; its replacement at the same path is the one git should run.
        let exe = exe.strip_suffix(" (deleted)").unwrap_or(exe);
        let hook = format!(
            "SANDFS_HOME={} {}",
            shell_quote(home.to_str().context("non-UTF-8 home directory")?),
            shell_quote(exe)
        );
        // Newer git turns its file monitor off on network and FUSE file systems, a
        // mount among them, unless `fsmonitor.allowRemote` says otherwise. And it starts
        // a maintenance process after commits and fetches, to pack loose objects, which
        // a workspace's few aren't worth.
        let mut config = format!(
            "[core]\n\trepositoryformatversion = 1\n\tfilemode = true\n\tbare = false\n\tlogallrefupdates = true\n\
             \tfsmonitor = {}\n\tfsmonitorHookVersion = 2\n\tuntrackedCache = true\n\
             [fsmonitor]\n\tallowRemote = true\n\
             [maintenance]\n\tauto = false\n\
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
        let _ = run_with_input(cmd, Some(&wanted));
    }
}

impl GitDir {
    /// The loose objects of [`OBJECTS`] whose ids start with the hex digits `first`.
    fn list_objects(&self, objects: &Objects, first: &str) -> Option<Listing> {
        let first = u8::from_str_radix(first, 16).ok().filter(|_| first.len() == 2)?;
        let mut listing = Listing::new();
        for id in objects.source.missing_files().into_iter().flat_map(|missing| missing.starting_with(first)) {
            let key = format!("{OBJECTS}/{}/{}", &id[..2], &id[2..]);
            listing.insert(
                id[2..].to_owned(),
                SourceEntry { kind: Kind::File { executable: false }, key: key.into(), size: None, modified: None },
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
        let plain = |path: &str| without_verbatim_prefix(path.into());
        assert_eq!(plain(r"\\?\C:\src\repo"), PathBuf::from(r"C:\src\repo"));
        assert_eq!(plain(r"\\?\UNC\host\share\repo"), PathBuf::from(r"\\host\share\repo"));
        assert_eq!(plain("/srv/repo"), PathBuf::from("/srv/repo"));
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
