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
    {
        assert_eq!(ws.stat("/head/link").unwrap().kind, Kind::Symlink);
        assert_eq!(ws.read_link("/head/link").unwrap(), "README.md");
    }

    ws.write("/head/src/lib.rs", "changed\n").unwrap();
    let changes = ws.changes().unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!((changes[0].path.as_str(), changes[0].kind), ("/head/src/lib.rs", ChangeKind::Modified));
}

#[test]
fn threads_reading_at_once_each_get_their_own_file() {
    use sandfs_core::Source;

    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    for i in 0..50 {
        std::fs::write(dir.path().join(format!("f{i}.txt")), format!("file {i}\n").repeat(i * 100)).unwrap();
    }
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-q", "-m", "files"]);
    let source = GitSource::open(dir.path(), "HEAD").unwrap();
    let listing = source.read_dir(&source.root().unwrap()).unwrap();
    // More readers than processes, so reads also wait for one to come free.
    let threads = std::thread::available_parallelism().map_or(1, usize::from) * 2;
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                for _ in 0..5 {
                    for i in 0..50 {
                        let entry = &listing[format!("f{i}.txt").as_str()];
                        let data = source.read_file(&entry.key).unwrap();
                        assert_eq!(data, format!("file {i}\n").repeat(i * 100).as_bytes());
                    }
                }
            });
        }
    });
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
            if read && !entry.kind.is_symlink() {
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
fn known_sizes_fill_listings_without_fetching() {
    let (_origin, clone) = partial_clone();
    let before = missing(clone.path());
    let readme = git(clone.path(), &["rev-parse", "HEAD:README.md"]);
    let sizes = [(readme.trim().into(), 7)].into_iter().collect();

    let mut ws = Workspace::new();
    let source = GitSource::open(clone.path(), "HEAD").unwrap().with_sizes().unwrap().with_known_sizes(sizes);
    ws.mount("/repo", Arc::new(source)).unwrap();
    let root = ws.list("/repo").unwrap();
    assert_eq!(root.iter().find(|e| e.name == "README.md").unwrap().size, Some(7));
    assert_eq!(root.iter().find(|e| e.name == "run.sh").unwrap().size, None, "not among the known sizes");
    assert_eq!(ws.stat("/repo/README.md").unwrap().size, 7);
    assert_eq!(missing(clone.path()), before, "neither the listing nor the size fetched anything");
    assert_eq!(ws.read_text("/repo/README.md").unwrap().len(), 7);
}

#[test]
fn a_walk_fetches_only_once_it_reads() {
    let (_origin, clone) = partial_clone();
    let before = missing(clone.path());

    let mut ws = Workspace::new();
    ws.mount("/repo", Arc::new(GitSource::open(clone.path(), "HEAD").unwrap().with_sizes().unwrap())).unwrap();
    // `find`: every name, no contents.
    assert_eq!(walk(&ws, "/repo", false), if cfg!(unix) { 5 } else { 4 });
    assert_eq!(missing(clone.path()), before, "a walk that only lists fetches nothing");

    // Reading what the walk listed: the first file of a folder comes alone, the second
    // brings the rest of the folder in one request, the symlink's blob too.
    let root_blobs = if cfg!(unix) { 3 } else { 2 };
    ws.read("/repo/README.md").unwrap();
    assert_eq!(missing(clone.path()), before - 1);
    ws.read("/repo/run.sh").unwrap();
    assert_eq!(missing(clone.path()), before - root_blobs);
    // One file in each of the other folders is no reason to fetch more.
    ws.read("/repo/src/lib.rs").unwrap();
    assert_eq!(missing(clone.path()), before - root_blobs - 1);
}

/// A blobless clone missing 2,500 files in five folders, more than one request fetches.
fn clone_with_many_files() -> (tempfile::TempDir, tempfile::TempDir) {
    let origin = tempfile::tempdir().unwrap();
    git(origin.path(), &["init", "-q"]);
    for folder in 0..5 {
        let dir = origin.path().join(format!("d{folder}"));
        std::fs::create_dir_all(&dir).unwrap();
        for file in 0..500 {
            std::fs::write(dir.join(format!("f{file}")), format!("{folder}/{file}\n")).unwrap();
        }
    }
    git(origin.path(), &["add", "-A"]);
    git(origin.path(), &["commit", "-q", "-m", "many"]);
    git(origin.path(), &["config", "uploadpack.allowFilter", "true"]);
    git(origin.path(), &["config", "uploadpack.allowAnySHA1InWant", "true"]);
    let clone = tempfile::tempdir().unwrap();
    let url = format!("file://{}", origin.path().display());
    git(clone.path(), &["clone", "-q", "--bare", "--filter=blob:none", &url, "."]);
    assert_eq!(missing(clone.path()), 2500);
    (origin, clone)
}

#[test]
fn a_tree_too_large_for_one_request_comes_in_the_background() {
    let (_origin, clone) = clone_with_many_files();

    let source = GitSource::open(clone.path(), "HEAD").unwrap().with_sizes().unwrap();
    let root = sandfs_core::Source::root(&source).unwrap();
    // A folder first, then the whole tree, as a long walk asks.
    let d0 = sandfs_core::Source::read_dir(&source, &root).unwrap()["d0"].key.clone();
    sandfs_core::Source::prefetch(&source, &d0, true);
    assert_eq!(missing(clone.path()), 2000);
    sandfs_core::Source::prefetch(&source, &root, true);
    // Requests go on while what they fetched is read, as a walk reading d1 does.
    let d1 = sandfs_core::Source::read_dir(&source, &root).unwrap()["d1"].key.clone();
    let keys: Vec<_> = sandfs_core::Source::read_dir(&source, &d1).unwrap().into_values().map(|e| e.key).collect();
    let mut files = keys.iter().cycle();
    let started = std::time::Instant::now();
    while missing(clone.path()) > 0 {
        assert!(started.elapsed() < std::time::Duration::from_secs(60), "the files never came");
        std::thread::sleep(std::time::Duration::from_millis(50));
        if missing(clone.path()) <= 1000 {
            sandfs_core::Source::read_file(&source, files.next().unwrap()).unwrap();
        }
    }
    // Listings know the sizes of what came, without reading the files.
    let d4 = sandfs_core::Source::read_dir(&source, &root).unwrap()["d4"].key.clone();
    let started = std::time::Instant::now();
    while sandfs_core::Source::read_dir(&source, &d4).unwrap()["f0"].size.is_none() {
        assert!(started.elapsed() < std::time::Duration::from_secs(10), "the sizes never showed");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[test]
fn a_background_fetch_stops_once_reads_do() {
    let (_origin, clone) = clone_with_many_files();
    let source = GitSource::open(clone.path(), "HEAD").unwrap();
    let root = sandfs_core::Source::root(&source).unwrap();
    sandfs_core::Source::prefetch(&source, &root, true);
    // The first request goes; nothing reads, so no other follows.
    let started = std::time::Instant::now();
    while missing(clone.path()) > 1500 {
        assert!(started.elapsed() < std::time::Duration::from_secs(60), "the first request never came");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    std::thread::sleep(std::time::Duration::from_secs(7));
    assert_eq!(missing(clone.path()), 1500);
}

#[test]
fn files_at_hand_are_told_from_those_on_the_server() {
    let (origin, clone) = clone_with_many_files();
    let id = |path: &str| git(clone.path(), &["rev-parse", &format!("HEAD:{path}")]).trim().to_owned();
    let (had, lacked, by_git) = (id("d0/f0"), id("d0/f1"), id("d0/f2"));
    git(clone.path(), &["cat-file", "-p", &had]);
    let source = GitSource::open(clone.path(), "HEAD").unwrap().with_sizes().unwrap();
    assert_eq!(source.missing_files().unwrap().len(), 2499);
    assert!(sandfs_core::Source::is_local(&source, &had));
    assert!(!sandfs_core::Source::is_local(&source, &lacked));
    sandfs_core::Source::read_file(&source, &lacked).unwrap();
    assert!(sandfs_core::Source::is_local(&source, &lacked), "fetched by the read");
    // Fetched by git itself, which the source hears about.
    git(clone.path(), &["cat-file", "-p", &by_git]);
    assert!(!sandfs_core::Source::is_local(&source, &by_git));
    source.fetched_elsewhere([by_git.as_str()]);
    assert!(sandfs_core::Source::is_local(&source, &by_git));
    // A repository that isn't a partial clone has every file.
    let full = GitSource::open(origin.path(), "HEAD").unwrap();
    assert!(sandfs_core::Source::is_local(&full, &lacked));
}
