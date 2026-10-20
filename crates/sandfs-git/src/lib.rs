//! A sandfs source for a git commit, read without checking it out.
//!
//! It is an optional source built on the same `Source` trait any harness can
//! implement, and a reference for writing one.
//!
//! [`GitSource`] reads trees and blobs on demand through long-running
//! `git cat-file --batch` processes: one, and a few more while several threads read at
//! once. Nothing is written to a working tree, and in a
//! partial clone (`git clone --filter=blob:none`) git fetches each blob from the
//! remote only when a file is first read, so mounting a large repository costs the
//! commit and tree objects plus whatever the agent actually opens.
//!
//! Listings carry no file sizes by default, because git keeps a blob's size inside the
//! blob. [`GitSource::with_sizes`] turns them on for files whose blobs are already
//! local. A [`Source::prefetch`] fetches a folder's missing blobs in one request,
//! so the workspace's next listing has every size, and a walk of a folder fetches
//! everything below it in one request rather than one per file.

use std::collections::HashSet;
use std::fmt;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, Once, OnceLock};

use bytes::Bytes;
use sandfs_core::{Error, Kind, Listing, Result, Source, SourceEntry, SourceKey};

/// A commit's tree, read lazily from a local git repository.
pub struct GitSource {
    repo: PathBuf,
    tree: SourceKey,
    hash_len: usize,
    readers: Readers,
    /// Set by [`GitSource::with_sizes`]: a `cat-file --batch-check` process for sizes.
    sizes: Option<Mutex<Batch>>,
    /// The remote a partial clone fetches missing blobs from.
    promisor: Option<String>,
    /// Whether the partial clone has every tree, as a clone that leaves out only blobs
    /// does, so one walk of the whole tree finds every missing file.
    trees_local: bool,
    /// Trees with every file below them local, so prefetching them does nothing.
    local: Mutex<HashSet<SourceKey>>,
    /// The blobs of the whole tree a partial clone lacked, found by one walk started in
    /// the background by the first listing, so later listings don't each start git.
    /// `None` inside when git couldn't say.
    absent: Arc<OnceLock<Option<ObjectIds>>>,
    walk: Once,
    /// Set when the source is dropped, which stops a walk still running.
    cancel: Arc<AtomicBool>,
    /// Blobs read or fetched since, which [`GitSource::absent`] may still list.
    fetched: Mutex<HashSet<SourceKey>>,
}

/// The most files one prefetch fetches. A folder missing more is left to fetch file by
/// file as it is read. Git can't tell a blob's size before fetching it, so this can't
/// be a byte limit: a prefetch fetches at most this many files of any size.
const PREFETCH_FILES: usize = 1000;

impl fmt::Debug for GitSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GitSource").field("repo", &self.repo).field("tree", &self.tree).finish()
    }
}

impl GitSource {
    /// Open `rev` (a commit, tag, branch or tree) in the repository at `repo`.
    ///
    /// The revision is resolved once, so the mount stays on that commit even if the
    /// branch moves later.
    pub fn open(repo: impl AsRef<Path>, rev: &str) -> Result<Self> {
        let repo = repo.as_ref().to_path_buf();
        if rev.starts_with('-') {
            return Err(Error::Source(format!("invalid revision {rev:?}")));
        }
        let tree = git_output(&repo, &["rev-parse", "--verify", "--end-of-options", &format!("{rev}^{{tree}}")])?;
        let hash_len = match git_output(&repo, &["rev-parse", "--show-object-format"])?.as_str() {
            "sha1" => 20,
            "sha256" => 32,
            other => return Err(Error::Source(format!("unsupported object format {other:?}"))),
        };
        let readers = Readers::new(&repo)?;
        let promisor = promisor_remote(&repo);
        let trees_local = promisor.as_ref().is_some_and(|remote| {
            let key = format!("remote.{remote}.partialclonefilter");
            git_output(&repo, &["config", "--get", &key]).is_ok_and(|filter| filter.starts_with("blob:"))
        });
        Ok(GitSource {
            repo,
            tree: tree.as_str().into(),
            hash_len,
            readers,
            sizes: None,
            promisor,
            trees_local,
            local: Mutex::default(),
            absent: Arc::default(),
            walk: Once::new(),
            cancel: Arc::default(),
            fetched: Mutex::default(),
        })
    }

    /// Report file sizes in directory listings, for the files whose blobs are local.
    ///
    /// In a partial clone a listing never fetches: the workspace asks for a
    /// [`Source::prefetch`] when it needs a size the listing left out, which fetches
    /// the folder's missing blobs in one request, and lists the folder again.
    pub fn with_sizes(mut self) -> Result<Self> {
        self.sizes = Some(Mutex::new(Batch::spawn(&self.repo, "--batch-check")?));
        Ok(self)
    }

    /// The id of the mounted tree.
    pub fn tree_id(&self) -> &str {
        &self.tree
    }

    /// Fill in sizes for the files of the tree `id` whose blobs are local.
    fn fill_sizes(&self, id: &str, listing: &mut Listing, sizes: &Mutex<Batch>) -> Result<()> {
        if listing.values().all(|entry| entry.kind.is_dir()) {
            return Ok(());
        }
        // Asking `cat-file` about a missing blob would fetch it, so leave those out.
        let missing: HashSet<String> = match (&self.promisor, self.absent()) {
            (None, _) => HashSet::new(),
            // Starting git costs more than a whole small folder, so once the walk of the
            // tree is done its answer serves every folder.
            (Some(_), Some(absent)) => {
                let fetched = self.fetched.lock().map_err(|_| Error::Source("git reader poisoned".into()))?;
                let files = listing.values().filter(|entry| !entry.kind.is_dir());
                files
                    .filter(|e| absent.contains(&e.key) && !fetched.contains(&e.key))
                    .map(|e| e.key.to_string())
                    .collect()
            }
            (Some(_), None) => self.missing(id, false, usize::MAX).map(HashSet::from_iter).unwrap_or_else(|| {
                listing.values().filter(|entry| !entry.kind.is_dir()).map(|entry| entry.key.to_string()).collect()
            }),
        };
        let mut sizes = sizes.lock().map_err(|_| Error::Source("git reader poisoned".into()))?;
        for entry in listing.values_mut().filter(|entry| !entry.kind.is_dir()) {
            if !missing.contains(&*entry.key) {
                entry.size = Some(sizes.header(&entry.key)?.1 as u64);
            }
        }
        Ok(())
    }

    /// The files of the tree a partial clone lacked when this source first looked, by
    /// blob id, waiting for that look if it hasn't finished. Files fetched since are
    /// still listed. `None` outside a partial clone that has every tree (as one that
    /// leaves out only blobs does), or when git couldn't say.
    pub fn missing_files(&self) -> Option<&ObjectIds> {
        if !self.trees_local {
            return None;
        }
        if self.absent.get().is_none() {
            // Walk here rather than wait: a walk the first listing started may still be
            // running, and two walks cost only time.
            self.walk.call_once(|| {});
            let _ = self.absent.set(walk_missing(&self.repo, &self.tree, self.hash_len, &self.cancel));
        }
        self.absent.get()?.as_ref()
    }

    /// The blobs the whole tree lacked, once the background walk has found them. The
    /// first call in a partial clone starts that walk.
    fn absent(&self) -> Option<&ObjectIds> {
        // A walk can't see the files inside trees the clone lacks.
        if !self.trees_local {
            return None;
        }
        self.walk.call_once(|| {
            let (repo, tree, absent) = (self.repo.clone(), self.tree.clone(), Arc::clone(&self.absent));
            let (hash_len, cancel) = (self.hash_len, Arc::clone(&self.cancel));
            let _ = std::thread::Builder::new().name("sandfs-git-walk".into()).spawn(move || {
                let _ = absent.set(walk_missing(&repo, &tree, hash_len, &cancel));
            });
        });
        self.absent.get()?.as_ref()
    }

    /// Note blobs that are local now, which the walk may have found missing. Once its
    /// answer is in, only blobs it lists need remembering.
    fn now_local<'a>(&self, ids: impl IntoIterator<Item = &'a str>) {
        if !self.trees_local || self.sizes.is_none() {
            return;
        }
        let absent = self.absent.get();
        if matches!(absent, Some(None)) {
            return;
        }
        let Ok(mut fetched) = self.fetched.lock() else { return };
        for id in ids {
            if absent.is_none_or(|absent| absent.as_ref().is_some_and(|absent| absent.contains(id))) {
                fetched.insert(id.into());
            }
        }
    }

    /// The blobs a partial clone lacks in the tree `id`: its own files, or with
    /// `recursive` every file below it. `None` when git can't say, or when more than
    /// `limit` are missing; git stops looking then.
    fn missing(&self, id: &str, recursive: bool, limit: usize) -> Option<Vec<String>> {
        // `--missing=print` lists absent objects as `?<id>` instead of fetching them, and
        // `tree:1` stops the walk at the tree's direct children.
        let mut command = git(&self.repo);
        command.args(["rev-list", "--objects", "--no-object-names", "--missing=print"]);
        if !recursive {
            command.arg("--filter=tree:1");
        }
        let mut child = command.arg(id).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
        let mut missing = Vec::new();
        for line in BufReader::new(child.stdout.take()?).lines() {
            // A walk cut short would pass for a complete one.
            let Ok(line) = line else {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            };
            if let Some(object) = line.strip_prefix('?') {
                missing.push(object.to_owned());
                if missing.len() > limit {
                    break;
                }
            }
        }
        let over = missing.len() > limit;
        if over {
            let _ = child.kill();
        }
        let status = child.wait().ok()?;
        (!over && status.success()).then_some(missing)
    }

    /// Fetch these blobs from `remote` in one request. Best effort: whatever this
    /// misses, git still fetches one blob at a time on first read.
    fn fetch(&self, remote: &str, blobs: &[String]) -> bool {
        // The same request git makes for a single missing object, with every id at once.
        let fetch = git(&self.repo)
            .args(["-c", "fetch.negotiationAlgorithm=noop", "fetch", remote, "--no-tags", "--no-write-fetch-head"])
            .args(["--recurse-submodules=no", "--filter=blob:none", "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        let Ok(mut fetch) = fetch else { return false };
        if let Some(mut stdin) = fetch.stdin.take() {
            let _ = stdin.write_all(blobs.join("\n").as_bytes()).and_then(|()| stdin.write_all(b"\n"));
        }
        fetch.wait().is_ok_and(|status| status.success())
    }

    /// Remember that every file below the tree `id` is local, with every tree below it,
    /// including files fetched some other way since the walk.
    fn mark_local(&self, id: &str) {
        // Trees are local in a blobless clone, so listing them fetches nothing. Read one
        // entry at a time, without names: below a large folder there are millions.
        let mut command = git(&self.repo);
        command.args(["ls-tree", "-r", "-t", "--format=%(objecttype) %(objectname)", id]);
        let Ok(mut child) = command.stdout(Stdio::piped()).stderr(Stdio::null()).spawn() else { return };
        let Some(stdout) = child.stdout.take() else { return };
        let mut trees = vec![SourceKey::from(id)];
        for line in BufReader::new(stdout).lines().map_while(std::result::Result::ok) {
            match line.split_once(' ') {
                Some(("tree", tree)) => trees.push(tree.into()),
                Some((_, file)) => self.now_local([file]),
                None => {}
            }
        }
        if child.wait().is_ok_and(|status| status.success()) {
            if let Ok(mut local) = self.local.lock() {
                local.extend(trees);
            }
        }
    }

    fn object(&self, id: &str, expected: &str) -> Result<Bytes> {
        let (kind, data) = self.readers.read(id)?;
        if kind != expected {
            return Err(Error::Source(format!("object {id} is a {kind}, expected a {expected}")));
        }
        Ok(data)
    }
}

impl Drop for GitSource {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Source for GitSource {
    fn root(&self) -> Result<SourceKey> {
        Ok(self.tree.clone())
    }

    fn read_dir(&self, key: &str) -> Result<Listing> {
        let mut listing = parse_tree(&self.object(key, "tree")?, self.hash_len)?;
        if let Some(sizes) = &self.sizes {
            self.fill_sizes(key, &mut listing, sizes)?;
        }
        Ok(listing)
    }

    fn read_file(&self, key: &str) -> Result<Bytes> {
        let data = self.object(key, "blob")?;
        self.now_local([key]);
        Ok(data)
    }

    /// Fetch the folder's missing blobs, or with `recursive` everything missing below
    /// it, in one request, when that is at most 1,000 files. Outside a partial clone
    /// every blob is local and this does nothing.
    fn prefetch(&self, key: &str, recursive: bool) {
        let Some(remote) = &self.promisor else { return };
        if self.local.lock().is_ok_and(|local| local.contains(key)) {
            return;
        }
        let Some(missing) = self.missing(key, recursive, PREFETCH_FILES) else { return };
        let fetched = missing.is_empty() || self.fetch(remote, &missing);
        if fetched {
            self.now_local(missing.iter().map(String::as_str));
            // The folder's files are all local now, including any fetched some other way
            // since the walk, which its answer would otherwise still count as missing.
            if let Ok(listing) = self.object(key, "tree").and_then(|tree| parse_tree(&tree, self.hash_len)) {
                self.now_local(listing.values().filter(|entry| !entry.kind.is_dir()).map(|entry| &*entry.key));
            }
        }
        if fetched && recursive {
            self.mark_local(key);
        }
    }
}

/// Parse a raw tree object: repeated `<octal mode> <name>\0<binary id>` records.
fn parse_tree(data: &[u8], hash_len: usize) -> Result<Listing> {
    let corrupt = || Error::Source("corrupt tree object".into());
    let mut listing = Listing::new();
    let mut rest = data;
    while !rest.is_empty() {
        let space = rest.iter().position(|&b| b == b' ').ok_or_else(corrupt)?;
        let nul = rest.iter().position(|&b| b == 0).ok_or_else(corrupt)?;
        if nul < space || rest.len() < nul + 1 + hash_len {
            return Err(corrupt());
        }
        let mode = &rest[..space];
        let name = std::str::from_utf8(&rest[space + 1..nul]);
        let id = hex(&rest[nul + 1..nul + 1 + hash_len]);
        rest = &rest[nul + 1 + hash_len..];
        let kind = match mode {
            b"40000" => Kind::Dir,
            b"100644" | b"100664" => Kind::File { executable: false },
            b"100755" => Kind::File { executable: true },
            // Symlinks (120000) are not part of a workspace, and submodules (160000)
            // point at commits in other repositories.
            _ => continue,
        };
        let Ok(name) = name else { continue }; // non-UTF-8 names cannot be workspace paths
        listing.insert(name.to_owned(), SourceEntry { kind, key: id.as_str().into(), size: None });
    }
    Ok(listing)
}

/// The blobs missing below the tree `id`, read straight into [`ObjectIds`], since a large
/// repository lacks hundreds of thousands. `None` when git can't say, or once `cancel`
/// is set, which kills the walk.
fn walk_missing(repo: &Path, id: &str, hash_len: usize, cancel: &AtomicBool) -> Option<ObjectIds> {
    let mut command = git(repo);
    command.args(["rev-list", "--objects", "--no-object-names", "--missing=print", id]);
    command.stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = command.spawn().ok()?;
    let mut ids: Vec<[u8; 32]> = Vec::new();
    for line in BufReader::new(child.stdout.take()?).lines() {
        let line = match line {
            Ok(line) if !cancel.load(Ordering::Relaxed) => line,
            // Cancelled, or cut short, which would pass for a complete walk.
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        };
        let mut blob = [0; 32];
        if line.strip_prefix('?').is_some_and(|hex| unhex(hex, &mut blob[..hash_len])) {
            ids.push(blob);
        }
    }
    child.wait().ok()?.success().then_some(())?;
    ids.sort_unstable();
    ids.dedup();
    Some(ObjectIds { sorted: ids.iter().flat_map(|blob| blob[..hash_len].iter().copied()).collect(), len: hash_len })
}

/// A set of object ids, kept as sorted raw bytes: a large repository lacks hundreds of
/// thousands of blobs, which as strings would take several times the memory.
pub struct ObjectIds {
    sorted: Vec<u8>,
    len: usize,
}

impl ObjectIds {
    /// The number of ids.
    pub fn len(&self) -> usize {
        self.sorted.len() / self.len
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.sorted.is_empty()
    }

    /// Whether the set holds the id `hex_id`.
    pub fn contains(&self, hex_id: &str) -> bool {
        self.index_of(hex_id).is_some()
    }

    /// Where `hex_id` is among the ids in order, from 0 to `len() - 1`, if it is one.
    pub fn index_of(&self, hex_id: &str) -> Option<usize> {
        let mut id = [0; 32];
        if !unhex(hex_id, &mut id[..self.len]) {
            return None;
        }
        let index = self.first_at_least(&id[..self.len]);
        (self.sorted.get(index * self.len..(index + 1) * self.len) == Some(&id[..self.len])).then_some(index)
    }

    /// The ids whose first byte is `first` (whose hex starts with its two digits), in order.
    pub fn starting_with(&self, first: u8) -> impl Iterator<Item = String> + '_ {
        let mut id = [0; 32];
        id[0] = first;
        self.sorted[self.first_at_least(&id[..self.len]) * self.len..]
            .chunks(self.len)
            .take_while(move |id| id[0] == first)
            .map(hex)
    }

    /// The index of the first id not below `id`.
    fn first_at_least(&self, id: &[u8]) -> usize {
        let (mut low, mut high) = (0, self.len());
        while low < high {
            let mid = (low + high) / 2;
            if &self.sorted[mid * self.len..(mid + 1) * self.len] < id {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        low
    }
}

impl fmt::Debug for ObjectIds {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ObjectIds").field("len", &self.len()).finish()
    }
}

/// Decode `hex` into `out`, which must be exactly half its length.
fn unhex(hex: &str, out: &mut [u8]) -> bool {
    if hex.len() != out.len() * 2 {
        return false;
    }
    for (i, byte) in out.iter_mut().enumerate() {
        match hex.get(2 * i..2 * i + 2).and_then(|pair| u8::from_str_radix(pair, 16).ok()) {
            Some(value) => *byte = value,
            None => return false,
        }
    }
    true
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn git_output(repo: &Path, args: &[&str]) -> Result<String> {
    let output = git(repo).args(args).stderr(Stdio::piped()).output().map_err(io_error)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::Source(format!("git {}: {}", args.join(" "), stderr.trim())));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// The remote a partial clone was made from, if `repo` is one.
fn promisor_remote(repo: &Path) -> Option<String> {
    let config = git_output(repo, &["config", "--get-regexp", r"^remote\..*\.promisor$"]).ok()?;
    config.lines().find_map(|line| {
        let (key, value) = line.split_once(' ')?;
        (value == "true").then(|| key.strip_prefix("remote.")?.strip_suffix(".promisor").map(str::to_owned))?
    })
}

fn git(repo: &Path) -> Command {
    let mut command = Command::new("git");
    // Never stop to ask for credentials while fetching blobs of a partial clone.
    command.arg("-C").arg(repo).env("GIT_TERMINAL_PROMPT", "0");
    command
}

fn io_error(err: std::io::Error) -> Error {
    Error::Source(format!("git: {err}"))
}

/// The `cat-file --batch` processes objects are read through. There is one to begin
/// with; a read that finds every process busy starts another, up to one per CPU, so
/// threads (or forks of a workspace) reading at once don't queue behind each other.
struct Readers {
    repo: PathBuf,
    limit: usize,
    /// The processes not reading right now, and how many there are in all.
    idle: Mutex<(Vec<Batch>, usize)>,
    returned: Condvar,
}

impl Readers {
    fn new(repo: &Path) -> Result<Self> {
        let first = Batch::spawn(repo, "--batch")?;
        Ok(Readers {
            repo: repo.to_path_buf(),
            limit: std::thread::available_parallelism().map_or(1, usize::from),
            idle: Mutex::new((vec![first], 1)),
            returned: Condvar::new(),
        })
    }

    fn read(&self, id: &str) -> Result<(String, Bytes)> {
        let mut batch = self.take()?;
        let read = batch.read(id);
        let mut idle = self.idle.lock().map_err(poisoned)?;
        // A failed read may leave the process midway through a reply, so it is replaced.
        if read.is_ok() {
            idle.0.push(batch);
        } else {
            idle.1 -= 1;
        }
        self.returned.notify_one();
        read
    }

    /// An idle process; else a new one, if there is room; else the next to come free.
    fn take(&self) -> Result<Batch> {
        let mut idle = self.idle.lock().map_err(poisoned)?;
        loop {
            if let Some(batch) = idle.0.pop() {
                return Ok(batch);
            }
            if idle.1 < self.limit {
                idle.1 += 1;
                drop(idle);
                let spawned = Batch::spawn(&self.repo, "--batch");
                if spawned.is_err() {
                    self.idle.lock().map_err(poisoned)?.1 -= 1;
                    self.returned.notify_one();
                }
                return spawned;
            }
            idle = self.returned.wait(idle).map_err(poisoned)?;
        }
    }
}

fn poisoned<T>(_: T) -> Error {
    Error::Source("git reader poisoned".into())
}

/// A `git cat-file --batch` (or `--batch-check`) process: write an object id, read a
/// header and, in `--batch` mode, the object's bytes.
struct Batch {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl Batch {
    fn spawn(repo: &Path, mode: &str) -> Result<Self> {
        let mut child = git(repo)
            .args(["cat-file", mode])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(io_error)?;
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().ok_or_else(|| Error::Source("git: no stdout".into()))?);
        Ok(Batch { child, stdin, stdout })
    }

    /// Ask for an object and read its header: the object's type and size.
    fn header(&mut self, id: &str) -> Result<(String, usize)> {
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::Source(format!("invalid object id {id:?}")));
        }
        let stdin = self.stdin.as_mut().ok_or_else(|| Error::Source("git reader closed".into()))?;
        writeln!(stdin, "{id}").and_then(|()| stdin.flush()).map_err(io_error)?;

        let mut header = String::new();
        self.stdout.read_line(&mut header).map_err(io_error)?;
        let fields: Vec<&str> = header.split_whitespace().collect();
        let (kind, size) = match fields.as_slice() {
            [_, kind, size] => (*kind, size.parse::<usize>().map_err(|_| Error::Source(format!("git: {header}")))?),
            // In a partial clone this also means the remote could not be reached.
            [_, "missing"] => {
                return Err(Error::Source(format!("object {id} is missing and could not be fetched from the remote")));
            }
            _ => return Err(Error::Source(format!("git: unexpected reply {header:?}"))),
        };
        Ok((kind.to_owned(), size))
    }

    fn read(&mut self, id: &str) -> Result<(String, Bytes)> {
        let (kind, size) = self.header(id)?;
        let mut data = vec![0; size + 1]; // the content is followed by a newline
        self.stdout.read_exact(&mut data).map_err(io_error)?;
        data.truncate(size);
        Ok((kind, Bytes::from(data)))
    }
}

impl Drop for Batch {
    fn drop(&mut self) {
        // Closing stdin tells cat-file to exit; reap it so no zombie is left behind.
        drop(self.stdin.take());
        let _ = self.child.wait();
    }
}

// Keeps the README example compiling.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeExample;

#[cfg(test)]
mod tests {
    use super::*;

    fn run(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "init.defaultBranch=main"])
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8(output.stdout).unwrap()
    }

    #[test]
    fn listings_use_one_walk_of_the_tree_once_it_is_done() {
        let origin = tempfile::tempdir().unwrap();
        run(origin.path(), &["init", "-q"]);
        std::fs::create_dir_all(origin.path().join("dir/sub")).unwrap();
        for name in ["a.txt", "b.txt", "dir/c.txt", "dir/d.txt", "dir/sub/e.txt"] {
            std::fs::write(origin.path().join(name), name).unwrap();
        }
        run(origin.path(), &["add", "-A"]);
        run(origin.path(), &["commit", "-q", "-m", "files"]);
        run(origin.path(), &["config", "uploadpack.allowFilter", "true"]);
        run(origin.path(), &["config", "uploadpack.allowAnySHA1InWant", "true"]);
        let clone = tempfile::tempdir().unwrap();
        let url = format!("file://{}", origin.path().display());
        run(clone.path(), &["clone", "-q", "--bare", "--filter=blob:none", &url, "."]);

        let source = GitSource::open(clone.path(), "HEAD").unwrap().with_sizes().unwrap();
        let root = source.root().unwrap();
        let size = |listing: &Listing, name: &str| listing[name].size;
        assert_eq!(size(&source.read_dir(&root).unwrap(), "a.txt"), None);
        // The first listing started the walk; wait for it.
        let start = std::time::Instant::now();
        while source.absent.get().is_none() {
            assert!(start.elapsed().as_secs() < 30, "the walk never finished");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(source.absent.get().unwrap().as_ref().unwrap().len(), 5);

        // What is read or fetched afterwards gets its size, though the walk found it missing.
        let listing = source.read_dir(&root).unwrap();
        assert_eq!(size(&listing, "a.txt"), None, "nothing fetched yet");
        assert_eq!(source.read_file(&listing["a.txt"].key).unwrap(), "a.txt");
        let listing = source.read_dir(&root).unwrap();
        assert_eq!((size(&listing, "a.txt"), size(&listing, "b.txt")), (Some(5), None));
        let dir = listing["dir"].key.clone();
        // One file fetched by something else, which the walk's answer doesn't know about,
        // then the rest of the folder by a prefetch.
        let c = source.read_dir(&dir).unwrap()["c.txt"].key.clone();
        run(clone.path(), &["cat-file", "-p", &c]);
        source.prefetch(&dir, false);
        let listing = source.read_dir(&dir).unwrap();
        assert_eq!((size(&listing, "c.txt"), size(&listing, "d.txt")), (Some(9), Some(9)));

        // A name git can't print as UTF-8, of a folder that is local, doesn't cut the
        // walk short before the missing file after it.
        let mktree = |entries: &[u8]| {
            let mut child = Command::new("git")
                .arg("-C")
                .arg(origin.path())
                .arg("mktree")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            child.stdin.take().unwrap().write_all(entries).unwrap();
            String::from_utf8(child.wait_with_output().unwrap().stdout).unwrap().trim().to_owned()
        };
        let blob = |path: &str| run(origin.path(), &["rev-parse", &format!("HEAD:{path}")]).trim().to_owned();
        let folder = mktree(format!("100644 blob {}\ta.txt\n", blob("a.txt")).as_bytes());
        let mut entries = format!("040000 tree {folder}\t").into_bytes();
        entries.extend_from_slice(b"\xff\n");
        entries.extend_from_slice(format!("100644 blob {}\tz\n", blob("b.txt")).as_bytes());
        let tree = mktree(&entries);
        run(origin.path(), &["tag", "odd", &tree]);
        run(clone.path(), &["fetch", "-q", "--filter=blob:none", "origin", "refs/tags/odd:refs/tags/odd"]);
        run(clone.path(), &["config", "core.quotePath", "false"]);
        let found = walk_missing(clone.path(), &tree, source.hash_len, &AtomicBool::new(false)).unwrap();
        assert_eq!(found.len(), 1, "b.txt, as z");

        // A walk stops when its source is dropped.
        let cancelled = AtomicBool::new(true);
        assert!(walk_missing(clone.path(), &root, source.hash_len, &cancelled).is_none());
        let found = walk_missing(clone.path(), &root, source.hash_len, &AtomicBool::new(false)).unwrap();
        assert_eq!(found.len(), 2, "b.txt and dir/sub/e.txt are left on the server");

        // A prefetch of everything below a folder also notes files deep inside it that
        // something else fetched.
        let sub = source.read_dir(&dir).unwrap()["sub"].key.clone();
        let e = source.read_dir(&sub).unwrap()["e.txt"].key.clone();
        run(clone.path(), &["cat-file", "-p", &e]);
        source.prefetch(&root, true);
        assert_eq!(size(&source.read_dir(&sub).unwrap(), "e.txt"), Some(13));

        // A clone that also leaves out trees lists each folder's missing files when it is
        // read, since one walk can't see inside trees that aren't there.
        let trees_too = tempfile::tempdir().unwrap();
        run(trees_too.path(), &["clone", "-q", "--bare", "--filter=tree:1", &url, "."]);
        let source = GitSource::open(trees_too.path(), "HEAD").unwrap().with_sizes().unwrap();
        let root = source.root().unwrap();
        let dir = source.read_dir(&root).unwrap()["dir"].key.clone();
        std::thread::sleep(std::time::Duration::from_millis(200));
        let listing = source.read_dir(&dir).unwrap();
        assert_eq!((size(&listing, "c.txt"), size(&listing, "d.txt")), (None, None), "nothing fetched to list");
    }

    #[test]
    fn missing_files_are_found_by_id_and_by_first_byte() {
        let origin = tempfile::tempdir().unwrap();
        run(origin.path(), &["init", "-q"]);
        for i in 0..40 {
            std::fs::write(origin.path().join(format!("{i}.txt")), i.to_string()).unwrap();
        }
        run(origin.path(), &["add", "-A"]);
        run(origin.path(), &["commit", "-q", "-m", "files"]);
        run(origin.path(), &["config", "uploadpack.allowFilter", "true"]);
        let clone = tempfile::tempdir().unwrap();
        let url = format!("file://{}", origin.path().display());
        run(clone.path(), &["clone", "-q", "--bare", "--filter=blob:none", &url, "."]);
        let mut blobs: Vec<String> =
            run(origin.path(), &["ls-tree", "--object-only", "HEAD"]).lines().map(String::from).collect();
        blobs.sort();
        blobs.dedup();

        // No listing came first, so this walks the tree itself.
        let source = GitSource::open(clone.path(), "HEAD").unwrap();
        let missing = source.missing_files().unwrap();
        assert_eq!(missing.len(), blobs.len());
        for (index, blob) in blobs.iter().enumerate() {
            assert_eq!(missing.index_of(blob), Some(index));
        }
        assert_eq!(missing.index_of(&"0".repeat(40)), None);
        assert!(!missing.contains("not hex"));
        for first in 0..=255u8 {
            let prefix = format!("{first:02x}");
            let expected: Vec<String> = blobs.iter().filter(|blob| blob.starts_with(&prefix)).cloned().collect();
            assert_eq!(missing.starting_with(first).collect::<Vec<_>>(), expected, "{prefix}");
        }
        // A repository with every blob local has nothing missing.
        assert!(GitSource::open(origin.path(), "HEAD").unwrap().missing_files().is_none());
    }
}
