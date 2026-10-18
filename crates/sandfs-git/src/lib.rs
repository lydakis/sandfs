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
}

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
        let batch = Batch::spawn(&repo)?;
        Ok(GitSource { repo, tree: tree.as_str().into(), hash_len, batch: Mutex::new(batch) })
    }

    /// The id of the mounted tree.
    pub fn tree_id(&self) -> &str {
        &self.tree
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
        parse_tree(&self.object(key, "tree")?, self.hash_len)
    }

    fn read_file(&self, key: &str) -> Result<Bytes> {
        self.object(key, "blob")
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

fn git(repo: &Path) -> Command {
    let mut command = Command::new("git");
    // Never stop to ask for credentials while fetching blobs of a partial clone.
    command.arg("-C").arg(repo).env("GIT_TERMINAL_PROMPT", "0");
    command
}

fn io_error(err: std::io::Error) -> Error {
    Error::Source(format!("git: {err}"))
}

/// A `git cat-file --batch` process: write an object id, read a header and its bytes.
struct Batch {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl Batch {
    fn spawn(repo: &Path) -> Result<Self> {
        let mut child = git(repo)
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(io_error)?;
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().ok_or_else(|| Error::Source("git: no stdout".into()))?);
        Ok(Batch { child, stdin, stdout })
    }

    fn read(&mut self, id: &str) -> Result<(String, Bytes)> {
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
            [_, "missing"] => return Err(Error::Source(format!("object {id} is missing"))),
            _ => return Err(Error::Source(format!("git: unexpected reply {header:?}"))),
        };
        let mut data = vec![0; size + 1]; // the content is followed by a newline
        self.stdout.read_exact(&mut data).map_err(io_error)?;
        data.truncate(size);
        Ok((kind.to_owned(), Bytes::from(data)))
    }
}

impl Drop for Batch {
    fn drop(&mut self) {
        // Closing stdin tells cat-file to exit; reap it so no zombie is left behind.
        drop(self.stdin.take());
        let _ = self.child.wait();
    }
}
