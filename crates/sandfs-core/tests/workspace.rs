//! Behavior tests for workspaces: laziness, forks, overlays and diffs.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bytes::Bytes;
use sandfs_core::{
    Change, ChangeKind, DirSource, Edit, Error, Kind, Listing, MemorySource, Source, SourceKey, VPath, Workspace,
};

/// A source whose listing of one directory key always fails.
#[derive(Debug)]
struct Failing {
    inner: MemorySource,
    broken_dir: &'static str,
}

impl Source for Failing {
    fn root(&self) -> sandfs_core::Result<SourceKey> {
        self.inner.root()
    }

    fn read_dir(&self, key: &str) -> sandfs_core::Result<Listing> {
        if key == self.broken_dir {
            return Err(Error::Source("listing failed".into()));
        }
        self.inner.read_dir(key)
    }

    fn read_file(&self, key: &str) -> sandfs_core::Result<Bytes> {
        self.inner.read_file(key)
    }
}

/// Wraps a source and counts how often it is asked for listings and file bytes.
#[derive(Debug)]
struct Counting {
    inner: MemorySource,
    dirs: AtomicUsize,
    files: AtomicUsize,
}

impl Counting {
    fn new(inner: MemorySource) -> Arc<Self> {
        Arc::new(Counting { inner, dirs: AtomicUsize::new(0), files: AtomicUsize::new(0) })
    }

    fn reads(&self) -> (usize, usize) {
        (self.dirs.load(Ordering::SeqCst), self.files.load(Ordering::SeqCst))
    }
}

impl Source for Counting {
    fn root(&self) -> sandfs_core::Result<SourceKey> {
        self.inner.root()
    }

    fn read_dir(&self, key: &str) -> sandfs_core::Result<Listing> {
        self.dirs.fetch_add(1, Ordering::SeqCst);
        self.inner.read_dir(key)
    }

    fn read_file(&self, key: &str) -> sandfs_core::Result<Bytes> {
        self.files.fetch_add(1, Ordering::SeqCst);
        self.inner.read_file(key)
    }
}

fn repo() -> MemorySource {
    MemorySource::from_files([
        ("README.md", "hello\n"),
        ("src/lib.rs", "pub fn one() -> u32 { 1 }\n"),
        ("src/util/mod.rs", "// util\n"),
        ("docs/guide.md", "guide\n"),
    ])
    .unwrap()
}

fn workspace() -> (Workspace, Arc<Counting>) {
    let source = Counting::new(repo());
    let mut ws = Workspace::new();
    ws.mount("/repo", source.clone()).unwrap();
    (ws, source)
}

fn names(ws: &Workspace, path: &str) -> Vec<String> {
    ws.list(path).unwrap().into_iter().map(|entry| entry.name).collect()
}

fn changes(ws: &Workspace) -> Vec<(String, ChangeKind)> {
    ws.diff().unwrap().into_iter().map(|Change { path, kind }| (path.to_string(), kind)).collect()
}

#[test]
fn reads_base_files_through_the_mount() {
    let (ws, _) = workspace();
    assert_eq!(ws.read_text("/repo/README.md").unwrap(), "hello\n");
    assert_eq!(names(&ws, "/"), ["repo"]);
    assert_eq!(names(&ws, "/repo"), ["README.md", "docs", "src"]);
    assert_eq!(ws.stat("/repo/src").unwrap().kind, Kind::Dir);
    assert!(matches!(ws.read("/repo/missing"), Err(Error::NotFound(_))));
    assert!(matches!(ws.read("/repo/src"), Err(Error::IsADirectory(_))));
}

#[test]
fn only_touched_paths_are_read_from_the_source() {
    let (ws, source) = workspace();
    assert_eq!(source.reads(), (0, 0));

    ws.read("/repo/src/lib.rs").unwrap();
    // The root and src listings, and one blob; docs/ and util/ are never listed.
    assert_eq!(source.reads(), (2, 1));

    // Stat and list use sizes from listings and read no content.
    ws.stat("/repo/README.md").unwrap();
    ws.list("/repo").unwrap();
    assert_eq!(source.reads(), (2, 1));
}

#[test]
fn forks_share_the_cache_and_not_the_changes() {
    let (mut ws, source) = workspace();
    ws.read("/repo/src/lib.rs").unwrap();
    let mut fork = ws.fork();

    fork.read("/repo/src/lib.rs").unwrap();
    assert_eq!(source.reads(), (2, 1), "the fork reuses what the parent fetched");

    fork.write("/repo/src/lib.rs", "changed").unwrap();
    ws.write("/repo/NEW.md", "new").unwrap();
    assert_eq!(ws.read_text("/repo/src/lib.rs").unwrap(), "pub fn one() -> u32 { 1 }\n");
    assert_eq!(fork.read_text("/repo/src/lib.rs").unwrap(), "changed");
    assert!(!fork.exists("/repo/NEW.md"));
}

#[test]
fn a_kept_fork_is_a_snapshot() {
    let (mut ws, _) = workspace();
    ws.write("/repo/a.txt", "1").unwrap();
    let snapshot = ws.fork();
    ws.write("/repo/a.txt", "2").unwrap();
    ws.remove("/repo/src", true).unwrap();

    ws = snapshot;
    assert_eq!(ws.read_text("/repo/a.txt").unwrap(), "1");
    assert!(ws.exists("/repo/src/lib.rs"));
}

#[test]
fn writes_create_parents_and_bump_versions() {
    let (mut ws, _) = workspace();
    assert_eq!(ws.write("/scratch/notes/a.txt", "a").unwrap(), 1);
    assert_eq!(names(&ws, "/"), ["repo", "scratch"]);
    assert_eq!(ws.stat("/repo/README.md").unwrap().version, 0);
    assert_eq!(ws.write("/repo/README.md", "v1").unwrap(), 2);
    assert_eq!(ws.write("/repo/README.md", "v2").unwrap(), 3);
}

#[test]
fn versioned_writes_detect_conflicts() {
    let (mut ws, _) = workspace();
    ws.write_versioned("/repo/README.md", "mine", 0).unwrap();
    let err = ws.write_versioned("/repo/README.md", "stale", 0).unwrap_err();
    assert_eq!(err, Error::VersionMismatch { path: "/repo/README.md".into(), expected: 0, actual: 1 });
    assert_eq!(ws.read_text("/repo/README.md").unwrap(), "mine");
}

#[test]
fn edits_require_unique_matches_and_apply_atomically() {
    let (mut ws, _) = workspace();
    ws.write("/f.txt", "a b a\n").unwrap();

    assert!(matches!(ws.edit("/f.txt", &[Edit::new("a", "x")]), Err(Error::EditFailed { .. })));
    assert!(matches!(ws.edit("/f.txt", &[Edit::new("zzz", "x")]), Err(Error::EditFailed { .. })));
    // The second edit fails, so the first is not applied either.
    assert!(ws.edit("/f.txt", &[Edit::new("b", "B"), Edit::new("nope", "x")]).is_err());
    assert_eq!(ws.read_text("/f.txt").unwrap(), "a b a\n");

    ws.edit("/f.txt", &[Edit::new("b", "B"), Edit { old: "a".into(), new: "x".into(), replace_all: true }]).unwrap();
    assert_eq!(ws.read_text("/f.txt").unwrap(), "x B x\n");
}

#[test]
fn removing_hides_base_entries_and_recreating_starts_empty() {
    let (mut ws, _) = workspace();
    assert!(matches!(ws.remove("/repo/src", false), Err(Error::DirectoryNotEmpty(_))));
    ws.remove("/repo/src", true).unwrap();
    assert!(!ws.exists("/repo/src/lib.rs"));
    assert_eq!(names(&ws, "/repo"), ["README.md", "docs"]);

    ws.mkdir("/repo/src", false).unwrap();
    assert!(names(&ws, "/repo/src").is_empty(), "old base children stay hidden");
    ws.write("/repo/src/lib.rs", "pub fn one() -> u32 { 1 }\n").unwrap();
    assert_eq!(names(&ws, "/repo/src"), ["lib.rs"]);
}

#[test]
fn mount_points_cannot_be_removed_or_replaced() {
    let (mut ws, _) = workspace();
    assert!(matches!(ws.remove("/repo", true), Err(Error::Unsupported(..))));
    assert!(matches!(ws.remove("/", true), Err(Error::Unsupported(..))));
    assert!(matches!(ws.mount("/repo", Arc::new(repo())), Err(Error::AlreadyExists(_))));
}

#[test]
fn nested_mounts_resolve_to_the_deepest_one() {
    let mut ws = Workspace::new();
    ws.mount("/a/b", Arc::new(repo())).unwrap();
    ws.mount("/a/b/src/vendor", Arc::new(MemorySource::from_files([("x.txt", "x")]).unwrap())).unwrap();
    assert_eq!(names(&ws, "/a"), ["b"]);
    assert_eq!(names(&ws, "/a/b/src"), ["lib.rs", "util", "vendor"]);
    assert_eq!(ws.read_text("/a/b/src/vendor/x.txt").unwrap(), "x");
}

#[test]
fn copies_and_renames_of_base_files_stay_lazy() {
    let (mut ws, source) = workspace();
    ws.copy("/repo/src", "/repo/src2", true).unwrap();
    ws.rename("/repo/docs", "/repo/manual").unwrap();
    assert_eq!(source.reads().1, 0, "no file content was read");

    assert_eq!(ws.read_text("/repo/src2/util/mod.rs").unwrap(), "// util\n");
    assert_eq!(ws.read_text("/repo/manual/guide.md").unwrap(), "guide\n");
    assert!(!ws.exists("/repo/docs"));
    assert!(matches!(ws.copy("/repo/src", "/repo/src/inner", true), Err(Error::Unsupported(..))));
    assert!(matches!(ws.rename("/repo/README.md", "/repo/manual"), Err(Error::AlreadyExists(_))));
}

#[test]
fn symlinks_are_followed_for_reads_and_writes() {
    let (mut ws, _) = workspace();
    ws.symlink("/repo/link.md", "README.md").unwrap();
    ws.symlink("/abs", "/repo/src/lib.rs").unwrap();
    assert_eq!(ws.read_text("/repo/link.md").unwrap(), "hello\n");
    assert_eq!(ws.read_link("/repo/link.md").unwrap(), "README.md");
    assert_eq!(ws.stat("/repo/link.md").unwrap().kind, Kind::Symlink);

    ws.write("/repo/link.md", "through").unwrap();
    assert_eq!(ws.read_text("/repo/README.md").unwrap(), "through");
    assert_eq!(ws.stat("/repo/link.md").unwrap().kind, Kind::Symlink);

    ws.symlink("/loop", "/loop").unwrap();
    assert!(matches!(ws.read("/loop"), Err(Error::Unsupported(..))));
    assert_eq!(ws.read_text("/abs").unwrap(), "pub fn one() -> u32 { 1 }\n");
}

#[test]
fn diff_reports_file_level_changes() {
    let (mut ws, _) = workspace();
    assert!(changes(&ws).is_empty());

    ws.write("/repo/README.md", "changed\n").unwrap();
    ws.write("/repo/new.txt", "new").unwrap();
    ws.remove("/repo/src/util", true).unwrap();
    ws.rename("/repo/docs/guide.md", "/repo/guide.md").unwrap();
    ws.write("/repo/src/lib.rs", "pub fn one() -> u32 { 1 }\n").unwrap(); // same bytes
    ws.write("/scratch.txt", "outside the mount").unwrap();

    assert_eq!(
        changes(&ws),
        [
            ("/repo/README.md".into(), ChangeKind::Modified),
            ("/repo/docs/guide.md".into(), ChangeKind::Deleted),
            ("/repo/guide.md".into(), ChangeKind::Added),
            ("/repo/new.txt".into(), ChangeKind::Added),
            ("/repo/src/util/mod.rs".into(), ChangeKind::Deleted),
            ("/scratch.txt".into(), ChangeKind::Added),
        ]
    );
}

#[test]
fn diff_sees_a_directory_replaced_by_a_file() {
    let (mut ws, _) = workspace();
    ws.remove("/repo/src", true).unwrap();
    ws.write("/repo/src", "now a file").unwrap();
    assert_eq!(
        changes(&ws),
        [
            ("/repo/src".into(), ChangeKind::Added),
            ("/repo/src/lib.rs".into(), ChangeKind::Deleted),
            ("/repo/src/util/mod.rs".into(), ChangeKind::Deleted),
        ]
    );
}

#[test]
fn deleting_then_restoring_a_file_is_no_change() {
    let (mut ws, _) = workspace();
    ws.remove("/repo/docs", true).unwrap();
    ws.write("/repo/docs/guide.md", "guide\n").unwrap();
    assert!(changes(&ws).is_empty());
}

#[test]
fn dir_source_reads_a_host_directory_lazily() {
    let dir = std::env::temp_dir().join(format!("sandfs-core-test-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("sub/file.txt"), "on disk").unwrap();

    let mut ws = Workspace::new();
    ws.mount("/host", Arc::new(DirSource::new(&dir).unwrap())).unwrap();
    assert_eq!(ws.read_text("/host/sub/file.txt").unwrap(), "on disk");
    ws.write("/host/sub/file.txt", "in the overlay").unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("sub/file.txt")).unwrap(), "on disk", "the host is never written");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn paths_cannot_escape_the_root() {
    let (ws, _) = workspace();
    assert!(matches!(ws.read("/../etc/passwd"), Err(Error::InvalidPath(_))));
    assert_eq!(VPath::parse("/repo/../repo/./README.md").unwrap().as_str(), "/repo/README.md");
}

#[test]
fn versions_are_never_reused_after_a_delete() {
    let (mut ws, _) = workspace();
    let stale = ws.stat("/repo/README.md").unwrap().version;
    ws.remove("/repo/README.md", false).unwrap();
    assert!(matches!(ws.write_versioned("/repo/README.md", "stale", stale), Err(Error::VersionMismatch { .. })));

    let first = ws.write("/new.txt", "1").unwrap();
    ws.remove("/new.txt", false).unwrap();
    ws.write("/new.txt", "2").unwrap();
    assert!(matches!(ws.write_versioned("/new.txt", "stale", first), Err(Error::VersionMismatch { .. })));
}

#[test]
fn invalid_symlink_targets_are_rejected_not_rewritten() {
    let mut source = MemorySource::from_files([("\u{fffd}", "wrong file")]).unwrap();
    source.insert("link", Bytes::from_static(b"\xff"), Kind::Symlink).unwrap();
    let mut ws = Workspace::new();
    ws.mount("/m", Arc::new(source)).unwrap();
    assert!(matches!(ws.read("/m/link"), Err(Error::NotText(_))));
    assert!(matches!(ws.read_link("/m/link"), Err(Error::NotText(_))));
}

#[test]
fn failed_copies_and_renames_change_nothing() {
    let source = Failing { inner: repo(), broken_dir: "src/util" };
    let mut ws = Workspace::new();
    ws.mount("/repo", Arc::new(source)).unwrap();

    assert!(ws.copy("/repo/src", "/repo/src2", true).is_err());
    assert!(!ws.exists("/repo/src2"));
    assert!(ws.rename("/repo/src", "/repo/moved").is_err());
    assert!(!ws.exists("/repo/moved"));
    assert!(ws.exists("/repo/src/lib.rs"));
    assert!(changes(&ws).is_empty());
}

#[test]
fn symlink_creation_reports_source_errors() {
    let mut ws = Workspace::new();
    ws.mount("/repo", Arc::new(Failing { inner: repo(), broken_dir: "" })).unwrap();
    assert!(matches!(ws.symlink("/repo/link", "README.md"), Err(Error::Source(_))));
}

#[test]
fn list_follows_a_final_directory_symlink() {
    let (mut ws, _) = workspace();
    ws.symlink("/alias", "/repo/src").unwrap();
    assert_eq!(names(&ws, "/alias"), ["lib.rs", "util"]);
}

#[test]
fn mounting_over_a_deleted_path_makes_the_mount_visible() {
    let (mut ws, _) = workspace();
    ws.remove("/repo/docs", true).unwrap();
    ws.mount("/repo/docs", Arc::new(MemorySource::from_files([("new.md", "new")]).unwrap())).unwrap();
    assert_eq!(ws.read_text("/repo/docs/new.md").unwrap(), "new");

    ws.remove("/repo/src", true).unwrap();
    ws.mount("/repo/src/vendor", Arc::new(MemorySource::from_files([("v.txt", "v")]).unwrap())).unwrap();
    assert_eq!(names(&ws, "/repo/src"), ["vendor"], "the deleted base entries stay hidden");
}

#[test]
fn memory_source_rejects_a_file_that_is_also_a_directory() {
    assert!(MemorySource::from_files([("a", "file"), ("a/b", "child")]).is_err());
    assert!(MemorySource::from_files([("a/b", "child"), ("a", "file")]).is_err());
}

#[cfg(unix)]
#[test]
fn dir_source_refuses_reads_through_a_swapped_in_symlink() {
    let base = std::env::temp_dir().join(format!("sandfs-escape-{}", std::process::id()));
    let (root, outside) = (base.join("root"), base.join("outside"));
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(root.join("sub/file"), "inside").unwrap();
    std::fs::write(outside.join("file"), "SECRET").unwrap();

    let mut ws = Workspace::new();
    ws.mount("/host", Arc::new(DirSource::new(&root).unwrap())).unwrap();
    ws.list("/host/sub").unwrap();
    std::fs::remove_dir_all(root.join("sub")).unwrap();
    std::os::unix::fs::symlink(&outside, root.join("sub")).unwrap();

    assert!(ws.read("/host/sub/file").is_err());
    std::fs::remove_dir_all(&base).unwrap();
}
