//! GitSource against real repositories, including blob laziness in a partial clone.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use sandfs_core::{ChangeKind, Kind, Workspace};
use sandfs_git::GitSource;

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=test", "-c", "user.email=test@example.com", "-c", "init.defaultBranch=main"])
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap()
}

/// A repository with nested files, an executable, a symlink and two commits.
fn make_repo(dir: &Path) {
    git(dir, &["init", "-q"]);
    std::fs::create_dir_all(dir.join("src/deep")).unwrap();
    std::fs::write(dir.join("README.md"), "first\n").unwrap();
    std::fs::write(dir.join("src/lib.rs"), "fn main() {}\n").unwrap();
    std::fs::write(dir.join("src/deep/data.txt"), "data\n").unwrap();
    std::fs::write(dir.join("run.sh"), "#!/bin/sh\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("README.md", dir.join("link")).unwrap();
    }
    git(dir, &["add", "-A"]);
    // Hosts without an executable bit (Windows) need the mode set in the index.
    git(dir, &["update-index", "--chmod=+x", "run.sh"]);
    git(dir, &["commit", "-q", "-m", "first"]);
    git(dir, &["tag", "v1"]);
    std::fs::write(dir.join("README.md"), "second\n").unwrap();
    git(dir, &["commit", "-q", "-am", "second"]);
}

#[test]
fn mounts_a_commit_without_a_checkout() {
    let dir = tempfile::tempdir().unwrap();
    make_repo(dir.path());

    let mut ws = Workspace::new();
    ws.mount("/head", Arc::new(GitSource::open(dir.path(), "HEAD").unwrap())).unwrap();
    ws.mount("/v1", Arc::new(GitSource::open(dir.path(), "v1").unwrap())).unwrap();

    assert_eq!(ws.read_text("/head/README.md").unwrap(), "second\n");
    assert_eq!(ws.read_text("/v1/README.md").unwrap(), "first\n");
    assert_eq!(ws.read_text("/head/src/deep/data.txt").unwrap(), "data\n");
    let names: Vec<String> = ws.list("/head").unwrap().into_iter().map(|e| e.name).collect();
    assert!(names.starts_with(&["README.md".to_owned()]) && names.contains(&"src".to_owned()));
    assert_eq!(ws.stat("/head/run.sh").unwrap().kind, Kind::File { executable: true });
    #[cfg(unix)]
    assert!(!ws.exists("/head/link"), "symlinks are skipped");

    ws.write("/head/src/lib.rs", "changed\n").unwrap();
    let changes = ws.changes().unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!((changes[0].path.as_str(), changes[0].kind), ("/head/src/lib.rs", ChangeKind::Modified));
}

#[test]
fn rejects_unknown_and_option_like_revisions() {
    let dir = tempfile::tempdir().unwrap();
    make_repo(dir.path());
    assert!(GitSource::open(dir.path(), "no-such-branch").is_err());
    assert!(GitSource::open(dir.path(), "--output=/tmp/x").is_err());
}

#[test]
fn partial_clone_fetches_only_the_blobs_that_are_read() {
    let origin = tempfile::tempdir().unwrap();
    make_repo(origin.path());
    git(origin.path(), &["config", "uploadpack.allowFilter", "true"]);
    let clone = tempfile::tempdir().unwrap();
    let url = format!("file://{}", origin.path().display());
    git(clone.path(), &["clone", "-q", "--bare", "--filter=blob:none", &url, "."]);

    let missing = |clone: &Path| {
        git(clone, &["rev-list", "--objects", "--missing=print", "HEAD"]).lines().filter(|l| l.starts_with('?')).count()
    };
    let before = missing(clone.path());
    assert!(before >= 4, "the clone starts without blobs, found {before} missing");

    let mut ws = Workspace::new();
    ws.mount("/repo", Arc::new(GitSource::open(clone.path(), "HEAD").unwrap())).unwrap();
    ws.list("/repo/src/deep").unwrap();
    assert_eq!(missing(clone.path()), before, "listing directories fetches no blobs");

    assert_eq!(ws.read_text("/repo/src/lib.rs").unwrap(), "fn main() {}\n");
    assert_eq!(missing(clone.path()), before - 1, "exactly one blob was fetched");
}

/// A blobless clone of `make_repo`, which can fetch any blob by id.
fn partial_clone() -> (tempfile::TempDir, tempfile::TempDir) {
    let origin = tempfile::tempdir().unwrap();
    make_repo(origin.path());
    git(origin.path(), &["config", "uploadpack.allowFilter", "true"]);
    git(origin.path(), &["config", "uploadpack.allowAnySHA1InWant", "true"]);
    let clone = tempfile::tempdir().unwrap();
    let url = format!("file://{}", origin.path().display());
    git(clone.path(), &["clone", "-q", "--bare", "--filter=blob:none", &url, "."]);
    (origin, clone)
}

/// How many blobs of `HEAD` the clone lacks.
fn missing(clone: &Path) -> usize {
    git(clone, &["rev-list", "--objects", "--missing=print", "HEAD^{tree}"])
        .lines()
        .filter(|l| l.starts_with('?'))
        .count()
}

/// Every file below `dir`, read as found when `read` is set, like `grep -r`.
fn walk(ws: &Workspace, dir: &str, read: bool) -> usize {
    let mut files = 0;
    for entry in ws.list(dir).unwrap() {
        let path = format!("{dir}/{}", entry.name);
        if entry.kind.is_dir() {
            files += walk(ws, &path, read);
        } else {
            if read {
                ws.read(&path).unwrap();
            }
            files += 1;
        }
    }
    files
}

#[test]
fn sizes_come_from_local_blobs_and_a_folder_prefetch() {
    let (_origin, clone) = partial_clone();
    let before = missing(clone.path());
    // README.md, run.sh and, where the repository has it, the symlink.
    let root_blobs = if cfg!(unix) { 3 } else { 2 };

    let mut ws = Workspace::new();
    ws.mount("/repo", Arc::new(GitSource::open(clone.path(), "HEAD").unwrap().with_sizes().unwrap())).unwrap();
    let root = ws.list("/repo").unwrap();
    assert_eq!(root.iter().find(|e| e.name == "README.md").unwrap().size, None, "the blob isn't local");
    assert_eq!(missing(clone.path()), before, "a listing fetches nothing");

    // One file's size fetches that file alone.
    assert_eq!(ws.stat("/repo/README.md").unwrap().size, 7);
    assert_eq!(missing(clone.path()), before - 1);
    // A second fetches the rest of the folder in one request, the symlink's blob too,
    // which reading files one by one would never fetch.
    assert_eq!(ws.stat("/repo/run.sh").unwrap().size, 10);
    assert_eq!(missing(clone.path()), before - root_blobs);
    let root = ws.list("/repo").unwrap();
    assert_eq!(root.iter().find(|e| e.name == "run.sh").unwrap().size, Some(10));
    assert_eq!(root.iter().find(|e| e.name == "src").unwrap().size, None);
}

#[test]
fn a_walk_fetches_only_once_it_reads() {
    let (_origin, clone) = partial_clone();
    let before = missing(clone.path());

    let mut ws = Workspace::new();
    ws.mount("/repo", Arc::new(GitSource::open(clone.path(), "HEAD").unwrap().with_sizes().unwrap())).unwrap();
    // `find`: every name, no contents.
    assert_eq!(walk(&ws, "/repo", false), 4);
    assert_eq!(missing(clone.path()), before, "a walk that only lists fetches nothing");

    // `grep -r`: once the walk reads below its start, the whole tree comes in one request.
    walk(&ws, "/repo", true);
    assert_eq!(missing(clone.path()), 0);
}
