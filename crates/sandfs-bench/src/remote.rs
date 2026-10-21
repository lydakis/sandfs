//! A large repository on a git server, read through a sandfs mount and through the
//! ways git puts the same files on disk.
//!
//! Every method starts from nothing (no local copy, an empty `SANDFS_HOME`) and gets
//! the same commit, then
//! 1. reads one file (the first moment the files are usable),
//! 2. reads a sample of files spread across the whole tree,
//! 3. searches one folder: reads every file in it and counts those holding a needle,
//!    as a build or a test run reads the folder it works on,
//! 4. runs `git status` for the first time,
//! 5. changes one file in that folder and commits it with `git commit -a`,
//! 6. runs `git status` again,
//! 7. makes a second independent copy to work in (a fork),
//! 8. runs `git status` in the fork for the first time,
//! 9. walks the whole copy with `find`, as a person or a tool looking for a file does,
//! 10. searches the whole copy with `grep -r`, as a person looking for a name does,
//!     then again, as the next search finds the files already read (8 and 9 skip
//!     the sparse checkout, which holds only the files it was told about),
//! 11. removes the fork.
//!
//! The git methods are a full clone, a shallow clone (depth 1), a blobless partial
//! clone (all history but no file contents until checkout) and a sparse checkout of a
//! depth-1 blobless clone told in advance exactly which files the run will read,
//! which is its best case. The fork of a git copy is `git worktree add`. Each method runs once
//! (the network makes repeats noisy rather than precise), in its own process, so the
//! peak memory reported is its own: the largest command it ran, plus for sandfs every
//! serving process.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use clap::ValueEnum;
use nix::sys::resource::{UsageWho, getrusage};

use crate::workflow::{disk_usage, tree_peak_rss};

/// Files read in step 2, besides the first one.
pub const SAMPLES: usize = 20;
/// The folder searched by default is the largest one with at most this many files.
const SUBTREE_FILES: usize = 1000;
const NEEDLE: &[u8] = b"the ";
/// What `grep -r` looks for across the whole copy: rare, so it reads every file.
const RARE: &str = "sandfs-bench-needle";
/// How long that search may take before it is stopped and reported as slower.
pub const GREP_LIMIT: Duration = Duration::from_secs(600);

/// How to get the repository's files.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Method {
    Sandfs,
    Sparse,
    Shallow,
    Blobless,
    Full,
}

impl Method {
    pub fn name(self) -> &'static str {
        match self {
            Method::Sandfs => "sandfs mount",
            Method::Sparse => "sparse checkout",
            Method::Shallow => "shallow clone",
            Method::Blobless => "blobless clone",
            Method::Full => "full clone",
        }
    }

    pub fn arg(self) -> &'static str {
        match self {
            Method::Sandfs => "sandfs",
            Method::Sparse => "sparse",
            Method::Shallow => "shallow",
            Method::Blobless => "blobless",
            Method::Full => "full",
        }
    }
}

/// What every method fetches and reads, worked out from the commit's listings.
pub struct Plan {
    pub url: String,
    pub commit: String,
    /// Regular files in the commit.
    pub files: usize,
    /// The first file read, then the sample.
    pub samples: Vec<String>,
    pub subtree: String,
    pub subtree_files: Vec<String>,
    /// The file with the most folders above it.
    pub deepest: String,
}

impl Plan {
    /// Fetch the commit at the server's `HEAD` with its listings but no file contents
    /// into `dir` (untimed), and pick what to read.
    pub fn fetch(url: &str, subtree: Option<&str>, dir: &Path) -> Result<Plan> {
        eprintln!("listing {url}");
        let dir_str = dir.to_str().context("scratch path is not UTF-8")?;
        git(None, &["clone", "-q", "--filter=blob:none", "--no-checkout", "--depth", "1", url, dir_str])?;
        Plan::load(url, subtree, dir)
    }

    /// The plan for the listings already fetched into `dir`.
    pub fn load(url: &str, subtree: Option<&str>, dir: &Path) -> Result<Plan> {
        let commit = git_output(Some(dir), &["rev-parse", "HEAD"])?;
        // Regular files only: the git source shows no symbolic links, and submodules are other
        // repositories. `-z` keeps names with unusual characters unquoted, and names
        // that aren't UTF-8 are skipped, as the sandfs source skips them. So are names
        // holding a line break, which a sparse checkout pattern can't express.
        let listing = git_bytes(Some(dir), &["ls-tree", "-r", "-z", "HEAD"])?;
        let mut files: Vec<String> = listing
            .split(|&byte| byte == 0)
            .filter_map(|line| {
                let line = std::str::from_utf8(line).ok()?;
                let (meta, path) = line.split_once('\t')?;
                let mode = meta.split(' ').next()?;
                (matches!(mode, "100644" | "100755") && !path.contains(['\n', '\r'])).then(|| path.to_owned())
            })
            .collect();
        files.sort();
        ensure!(files.len() > SAMPLES, "{url} has only {} files", files.len());
        let samples = spread(files.len()).map(|i| files[i].clone()).collect();
        let subtree = match subtree {
            Some(subtree) => subtree.trim_matches('/').to_owned(),
            None => largest_folder(&files, SUBTREE_FILES).context("no folder to search")?,
        };
        // The empty folder is the repository's root.
        let prefix = if subtree.is_empty() { String::new() } else { format!("{subtree}/") };
        let subtree_files: Vec<String> = files.iter().filter(|path| path.starts_with(&prefix)).cloned().collect();
        ensure!(!subtree_files.is_empty(), "{subtree} holds no files in {url}");
        let deepest = files.iter().max_by_key(|path| path.matches('/').count()).cloned().unwrap_or_default();
        Ok(Plan { url: url.to_owned(), commit, files: files.len(), samples, subtree, subtree_files, deepest })
    }
}

impl Plan {
    /// Write the plan to `path`, for a child process to [`Plan::read`] without running
    /// git itself: whatever the child runs counts toward its method's peak memory.
    pub fn save(&self, path: &Path) -> Result<()> {
        let json = serde_json::json!({
            "url": self.url,
            "commit": self.commit,
            "files": self.files,
            "samples": self.samples,
            "subtree": self.subtree,
            "subtree_files": self.subtree_files,
            "deepest": self.deepest,
        });
        Ok(std::fs::write(path, serde_json::to_vec(&json)?)?)
    }

    /// A plan written by [`Plan::save`].
    pub fn read(path: &Path) -> Result<Plan> {
        let json: serde_json::Value = serde_json::from_slice(&std::fs::read(path)?)?;
        let text = |key: &str| json[key].as_str().map(str::to_owned).with_context(|| format!("plan has no {key}"));
        let list = |key: &str| -> Result<Vec<String>> {
            json[key]
                .as_array()
                .with_context(|| format!("plan has no {key}"))?
                .iter()
                .map(|path| path.as_str().map(str::to_owned).context("plan path is not a string"))
                .collect()
        };
        Ok(Plan {
            url: text("url")?,
            commit: text("commit")?,
            files: usize::try_from(json["files"].as_u64().context("plan has no files")?)?,
            samples: list("samples")?,
            subtree: text("subtree")?,
            subtree_files: list("subtree_files")?,
            deepest: text("deepest")?,
        })
    }
}

/// The folder with the most files, counting those in its subfolders, of at most
/// `limit`; the shallowest one on a tie. The root is the empty folder.
fn largest_folder(files: &[String], limit: usize) -> Option<String> {
    let mut counts = std::collections::BTreeMap::<&str, usize>::new();
    counts.insert("", files.len());
    for path in files {
        let mut end = 0;
        while let Some(slash) = path[end..].find('/') {
            end += slash;
            *counts.entry(&path[..end]).or_default() += 1;
            end += 1;
        }
    }
    counts
        .into_iter()
        .filter(|(_, count)| *count <= limit)
        .max_by_key(|(folder, count)| {
            let depth = if folder.is_empty() { 0 } else { folder.matches('/').count() + 1 };
            (*count, std::cmp::Reverse(depth))
        })
        .map(|(folder, _)| folder.to_owned())
}

/// One method's result.
pub struct RemoteResult {
    pub method: Method,
    /// From nothing until files can be read: the clone and checkout, or the mount.
    pub ready: Duration,
    pub first: Duration,
    pub samples: Duration,
    pub search: Duration,
    /// The first `git status`, the commit of one changed file, and `git status` after.
    pub status: Duration,
    pub commit: Duration,
    pub status_again: Duration,
    pub fork: Duration,
    /// The first `git status` in the fork.
    pub status_fork: Duration,
    /// `find` over the whole copy; `None` for the sparse checkout.
    pub find: Option<Duration>,
    /// `grep -r` over the whole copy, which the sparse checkout doesn't hold; `None`
    /// inside when it was stopped at [`GREP_LIMIT`].
    pub grep: Option<Option<Duration>>,
    /// The same search again.
    pub grep_again: Option<Option<Duration>>,
    /// Removing the fork: unmounting and deleting it, or `git worktree remove`.
    pub remove: Duration,
    /// Files in the searched folder holding the needle, and the bytes read in all;
    /// every method must agree.
    pub matches: usize,
    pub bytes: u64,
    pub peak_rss: u64,
    pub disk: u64,
}

/// Run `method` against `plan` in `root`, an empty directory of its own.
pub fn run(plan: &Plan, method: Method, root: &Path, sandfs_bin: &Path) -> Result<RemoteResult> {
    let copy = root.join("copy");
    let forked = root.join("fork");
    let home = root.join("home");
    let sandfs = |args: &[&std::ffi::OsStr]| -> Result<()> {
        let output = Command::new(sandfs_bin).env("SANDFS_HOME", &home).args(args).stderr(Stdio::piped()).output()?;
        ensure!(output.status.success(), "sandfs {args:?}: {}", String::from_utf8_lossy(&output.stderr));
        Ok(())
    };
    // sandfs mounts outside `root`, so the disk count sees only what sandfs keeps.
    let mut mounts = Mounts {
        dir: tempfile::Builder::new().prefix("remote-mounts-").tempdir_in(root.parent().context("root")?)?,
        bin: sandfs_bin.to_owned(),
        home: home.clone(),
        mounted: false,
    };
    let (dir, fork_dir) = match method {
        Method::Sandfs => (mounts.dir.path().join("copy"), mounts.dir.path().join("fork")),
        _ => (copy.clone(), forked.clone()),
    };
    let copy_str = copy.to_str().context("scratch path is not UTF-8")?;

    let start = Instant::now();
    match method {
        Method::Sandfs => {
            std::fs::create_dir_all(&dir)?;
            let source = format!("git:{}@{}", plan.url, plan.commit);
            mounts.mounted = true;
            sandfs(&["mount".as_ref(), source.as_ref(), dir.as_os_str(), "--name".as_ref(), "copy".as_ref()])?;
        }
        Method::Full => {
            git(None, &["clone", "-q", "--no-checkout", &plan.url, copy_str])?;
            checkout(&copy, &plan.commit)?;
        }
        Method::Shallow => {
            git(None, &["init", "-q", copy_str])?;
            git(Some(&copy), &["fetch", "-q", "--depth", "1", &plan.url, &plan.commit])?;
            checkout(&copy, &plan.commit)?;
        }
        Method::Blobless => {
            git(None, &["clone", "-q", "--filter=blob:none", "--no-checkout", &plan.url, copy_str])?;
            checkout(&copy, &plan.commit)?;
        }
        Method::Sparse => {
            blobless(&plan.url, &plan.commit, &copy)?;
            // Exactly the files the run will read, which only a caller that knows them
            // in advance can list.
            let folder =
                if plan.subtree.is_empty() { "/*".to_owned() } else { format!("/{}/", pattern(&plan.subtree)) };
            let mut patterns = vec!["set".to_owned(), "--no-cone".to_owned(), folder];
            patterns.extend(plan.samples.iter().map(|path| format!("/{}", pattern(path))));
            let mut args = vec!["sparse-checkout"];
            args.extend(patterns.iter().map(String::as_str));
            git(Some(&copy), &args)?;
            checkout(&copy, &plan.commit)?;
        }
    }
    let ready = start.elapsed();

    let mut bytes = 0u64;
    let start = Instant::now();
    bytes += std::fs::read(dir.join(&plan.samples[0]))?.len() as u64;
    let first = start.elapsed();

    let start = Instant::now();
    for path in &plan.samples[1..] {
        bytes += std::fs::read(dir.join(path)).with_context(|| format!("reading {path}"))?.len() as u64;
    }
    let samples = start.elapsed();

    let start = Instant::now();
    let mut matches = 0;
    for path in &plan.subtree_files {
        let data = std::fs::read(dir.join(path)).with_context(|| format!("reading {path}"))?;
        bytes += data.len() as u64;
        matches += usize::from(data.windows(NEEDLE.len()).any(|window| window == NEEDLE));
    }
    let search = start.elapsed();

    // Git in the copy, as a person or a CI job uses it. In the mount, git's file
    // monitor is sandfs, which the mount's `.git` names.
    let in_copy = |args: &[&str]| git_output(Some(&dir), args);
    let start = Instant::now();
    let listed = in_copy(&["status", "--porcelain"])?;
    let status = start.elapsed();
    warn_unless_clean(method, "the first git status", &listed);
    let changed = dir.join(&plan.subtree_files[0]);
    let start = Instant::now();
    let mut file = std::fs::OpenOptions::new().append(true).open(&changed)?;
    std::io::Write::write_all(&mut file, b"\nchanged by sandfs-bench\n")?;
    drop(file);
    in_copy(&[
        "-c",
        "user.name=bench",
        "-c",
        "user.email=bench@example.com",
        "-c",
        "core.hooksPath=/dev/null",
        "commit",
        "-q",
        "--no-gpg-sign",
        "-a",
        "-m",
        "change",
    ])?;
    let commit = start.elapsed();
    ensure!(in_copy(&["rev-parse", "HEAD^"])? == plan.commit, "the commit isn't on top of {}", plan.commit);
    let start = Instant::now();
    let listed = in_copy(&["status", "--porcelain"])?;
    let status_again = start.elapsed();
    warn_unless_clean(method, "git status after the commit", &listed);

    let start = Instant::now();
    match method {
        Method::Sandfs => {
            std::fs::create_dir_all(&fork_dir)?;
            sandfs(&["fork".as_ref(), dir.as_os_str(), fork_dir.as_os_str(), "--name".as_ref(), "fork".as_ref()])?;
        }
        _ => {
            let fork_str = fork_dir.to_str().context("scratch path is not UTF-8")?;
            // From the day's commit, as a fork carries it.
            git(Some(&copy), &["worktree", "add", "-q", "--detach", fork_str, "HEAD"])?;
        }
    }
    let fork = start.elapsed();
    ensure!(fork_dir.join(&plan.samples[0]).is_file(), "the fork has no {}", plan.samples[0]);
    let start = Instant::now();
    let listed = git_output(Some(&fork_dir), &["status", "--porcelain"])?;
    let status_fork = start.elapsed();
    warn_unless_clean(method, "the fork's first git status", &listed);
    // What the day's work left on disk, before the whole-copy search brings in the rest.
    let disk = disk_usage(root)?;

    let whole = method != Method::Sparse;
    let find = whole
        .then(|| -> Result<Duration> {
            let start = Instant::now();
            let output = Command::new("find")
                .args([".", "-path", "./.git", "-prune", "-o", "-type", "f", "-print"])
                .current_dir(&dir)
                .stderr(Stdio::null())
                .output()?;
            let elapsed = start.elapsed();
            ensure!(output.status.success(), "find over the copy failed ({})", output.status);
            let found = output.stdout.iter().filter(|&&byte| byte == b'\n').count();
            ensure!(found >= plan.files, "find saw {found} files of {}", plan.files);
            Ok(elapsed)
        })
        .transpose()?;
    let search_all = || -> Result<Option<Duration>> {
        let start = Instant::now();
        let mut grep = Command::new("grep")
            .args(["-r", "-l", "-F", "--exclude-dir=.git", "-e", RARE, "."])
            .current_dir(&dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        loop {
            if let Some(status) = grep.try_wait()? {
                // 0: something matched, 1: nothing did; 2: an error.
                ensure!(matches!(status.code(), Some(0 | 1)), "grep -r over the copy failed ({status})");
                return Ok(Some(start.elapsed()));
            }
            if start.elapsed() > GREP_LIMIT {
                grep.kill()?;
                grep.wait()?;
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    let grep = whole.then(search_all).transpose()?;
    let grep_again = whole.then(search_all).transpose()?;

    // The serving processes' peaks, with the git helpers they keep running, read
    // before they exit.
    let mut serving = 0;
    if method == Method::Sandfs {
        // One process serves both mounts, so it is counted once.
        let mut pids = std::collections::BTreeSet::new();
        for name in ["copy", "fork"] {
            let info = std::fs::read(home.join("workspaces").join(name).join("mount.json"))?;
            let info: serde_json::Value = serde_json::from_slice(&info)?;
            pids.insert(u32::try_from(info["pid"].as_u64().context("mount.json has no pid")?)?);
        }
        for pid in pids {
            serving += tree_peak_rss(pid)?;
        }
    }
    let start = Instant::now();
    match method {
        Method::Sandfs => {
            sandfs(&["unmount".as_ref(), fork_dir.as_os_str()])?;
            sandfs(&["rm".as_ref(), "fork".as_ref()])?;
        }
        _ => {
            let fork_str = fork_dir.to_str().context("scratch path is not UTF-8")?;
            git(Some(&copy), &["worktree", "remove", "--force", fork_str])?;
        }
    }
    let remove = start.elapsed();
    ensure!(!fork_dir.join(&plan.samples[0]).exists(), "the fork still holds {}", plan.samples[0]);
    if method == Method::Sandfs {
        sandfs(&["unmount".as_ref(), dir.as_os_str()])?;
        mounts.mounted = false;
    }
    drop(mounts);
    // Every command this process ran has exited; the largest one's peak counts.
    let commands = u64::try_from(getrusage(UsageWho::RUSAGE_CHILDREN)?.max_rss())? * 1024;
    Ok(RemoteResult {
        method,
        ready,
        first,
        samples,
        search,
        status,
        commit,
        status_again,
        fork,
        status_fork,
        find,
        grep,
        grep_again,
        remove,
        matches,
        bytes,
        peak_rss: commands + serving,
        disk,
    })
}

/// Report a `git status` that lists files when nothing (else) has changed, which means
/// the copy isn't a faithful checkout, without stopping the run.
fn warn_unless_clean(method: Method, what: &str, listed: &str) {
    if !listed.is_empty() {
        let lines = listed.lines().count();
        let first = listed.lines().next().unwrap_or_default();
        eprintln!("{}: {what} listed {lines} files, starting with {first:?}", method.name());
    }
}

/// Check out `commit` in the clone at `dir` byte for byte, as sandfs serves it. The
/// clone's own attributes file outranks the repository's, so line ending, encoding,
/// `ident` and filter rules can't change what is written; worktrees share it.
fn checkout(dir: &Path, commit: &str) -> Result<()> {
    let info = dir.join(".git/info");
    std::fs::create_dir_all(&info)?;
    std::fs::write(info.join("attributes"), "* -text -eol -ident -filter -working-tree-encoding\n")?;
    git(Some(dir), &["checkout", "-q", "--detach", commit])
}

/// `SAMPLES + 1` positions spread evenly over `len` items, from the first to the last.
fn spread(len: usize) -> impl Iterator<Item = usize> {
    (0..=SAMPLES).map(move |i| i * (len - 1) / SAMPLES)
}

/// The leanest copy git offers in a new directory `dir`: the commit and its folder
/// listings, no file contents, fetching each file from `url` when it is first read.
pub fn blobless(url: &str, commit: &str, dir: &Path) -> Result<()> {
    let dir_str = dir.to_str().context("scratch path is not UTF-8")?;
    git(None, &["init", "-q", dir_str])?;
    git(Some(dir), &["remote", "add", "origin", url])?;
    git(Some(dir), &["config", "remote.origin.promisor", "true"])?;
    git(Some(dir), &["config", "remote.origin.partialclonefilter", "blob:none"])?;
    git(Some(dir), &["fetch", "-q", "--depth", "1", "--filter=blob:none", "origin", commit])
}

/// Where the sandfs method mounts. Whatever is still mounted is unmounted first when
/// this goes, so removing the directory never walks into a mount: that would list,
/// and so fetch, the whole repository.
struct Mounts {
    dir: tempfile::TempDir,
    bin: std::path::PathBuf,
    home: std::path::PathBuf,
    /// Whether a mount may still be up, so the clone methods, which never mount, and
    /// a finished run start no unmount commands that would count toward peak memory.
    mounted: bool,
}

impl Drop for Mounts {
    fn drop(&mut self) {
        if !self.mounted {
            return;
        }
        for name in ["copy", "fork"] {
            let mountpoint = self.dir.path().join(name);
            let unmounted = Command::new(&self.bin)
                .env("SANDFS_HOME", &self.home)
                .arg("unmount")
                .arg(&mountpoint)
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success());
            if !unmounted {
                let _ = Command::new("fusermount3").arg("-uz").arg(&mountpoint).stderr(Stdio::null()).status();
            }
        }
    }
}

/// `path` as a sparse-checkout pattern matching only itself: a backslash makes git
/// read a wildcard, bracket or space literally.
fn pattern(path: &str) -> String {
    let mut escaped = String::with_capacity(path.len());
    for c in path.chars() {
        if matches!(c, '\\' | '*' | '?' | '[' | ' ') {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

fn git(dir: Option<&Path>, args: &[&str]) -> Result<()> {
    git_output(dir, args).map(drop)
}

fn git_output(dir: Option<&Path>, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8(git_bytes(dir, args)?)?.trim().to_owned())
}

fn git_bytes(dir: Option<&Path>, args: &[&str]) -> Result<Vec<u8>> {
    let mut command = Command::new("git");
    if let Some(dir) = dir {
        command.arg("-C").arg(dir);
    }
    // No background maintenance: it would keep running past the measurement.
    let output = command.args(["-c", "gc.auto=0", "-c", "maintenance.auto=false"]).args(args).output()?;
    if !output.status.success() {
        bail!("git {}: {}", args.join(" "), String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::{Plan, SAMPLES, largest_folder, pattern, spread};

    #[test]
    fn samples_reach_the_end_of_the_tree() {
        let positions: Vec<usize> = spread(41).collect();
        assert_eq!((positions[0], positions[SAMPLES]), (0, 40));
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(spread(20_807).last(), Some(20_806));
    }

    #[test]
    fn the_searched_folder_is_the_largest_under_the_limit() {
        let files: Vec<String> =
            ["a/1", "a/2", "a/b/3", "a/b/4", "c/5", "top"].iter().map(|s| (*s).to_owned()).collect();
        assert_eq!(largest_folder(&files, 5).as_deref(), Some("a"));
        assert_eq!(largest_folder(&files, 3).as_deref(), Some("a/b"));
        assert_eq!(largest_folder(&files, 1).as_deref(), Some("c"));
        assert_eq!(largest_folder(&files, 6).as_deref(), Some(""), "the whole repository fits");
        let flat: Vec<String> = ["x", "y"].iter().map(|s| (*s).to_owned()).collect();
        assert_eq!(largest_folder(&flat, 10).as_deref(), Some(""));
    }

    #[test]
    fn a_saved_plan_reads_back_the_same() {
        let strings = |list: &[&str]| list.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        let plan = Plan {
            url: "https://example.com/data".to_owned(),
            commit: "0123abcd".to_owned(),
            files: 3,
            samples: strings(&["a", "b/c"]),
            subtree: "b".to_owned(),
            subtree_files: strings(&["b/c"]),
            deepest: "b/c".to_owned(),
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plan.json");
        plan.save(&path).unwrap();
        let read = Plan::read(&path).unwrap();
        assert_eq!((read.url, read.commit, read.files), (plan.url, plan.commit, plan.files));
        assert_eq!((read.samples, read.subtree, read.subtree_files), (plan.samples, plan.subtree, plan.subtree_files));
        assert_eq!(read.deepest, plan.deepest);
    }

    #[test]
    fn sparse_patterns_match_names_literally() {
        assert_eq!(pattern("app/[id]/page *.tsx"), r"app/\[id]/page\ \*.tsx");
        assert_eq!(pattern(r"a\b?"), r"a\\b\?");
    }
}
