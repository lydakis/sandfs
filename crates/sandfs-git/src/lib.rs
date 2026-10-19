//! A sandfs source for a git commit, read without checking it out.
//!
//! It is an optional source built on the same `Source` trait any harness can
//! implement, and a reference for writing one.
//!
//! [`GitSource`] reads trees and blobs on demand through one long-running
//! `git cat-file --batch` process. Nothing is written to a working tree, and in a
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
use std::sync::Mutex;

use bytes::Bytes;
use sandfs_core::{Error, Kind, Listing, Result, Source, SourceEntry, SourceKey};

/// A commit's tree, read lazily from a local git repository.
pub struct GitSource {
    repo: PathBuf,
    tree: SourceKey,
    hash_len: usize,
    batch: Mutex<Batch>,
    /// Set by [`GitSource::with_sizes`]: a `cat-file --batch-check` process for sizes.
    sizes: Option<Mutex<Batch>>,
    /// The remote a partial clone fetches missing blobs from.
    promisor: Option<String>,
    /// Trees with every file below them local, so prefetching them does nothing.
    local: Mutex<HashSet<SourceKey>>,
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
        let batch = Batch::spawn(&repo, "--batch")?;
        let promisor = promisor_remote(&repo);
        Ok(GitSource {
            repo,
            tree: tree.as_str().into(),
            hash_len,
            batch: Mutex::new(batch),
            sizes: None,
            promisor,
            local: Mutex::default(),
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
        let missing = match self.promisor {
            Some(_) => self.missing(id, false, usize::MAX).unwrap_or_else(|| {
                listing.values().filter(|entry| !entry.kind.is_dir()).map(|entry| entry.key.to_string()).collect()
            }),
            None => Vec::new(),
        };
        let missing: HashSet<&str> = missing.iter().map(String::as_str).collect();
        let mut sizes = sizes.lock().map_err(|_| Error::Source("git reader poisoned".into()))?;
        for entry in listing.values_mut().filter(|entry| !entry.kind.is_dir()) {
            if !missing.contains(&*entry.key) {
                entry.size = Some(sizes.header(&entry.key)?.1 as u64);
            }
        }
        Ok(())
    }

    /// The blobs a partial clone lacks in the tree `id`: its own files, or with
    /// `recursive` every file below it. `None` when git can't say, or when more than
    /// `limit` are missing; git stops looking then.
    fn missing(&self, id: &str, recursive: bool, limit: usize) -> Option<Vec<String>> {
        // `--missing=print` lists absent objects as `?<id>` instead of fetching them, and
        // `tree:1` stops the walk at the tree's direct children.
        let mut command = git(&self.repo);
        command.args(["rev-list", "--objects", "--missing=print"]);
        if !recursive {
            command.arg("--filter=tree:1");
        }
        let mut child = command.arg(id).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
        let mut missing = Vec::new();
        for line in BufReader::new(child.stdout.take()?).lines() {
            let Ok(line) = line else { break };
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

    /// Remember that every file below the tree `id` is local, with every tree below it.
    fn mark_local(&self, id: &str) {
        // Trees are local in a blobless clone, so listing them fetches nothing.
        let Ok(trees) = git_output(&self.repo, &["ls-tree", "-r", "-d", "--object-only", id]) else { return };
        let Ok(mut local) = self.local.lock() else { return };
        local.insert(id.into());
        local.extend(trees.lines().map(SourceKey::from));
    }

    fn object(&self, id: &str, expected: &str) -> Result<Bytes> {
        let mut batch = self.batch.lock().map_err(|_| Error::Source("git reader poisoned".into()))?;
        let (kind, data) = batch.read(id)?;
        if kind != expected {
            return Err(Error::Source(format!("object {id} is a {kind}, expected a {expected}")));
        }
        Ok(data)
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
        self.object(key, "blob")
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
