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

/// A blobless partial clone of `origin`, and a count of the blobs of its head commit
/// that it still lacks.
fn partial_clone(origin: &Path) -> (tempfile::TempDir, impl Fn(&Path) -> usize) {
    git(origin, &["config", "uploadpack.allowFilter", "true"]);
    git(origin, &["config", "uploadpack.allowAnySHA1InWant", "true"]);
    let clone = tempfile::tempdir().unwrap();
    let url = format!("file://{}", origin.display());
    git(clone.path(), &["clone", "-q", "--bare", "--filter=blob:none", &url, "."]);
    let missing = |clone: &Path| {
        git(clone, &["rev-list", "--objects", "--missing=print", "HEAD^{tree}"])
            .lines()
            .filter(|l| l.starts_with('?'))
            .count()
    };
    (clone, missing)
}

/// Commit `files` (path, contents) to a new repository at `dir`.
fn commit_files(dir: &Path, files: &[(String, String)]) {
    git(dir, &["init", "-q"]);
    for (path, contents) in files {
        let path = dir.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", "first"]);
}

#[test]
fn sizes_fetch_a_directory_of_a_partial_clone_in_one_request() {
    let origin = tempfile::tempdir().unwrap();
    make_repo(origin.path());
    let (clone, missing) = partial_clone(origin.path());
    let before = missing(clone.path());

    let mut ws = Workspace::new();
    ws.mount("/repo", Arc::new(GitSource::open(clone.path(), "HEAD").unwrap().with_sizes().unwrap())).unwrap();
    let src = ws.list("/repo/src").unwrap();
    assert_eq!(src.iter().find(|e| e.name == "lib.rs").unwrap().size, Some(13));
    assert_eq!(ws.stat("/repo/src/lib.rs").unwrap().size, 13);
    // Reaching /repo/src lists /repo too: its blobs (README.md, run.sh and, where the
    // repository has it, the symlink) and lib.rs were fetched, while src/deep, never
    // listed, still has its blob missing.
    let root_blobs = if cfg!(unix) { 3 } else { 2 };
    assert_eq!(missing(clone.path()), before - root_blobs - 1);

    let root = ws.list("/repo").unwrap();
    assert_eq!(root.iter().find(|e| e.name == "README.md").unwrap().size, Some(7));
    assert_eq!(root.iter().find(|e| e.name == "src").unwrap().size, None);
}

#[test]
fn walking_a_small_directory_fetches_it_whole_in_one_request() {
    let origin = tempfile::tempdir().unwrap();
    let files: Vec<(String, String)> = ["top", "a/x/1", "a/x/2", "a/y/3", "a/z/4", "a/z/deep/5", "b/6"]
        .iter()
        .map(|path| ((*path).to_owned(), format!("{path}\n")))
        .collect();
    commit_files(origin.path(), &files);
    let (clone, missing) = partial_clone(origin.path());
    assert_eq!(missing(clone.path()), 7);

    let mut ws = Workspace::new();
    ws.mount("/repo", Arc::new(GitSource::open(clone.path(), "HEAD").unwrap().with_sizes().unwrap())).unwrap();
    // One path through /repo/a fetches only the files on it.
    ws.list("/repo/a/x").unwrap();
    assert_eq!(missing(clone.path()), 4, "top, a/x/1 and a/x/2 came");
    // A second folder of /repo/a listed: it is being walked, so all of it comes,
    // a/z/deep included, while /repo/b stays out.
    ws.list("/repo/a/y").unwrap();
    assert_eq!(missing(clone.path()), 1);
    assert_eq!(ws.read_text("/repo/a/z/deep/5").unwrap(), "a/z/deep/5\n");
    assert_eq!(missing(clone.path()), 1, "nothing more was fetched for a/z");
}

#[test]
fn walking_a_large_directory_fetches_each_small_folder_in_it_whole() {
    let origin = tempfile::tempdir().unwrap();
    // Distinct contents, so each file is its own blob: big/many holds one more file
    // than a single request takes for a whole folder.
    let mut files: Vec<(String, String)> = (0..1001).map(|i| (format!("big/many/{i}"), format!("{i}\n"))).collect();
    for path in ["big/notes", "big/few/1", "big/few/sub/2", "big/other/3"] {
        files.push((path.to_owned(), format!("{path}\n")));
    }
    commit_files(origin.path(), &files);
    let (clone, missing) = partial_clone(origin.path());
    assert_eq!(missing(clone.path()), 1005);

    let mut ws = Workspace::new();
    ws.mount("/repo", Arc::new(GitSource::open(clone.path(), "HEAD").unwrap().with_sizes().unwrap())).unwrap();
    ws.list("/repo/big/other").unwrap();
    assert_eq!(missing(clone.path()), 1003, "big/notes and big/other/3 came");
    // /repo/big is now walked but too large to fetch whole, so big/few comes whole
    // and big/many stays missing.
    ws.list("/repo/big/few").unwrap();
    assert_eq!(missing(clone.path()), 1001);
    assert_eq!(ws.read_text("/repo/big/many/1000").unwrap(), "1000\n");
}
