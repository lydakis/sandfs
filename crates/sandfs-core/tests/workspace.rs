//! Behavior tests for workspaces: laziness, forks, overlays and changes.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bytes::Bytes;
use sandfs_core::{
    Change, ChangeKind, DirSource, Error, Kind, Listing, MemorySource, Source, SourceKey, Stored, VPath, Workspace,
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

/// A source that never reports sizes in its listings, like an API without metadata.
#[derive(Debug)]
struct Unsized(MemorySource);

impl Source for Unsized {
    fn root(&self) -> sandfs_core::Result<SourceKey> {
        self.0.root()
    }

    fn read_dir(&self, key: &str) -> sandfs_core::Result<Listing> {
        let mut listing = self.0.read_dir(key)?;
        for entry in listing.values_mut() {
            entry.size = None;
        }
        Ok(listing)
    }

    fn read_file(&self, key: &str) -> sandfs_core::Result<Bytes> {
        self.0.read_file(key)
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
    ws.changes().unwrap().into_iter().map(|Change { path, kind }| (path.to_string(), kind)).collect()
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
    // A rejected write leaves the file exactly as it was, version included.
    assert_eq!(ws.read_text("/repo/README.md").unwrap(), "mine");
    assert_eq!(ws.stat("/repo/README.md").unwrap().version, 1);
    assert_eq!(changes(&ws), [("/repo/README.md".into(), ChangeKind::Modified)]);
}

#[test]
fn a_replaced_file_gets_a_fresh_version() {
    // The mount replaces a file (`mv` or `cp` over it) by removing it first.
    let (mut ws, _) = workspace();
    let draft = ws.write("/repo/docs/guide.md", "draft").unwrap();
    ws.remove("/repo/docs/guide.md", false).unwrap();
    ws.copy("/repo/README.md", "/repo/docs/guide.md", false).unwrap();
    let copied = ws.stat("/repo/docs/guide.md").unwrap().version;
    assert!(copied > draft);
    assert_eq!(ws.stat("/repo/README.md").unwrap().version, 0, "the copied file is untouched");
    assert!(matches!(ws.write_versioned("/repo/docs/guide.md", "stale", draft), Err(Error::VersionMismatch { .. })));

    let base = ws.stat("/repo/src/lib.rs").unwrap().version;
    ws.write("/new.txt", "new").unwrap();
    ws.remove("/repo/src/lib.rs", false).unwrap();
    ws.rename("/new.txt", "/repo/src/lib.rs").unwrap();
    assert_ne!(ws.stat("/repo/src/lib.rs").unwrap().version, base);
    assert!(matches!(ws.write_versioned("/repo/src/lib.rs", "stale", base), Err(Error::VersionMismatch { .. })));

    assert_eq!(ws.read_text("/repo/docs/guide.md").unwrap(), "hello\n");
    assert_eq!(ws.read_text("/repo/src/lib.rs").unwrap(), "new");
    assert_eq!(
        changes(&ws),
        [("/repo/docs/guide.md".into(), ChangeKind::Modified), ("/repo/src/lib.rs".into(), ChangeKind::Modified)]
    );
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
fn each_mount_resolves_its_own_entries() {
    // Same relative paths under two mounts, with different contents and kinds.
    let mut ws = Workspace::new();
    ws.mount("/one", Arc::new(MemorySource::from_files([("data/same.csv", "1"), ("x/y", "file")]).unwrap())).unwrap();
    ws.mount("/two", Arc::new(MemorySource::from_files([("data/same.csv", "22"), ("x/y/z", "dir")]).unwrap())).unwrap();
    for _ in 0..2 {
        assert_eq!(ws.stat("/one/data/same.csv").unwrap().size, 1);
        assert_eq!(ws.stat("/two/data/same.csv").unwrap().size, 2);
        assert_eq!(ws.stat("/one/x/y").unwrap().kind, Kind::File { executable: false });
        assert_eq!(ws.stat("/two/x/y").unwrap().kind, Kind::Dir);
        assert!(matches!(ws.stat("/one/x/y/z"), Err(Error::NotADirectory(_) | Error::NotFound(_))));
    }
    let fork = ws.fork();
    assert_eq!(fork.read_text("/two/data/same.csv").unwrap(), "22");
    assert_eq!(fork.read_text("/two/x/y/z").unwrap(), "dir");
}

#[test]
fn a_deep_path_lists_each_directory_once() {
    let deep: String = (0..40).map(|level| format!("level{level}/")).collect();
    let source = Counting::new(MemorySource::from_files([(format!("{deep}leaf.bin"), "leaf")]).unwrap());
    let mut ws = Workspace::new();
    ws.mount("/deep", source.clone()).unwrap();
    let leaf = format!("/deep/{deep}leaf.bin");
    for _ in 0..3 {
        assert_eq!(ws.stat(&leaf).unwrap().size, 4);
    }
    assert_eq!(source.reads(), (41, 0));
    ws.write(&format!("/deep/{deep}new.bin"), "new").unwrap();
    assert_eq!(ws.read_text(&leaf).unwrap(), "leaf");
    assert_eq!(source.reads(), (41, 1));
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
fn changes_are_reported_per_file() {
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
fn changes_include_a_directory_replaced_by_a_file() {
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

#[cfg(unix)]
#[test]
fn dir_source_refuses_listings_through_a_swapped_in_symlink() {
    let base = std::env::temp_dir().join(format!("sandfs-escape-list-{}", std::process::id()));
    let (root, outside) = (base.join("root"), base.join("outside"));
    std::fs::create_dir_all(root.join("a/b/c")).unwrap();
    std::fs::create_dir_all(outside.join("c")).unwrap();
    let source = DirSource::new(&root).unwrap();
    assert!(source.read_dir("a/b").unwrap().contains_key("c"));

    // A folder anywhere on the way swapped for a link out of the root.
    std::fs::remove_dir_all(root.join("a/b")).unwrap();
    std::os::unix::fs::symlink(&outside, root.join("a/b")).unwrap();
    assert!(source.read_dir("a/b").is_err());
    assert!(source.read_dir("a/b/c").is_err());
    // A fifo in a folder's place fails at once instead of waiting for a writer.
    std::fs::remove_file(root.join("a/b")).unwrap();
    assert!(std::process::Command::new("mkfifo").arg(root.join("a/b")).status().unwrap().success());
    assert!(source.read_dir("a/b").is_err());
    // On Linux, where the kernel names the folder it opened, a link to a folder inside
    // the root is refused too: the key names another path. Elsewhere the resolved path
    // is only checked to be inside the root.
    #[cfg(target_os = "linux")]
    {
        std::fs::remove_file(root.join("a/b")).unwrap();
        std::os::unix::fs::symlink(root.join("a"), root.join("a/b")).unwrap();
        assert!(source.read_dir("a/b").is_err());
    }
    std::fs::remove_dir_all(&base).unwrap();
}

#[test]
fn dir_source_never_reads_keys_that_leave_its_folder() {
    let base = std::env::temp_dir().join(format!("sandfs-keys-{}", std::process::id()));
    let (root, outside) = (base.join("root"), base.join("outside"));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("file"), "SECRET").unwrap();

    let source = DirSource::new(&root).unwrap();
    let absolute = outside.join("file").canonicalize().unwrap();
    for key in [absolute.to_str().unwrap(), "../outside/file"] {
        assert!(source.read_file(key).is_err(), "{key} was read");
        assert!(source.read_range(key, 0, 3).is_err(), "{key} was read in a range");
    }
    std::fs::remove_dir_all(&base).unwrap();
}

#[test]
fn stat_finds_sizes_a_source_did_not_report() {
    let mut ws = Workspace::new();
    ws.mount("/api", Arc::new(Unsized(repo()))).unwrap();
    assert_eq!(ws.list("/api").unwrap()[0].size, None);
    assert_eq!(ws.stat("/api/README.md").unwrap().size, 6);
    assert_eq!(ws.stat("/api/src").unwrap().size, 0);
}

#[test]
fn binary_files_round_trip_untouched() {
    let (mut ws, _) = workspace();
    let png = Bytes::from_static(b"\x89PNG\r\n\x1a\n\x00\xff");
    ws.write("/out/chart.png", png.clone()).unwrap();
    assert_eq!(ws.read("/out/chart.png").unwrap(), png);
    assert!(matches!(ws.read_text("/out/chart.png"), Err(Error::NotText(_))));
    assert_eq!(changes(&ws), [("/out/chart.png".into(), ChangeKind::Added)]);
}

#[test]
fn executable_bits_survive_copies() {
    let mut source = MemorySource::default();
    source.insert("bin/run", Bytes::from_static(b"#!/bin/sh\n"), true).unwrap();
    let mut ws = Workspace::new();
    ws.mount("/m", Arc::new(source)).unwrap();
    ws.copy("/m/bin/run", "/m/bin/run2", false).unwrap();
    assert_eq!(ws.stat("/m/bin/run2").unwrap().kind, Kind::File { executable: true });
}

#[test]
fn symlinks_are_stored_and_never_followed() {
    let (mut ws, _) = workspace();
    let version = ws.symlink("/repo/.venv/lib64", "lib").unwrap();
    assert!(version > 0);
    assert_eq!(ws.read_link("/repo/.venv/lib64").unwrap(), "lib");
    let meta = ws.stat("/repo/.venv/lib64").unwrap();
    assert_eq!((meta.kind, meta.size, meta.version), (Kind::Symlink, 3, version));
    assert_eq!(ws.list("/repo/.venv").unwrap()[0].kind, Kind::Symlink);

    // The link itself is the entry: reads, writes and modes refuse it rather than
    // follow it, and the target never has to exist.
    ws.symlink("/repo/readme", "README.md").unwrap();
    assert!(matches!(ws.read("/repo/readme"), Err(Error::Unsupported(..))));
    assert!(matches!(ws.read_range("/repo/readme", 0, 2), Err(Error::Unsupported(..))));
    assert!(matches!(ws.write("/repo/readme", "x"), Err(Error::Unsupported(..))));
    assert!(matches!(ws.set_executable("/repo/readme", true), Err(Error::Unsupported(..))));
    assert!(matches!(ws.read_link("/repo/README.md"), Err(Error::Unsupported(..))));
    assert!(matches!(ws.symlink("/repo/README.md", "x"), Err(Error::AlreadyExists(_))));
    assert!(matches!(ws.symlink("/repo/empty", ""), Err(Error::InvalidPath(_))));
    assert!(matches!(ws.read("/repo/readme/x"), Err(Error::NotADirectory(_))));

    // Copies and moves take the link, not what it points to.
    ws.copy("/repo/readme", "/repo/readme2", false).unwrap();
    ws.rename("/repo/readme", "/repo/docs/readme").unwrap();
    assert_eq!(ws.read_link("/repo/readme2").unwrap(), "README.md");
    assert_eq!(ws.read_link("/repo/docs/readme").unwrap(), "README.md");

    let attempt = ws.fork();
    ws.remove("/repo/readme2", false).unwrap();
    assert_eq!(attempt.read_link("/repo/readme2").unwrap(), "README.md");
    assert_eq!(
        changes(&ws),
        [("/repo/.venv/lib64".into(), ChangeKind::Added), ("/repo/docs/readme".into(), ChangeKind::Added)]
    );
}

#[test]
fn a_link_replacing_a_base_file_is_a_modification() {
    let (mut ws, _) = workspace();
    ws.remove("/repo/README.md", false).unwrap();
    ws.symlink("/repo/README.md", "docs/guide.md").unwrap();
    assert_eq!(changes(&ws), [("/repo/README.md".into(), ChangeKind::Modified)]);
}

#[cfg(unix)]
#[test]
fn dir_source_lists_symlinks_without_following_them() {
    let base = std::env::temp_dir().join(format!("sandfs-links-{}", std::process::id()));
    let (root, outside) = (base.join("root"), base.join("outside"));
    std::fs::create_dir_all(root.join("lib")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(root.join("data.csv"), "a,b\n").unwrap();
    std::fs::write(outside.join("secret"), "SECRET").unwrap();
    std::os::unix::fs::symlink(outside.join("secret"), root.join("secret")).unwrap();
    std::os::unix::fs::symlink("../outside", root.join("away")).unwrap();
    std::os::unix::fs::symlink("lib", root.join("lib64")).unwrap();
    // Absolute links into the folder, which a mount must not lead back to.
    std::os::unix::fs::symlink(root.join("data.csv"), root.join("lib/data.csv")).unwrap();
    std::os::unix::fs::symlink(root.join("lib/../lib/x"), root.join("lib/x-abs")).unwrap();
    std::os::unix::fs::symlink(&root, root.join("lib/top")).unwrap();
    std::os::unix::fs::symlink(base.join("root/lib"), root.join("libs")).unwrap();
    std::os::unix::fs::symlink(root.join("data.csv"), outside.join("alias")).unwrap();
    std::os::unix::fs::symlink(outside.join("alias"), root.join("lib/alias")).unwrap();
    std::os::unix::fs::symlink(root.join("lib"), outside.join("lib-alias")).unwrap();
    std::os::unix::fs::symlink(outside.join("lib-alias/../new.csv"), root.join("lib/new")).unwrap();
    // Relative links that leave the folder and come back, which a mount elsewhere must
    // not resolve back into it either.
    std::os::unix::fs::symlink("../root/data.csv", root.join("again")).unwrap();
    std::os::unix::fs::symlink("../../root/lib", root.join("lib/again")).unwrap();
    // `..` after a link out of the folder ends outside it, however it is spelled.
    std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
    std::os::unix::fs::symlink(root.join("escape/../data.csv"), root.join("lib/escaped")).unwrap();
    // A missing folder before `..` leaves a link dangling, so it must not reach anything.
    std::os::unix::fs::symlink("../../missing/../outside/secret", root.join("lib/dangling")).unwrap();
    std::os::unix::fs::symlink("missing/../../data.csv", root.join("lib/dangling-in")).unwrap();
    // A trailing `/` asks for a folder, so a link to a file that way dangles too.
    std::os::unix::fs::symlink("../../outside/secret/", root.join("lib/not-a-folder")).unwrap();
    std::os::unix::fs::symlink("../data.csv/.", root.join("lib/not-a-folder-in")).unwrap();
    std::os::unix::fs::symlink(".././lib/", root.join("lib/self")).unwrap();

    let mut ws = Workspace::new();
    ws.mount("/host", Arc::new(DirSource::new(&root).unwrap())).unwrap();
    assert_eq!(names(&ws, "/host"), ["again", "away", "data.csv", "escape", "lib", "lib64", "libs", "secret"]);
    assert_eq!(ws.stat("/host/lib64").unwrap().kind, Kind::Symlink);
    assert_eq!(ws.read_link("/host/lib64").unwrap(), "lib");
    let away = outside.canonicalize().unwrap();
    assert_eq!(ws.read_link("/host/away").unwrap(), away.to_str().unwrap(), "where it leads on the host");
    assert_eq!(ws.read_link("/host/again").unwrap(), "data.csv");
    assert_eq!(ws.read_link("/host/lib/again").unwrap(), ".");
    let escaped = root.join("escape/../data.csv");
    assert_eq!(ws.read_link("/host/lib/escaped").unwrap(), escaped.to_str().unwrap());
    let dangling = base.canonicalize().unwrap().join("missing/../outside/secret");
    assert_eq!(ws.read_link("/host/lib/dangling").unwrap(), dangling.to_str().unwrap());
    let dangling_in = root.canonicalize().unwrap().join("lib/missing/../../data.csv");
    assert_eq!(ws.read_link("/host/lib/dangling-in").unwrap(), dangling_in.to_str().unwrap(), "anchored to the host");
    let not_a_folder = outside.canonicalize().unwrap().join("secret/");
    assert_eq!(ws.read_link("/host/lib/not-a-folder").unwrap(), not_a_folder.to_str().unwrap());
    assert_eq!(ws.read_link("/host/lib/not-a-folder-in").unwrap(), "../data.csv/");
    assert_eq!(ws.read_link("/host/lib/self").unwrap(), "./");
    assert_eq!(ws.read_link("/host/lib/data.csv").unwrap(), "../data.csv");
    assert_eq!(ws.read_link("/host/lib/x-abs").unwrap(), "x", "a target that doesn't exist yet");
    assert_eq!(ws.read_link("/host/lib/top").unwrap(), "..");
    assert_eq!(ws.read_link("/host/libs").unwrap(), "lib");
    assert_eq!(ws.read_link("/host/lib/alias").unwrap(), "../data.csv", "through a link outside the folder");
    assert_eq!(ws.read_link("/host/lib/new").unwrap(), "../new.csv", "`..` after a link, as the OS takes it");
    assert_eq!(ws.stat("/host/lib/data.csv").unwrap().size, 11, "listed with its new length");
    assert!(changes(&ws).is_empty());

    // Nothing reads through a link out of the folder: the target is all there is.
    let target = outside.join("secret");
    assert_eq!(ws.read_link("/host/secret").unwrap(), target.to_str().unwrap());
    assert_eq!(ws.stat("/host/secret").unwrap().size, target.to_str().unwrap().len() as u64);
    assert!(matches!(ws.read("/host/secret"), Err(Error::Unsupported(..))));
    assert!(matches!(ws.read_range("/host/secret", 0, 6), Err(Error::Unsupported(..))));
    assert!(matches!(ws.list("/host/away"), Err(Error::NotADirectory(_))));
    assert!(ws.read("/host/away/secret").is_err());
    let source = DirSource::new(&root).unwrap();
    assert_eq!(source.read_file("secret").unwrap(), target.to_str().unwrap().as_bytes());
    assert_eq!(source.read_range("secret", 0, 3).unwrap(), &target.to_str().unwrap().as_bytes()[..3]);
    assert!(source.read_file("away/secret").is_err());
    assert!(source.read_dir("away").is_err());

    // A copy of a link is a link, and changes report it like a file.
    ws.copy("/host/lib64", "/host/lib32", false).unwrap();
    assert_eq!(ws.read_link("/host/lib32").unwrap(), "lib");
    assert_eq!(changes(&ws), [("/host/lib32".into(), ChangeKind::Added)]);
    std::fs::remove_dir_all(&base).unwrap();
}

#[test]
fn executable_bits_change_through_the_overlay() {
    let (mut ws, source) = workspace();
    let version = ws.set_executable("/repo/README.md", true).unwrap();
    assert!(version > 0);
    assert_eq!(ws.stat("/repo/README.md").unwrap().kind, Kind::File { executable: true });
    assert_eq!(source.reads().1, 0, "changing the mode reads nothing");
    assert_eq!(changes(&ws), [("/repo/README.md".to_owned(), ChangeKind::Modified)]);

    assert_eq!(ws.set_executable("/repo/README.md", true).unwrap(), version, "no change, same version");
    ws.set_executable("/repo/README.md", false).unwrap();
    assert!(changes(&ws).is_empty(), "back to the base mode");
    assert!(matches!(ws.set_executable("/repo/src", true), Err(Error::IsADirectory(_))));
}

/// A source that can only be read in ranges, counting each ranged read.
#[derive(Debug)]
struct Ranged {
    inner: MemorySource,
    ranges: AtomicUsize,
}

impl Source for Ranged {
    fn root(&self) -> sandfs_core::Result<SourceKey> {
        self.inner.root()
    }

    fn read_dir(&self, key: &str) -> sandfs_core::Result<Listing> {
        self.inner.read_dir(key)
    }

    fn read_file(&self, key: &str) -> sandfs_core::Result<Bytes> {
        self.inner.read_file(key)
    }

    fn read_range(&self, key: &str, offset: u64, len: u64) -> sandfs_core::Result<Bytes> {
        self.ranges.fetch_add(1, Ordering::SeqCst);
        self.inner.read_range(key, offset, len)
    }
}

#[test]
fn ranged_reads_clamp_to_the_end_of_the_file() {
    let (mut ws, _) = workspace();
    ws.write("/notes.txt", "0123456789").unwrap();
    assert_eq!(ws.read_range("/notes.txt", 3, 4).unwrap(), "3456");
    assert_eq!(ws.read_range("/notes.txt", 8, 10).unwrap(), "89");
    assert_eq!(ws.read_range("/notes.txt", 20, 1).unwrap(), "");
    assert_eq!(ws.read_range("/repo/README.md", 0, 4).unwrap(), "hell");
    assert!(matches!(ws.read_range("/repo/src", 0, 1), Err(Error::IsADirectory(_))));
}

#[test]
fn ranged_reads_go_to_the_source_until_the_file_is_read_whole() {
    let source = Arc::new(Ranged { inner: repo(), ranges: AtomicUsize::new(0) });
    let mut ws = Workspace::new();
    ws.mount("/data", source.clone()).unwrap();

    assert_eq!(ws.read_range("/data/README.md", 1, 3).unwrap(), "ell");
    assert_eq!(ws.read_range("/data/README.md", 1, 3).unwrap(), "ell");
    assert_eq!(source.ranges.load(Ordering::SeqCst), 2, "partial reads are not cached");

    ws.read("/data/README.md").unwrap();
    assert_eq!(ws.read_range("/data/README.md", 0, 2).unwrap(), "he");
    assert_eq!(source.ranges.load(Ordering::SeqCst), 2, "a whole read fills the cache");
}

#[test]
fn dir_source_reads_ranges_from_disk() {
    let dir = std::env::temp_dir().join(format!("sandfs-range-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("big.bin"), (0..=255u8).collect::<Vec<_>>()).unwrap();

    let mut ws = Workspace::new();
    ws.mount("/host", Arc::new(DirSource::new(&dir).unwrap())).unwrap();
    assert_eq!(ws.read_range("/host/big.bin", 250, 100).unwrap(), Bytes::from_static(&[250, 251, 252, 253, 254, 255]));
    assert_eq!(ws.read_range("/host/big.bin", 1000, 10).unwrap(), Bytes::new());
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A slow source that counts file reads and fails the first one when asked to.
#[derive(Debug)]
struct Slow {
    inner: MemorySource,
    reads: AtomicUsize,
    fail_first: bool,
}

impl Source for Slow {
    fn root(&self) -> sandfs_core::Result<SourceKey> {
        self.inner.root()
    }

    fn read_dir(&self, key: &str) -> sandfs_core::Result<Listing> {
        self.inner.read_dir(key)
    }

    fn read_file(&self, key: &str) -> sandfs_core::Result<Bytes> {
        let attempt = self.reads.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(std::time::Duration::from_millis(50));
        if self.fail_first && attempt == 0 {
            return Err(Error::Source("network blip".into()));
        }
        self.inner.read_file(key)
    }
}

#[test]
fn concurrent_reads_of_one_file_fetch_it_once() {
    let source = Arc::new(Slow { inner: repo(), reads: AtomicUsize::new(0), fail_first: false });
    let mut ws = Workspace::new();
    ws.mount("/data", source.clone()).unwrap();
    ws.list("/data").unwrap();

    let forks: Vec<Workspace> = (0..8).map(|_| ws.fork()).collect();
    std::thread::scope(|scope| {
        for fork in &forks {
            scope.spawn(move || assert_eq!(fork.read_text("/data/README.md").unwrap(), "hello\n"));
        }
    });
    assert_eq!(source.reads.load(Ordering::SeqCst), 1);
}

#[test]
fn failed_fetches_are_retried_not_cached() {
    let source = Arc::new(Slow { inner: repo(), reads: AtomicUsize::new(0), fail_first: true });
    let mut ws = Workspace::new();
    ws.mount("/data", source.clone()).unwrap();
    assert!(matches!(ws.read("/data/README.md"), Err(Error::Source(_))));
    assert_eq!(ws.read_text("/data/README.md").unwrap(), "hello\n");
    assert_eq!(source.reads.load(Ordering::SeqCst), 2);
}

/// A store of saved files: a source that is never mounted, as a disk store would be.
fn store(files: &[(&str, &str)]) -> Arc<Counting> {
    Counting::new(MemorySource::from_files(files.iter().map(|(key, data)| (*key, data.to_string()))).unwrap())
}

fn stored(source: &Arc<Counting>, key: &str, size: u64) -> Stored {
    Stored { source: source.clone(), key: key.into(), size }
}

#[test]
fn stored_files_are_read_from_their_store_every_time() {
    let saved = store(&[("1", "saved report"), ("2", "notes")]);
    let mut ws = Workspace::new();
    ws.mount("/data", Arc::new(repo())).unwrap();
    let version = ws.write_stored("/data/report.txt", stored(&saved, "1", 12)).unwrap();
    assert_eq!(saved.reads(), (0, 0), "writing a stored file reads nothing");
    let meta = ws.stat("/data/report.txt").unwrap();
    assert_eq!((meta.size, meta.version), (12, version), "its size comes from the store's record");
    assert_eq!(ws.list("/data").unwrap().iter().find(|e| e.name == "report.txt").unwrap().size, Some(12));

    assert_eq!(ws.read_text("/data/report.txt").unwrap(), "saved report");
    assert_eq!(ws.read_text("/data/report.txt").unwrap(), "saved report");
    assert_eq!(saved.reads(), (0, 2), "nothing is kept in memory, so each read goes to the store");
    assert_eq!(ws.read_range("/data/report.txt", 6, 3).unwrap(), "rep");
    assert_eq!(
        ws.changes().unwrap(),
        [Change { path: VPath::parse("/data/report.txt").unwrap(), kind: ChangeKind::Added }]
    );

    // Copies and moves keep reading from the store, and say where.
    ws.rename("/data/report.txt", "/data/final.txt").unwrap();
    ws.copy("/data/final.txt", "/data/copy.txt", false).unwrap();
    for path in ["/data/final.txt", "/data/copy.txt"] {
        assert_eq!(&*ws.stored(path).unwrap().unwrap().key, "1");
        assert_eq!(ws.read_text(path).unwrap(), "saved report");
    }
    assert!(ws.stored("/data/README.md").unwrap().is_none(), "a base file isn't stored");
    ws.write("/data/final.txt", "rewritten").unwrap();
    assert!(ws.stored("/data/final.txt").unwrap().is_none(), "a new write lives in memory again");
}

#[test]
fn storing_a_file_keeps_its_version_unless_it_changed_since() {
    let saved = store(&[("1", "draft one"), ("2", "draft two")]);
    let mut ws = Workspace::new();
    ws.mount("/data", Arc::new(repo())).unwrap();
    let version = ws.write("/data/draft.txt", "draft one").unwrap();
    let fork = ws.fork();
    assert!(ws.store("/data/draft.txt", version, stored(&saved, "1", 9)).unwrap());
    let meta = ws.stat("/data/draft.txt").unwrap();
    assert_eq!((meta.version, meta.size), (version, 9), "same bytes, same version");
    assert_eq!(ws.read_text("/data/draft.txt").unwrap(), "draft one");
    assert_eq!(saved.reads(), (0, 1));
    assert!(fork.stored("/data/draft.txt").unwrap().is_none(), "forks taken before keep their own copy");

    // A file written again since the save has bytes the store doesn't hold.
    let newer = ws.write("/data/draft.txt", "draft two, longer").unwrap();
    assert!(!ws.store("/data/draft.txt", version, stored(&saved, "2", 9)).unwrap());
    assert_eq!(ws.read_text("/data/draft.txt").unwrap(), "draft two, longer");
    assert_eq!(ws.stat("/data/draft.txt").unwrap().version, newer);
    // Base files and missing files are never stored.
    assert!(!ws.store("/data/README.md", 0, stored(&saved, "2", 9)).unwrap());
    assert!(!ws.store("/data/missing.txt", 0, stored(&saved, "2", 9)).unwrap());
    assert!(!ws.exists("/data/missing.txt"));

    // Keeping the executable bit, which is part of the file, not its bytes.
    let version = ws.write("/data/run.sh", "draft two").unwrap();
    let version = ws.set_executable("/data/run.sh", true).unwrap().max(version);
    assert!(ws.store("/data/run.sh", version, stored(&saved, "2", 9)).unwrap());
    assert_eq!(ws.stat("/data/run.sh").unwrap().kind, Kind::File { executable: true });
}
