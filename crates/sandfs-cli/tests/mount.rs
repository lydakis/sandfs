//! The `sandfs` binary against a real FUSE mount.
//!
//! Skipped where FUSE is unavailable (no `/dev/fuse` or no `fusermount3`), unless
//! `SANDFS_REQUIRE_FUSE` is set, as CI does, in which case that is a failure.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn fuse_available() -> bool {
    let tool = Command::new("fusermount3").arg("--version").output().is_ok_and(|out| out.status.success());
    let available = Path::new("/dev/fuse").exists() && tool;
    if !available {
        assert!(std::env::var_os("SANDFS_REQUIRE_FUSE").is_none(), "FUSE is required but unavailable");
        eprintln!("skipping: FUSE is not available here");
    }
    available
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=test", "-c", "user.email=test@example.com", "-c", "init.defaultBranch=main"])
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

/// Runs the binary with its own home, and unmounts whatever it mounted when dropped.
struct Sandfs {
    home: tempfile::TempDir,
    mounts: Vec<PathBuf>,
}

impl Sandfs {
    fn new() -> Self {
        Sandfs { home: tempfile::tempdir().unwrap(), mounts: Vec::new() }
    }

    fn try_run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_sandfs")).env("SANDFS_HOME", self.home.path()).args(args).output().unwrap()
    }

    fn run(&self, args: &[&str]) -> Output {
        let output = self.try_run(args);
        assert!(output.status.success(), "sandfs {args:?}: {}", String::from_utf8_lossy(&output.stderr));
        output
    }

    fn stdout(&self, args: &[&str]) -> String {
        String::from_utf8(self.run(args).stdout).unwrap()
    }

    fn mount(&mut self, args: &[&str], dir: &Path) {
        self.run(args);
        self.mounts.push(dir.to_path_buf());
    }

    fn unmount(&mut self, dir: &Path) {
        self.run(&["unmount", dir.to_str().unwrap()]);
        self.mounts.retain(|mount| mount != dir);
    }
}

impl Drop for Sandfs {
    fn drop(&mut self) {
        for dir in &self.mounts {
            let _ = Command::new("fusermount3").arg("-u").arg(dir).status();
        }
    }
}

#[test]
fn mount_fork_changes_and_remount() {
    if !fuse_available() {
        return;
    }
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q"]);
    std::fs::create_dir(repo.path().join("src")).unwrap();
    std::fs::write(repo.path().join("README.md"), "hello\n").unwrap();
    std::fs::write(repo.path().join("src/lib.rs"), "fn one() {}\n").unwrap();
    git(repo.path(), &["add", "-A"]);
    git(repo.path(), &["commit", "-q", "-m", "first"]);

    let mut sandfs = Sandfs::new();
    let dirs = tempfile::tempdir().unwrap();
    let main = dirs.path().join("main");
    let fix = dirs.path().join("fix");
    let source = format!("git:{}@main", repo.path().display());
    sandfs.mount(&["mount", &source, main.to_str().unwrap()], &main);

    // Normal file operations on the mount.
    assert_eq!(std::fs::read_to_string(main.join("README.md")).unwrap(), "hello\n");
    assert_eq!(std::fs::metadata(main.join("src/lib.rs")).unwrap().len(), 12);
    std::fs::write(main.join("README.md"), "hello, sandfs\n").unwrap();
    std::fs::write(main.join("notes.txt"), "notes\n").unwrap();
    std::fs::create_dir(main.join("empty")).unwrap();
    std::fs::remove_file(main.join("src/lib.rs")).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(main.join("run.sh"), "#!/bin/sh\necho ran\n").unwrap();
        std::fs::set_permissions(main.join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let out = Command::new(main.join("run.sh")).output().unwrap();
        assert_eq!(out.stdout, b"ran\n");
    }
    assert_eq!(sandfs.stdout(&["changes", "main"]), "M README.md\nA notes.txt\nA run.sh\nD src/lib.rs\n");
    assert_eq!(std::fs::read_to_string(repo.path().join("README.md")).unwrap(), "hello\n", "the repo is untouched");

    // A fork starts from the workspace's changes and then goes its own way.
    sandfs.mount(&["fork", main.to_str().unwrap(), fix.to_str().unwrap()], &fix);
    assert_eq!(std::fs::read_to_string(fix.join("notes.txt")).unwrap(), "notes\n");
    std::fs::write(fix.join("fix.txt"), "only in the fork\n").unwrap();
    assert!(!main.join("fix.txt").exists());
    assert!(sandfs.stdout(&["changes", fix.to_str().unwrap()]).contains("A fix.txt\n"));

    // Changes survive unmounting and mounting again.
    sandfs.unmount(&main);
    assert!(sandfs.stdout(&["list"]).contains("main\t"));
    sandfs.mount(&["mount", "main", main.to_str().unwrap()], &main);
    assert_eq!(std::fs::read_to_string(main.join("README.md")).unwrap(), "hello, sandfs\n");
    assert!(main.join("empty").is_dir(), "an empty directory is kept");
    assert!(!main.join("src/lib.rs").exists());
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(main.join("run.sh")).unwrap().permissions().mode() & 0o777, 0o755);
    }

    sandfs.unmount(&main);
    sandfs.unmount(&fix);
    sandfs.run(&["rm", "fix"]);
    assert!(!sandfs.stdout(&["list"]).contains("fix\t"));
}

#[test]
fn a_directory_source_is_never_written() {
    if !fuse_available() {
        return;
    }
    let data = tempfile::tempdir().unwrap();
    std::fs::create_dir(data.path().join("raw")).unwrap();
    std::fs::write(data.path().join("raw/rows.csv"), "id,value\n1,10\n").unwrap();

    let mut sandfs = Sandfs::new();
    let inside = data.path().join("ws");
    let refused = sandfs.try_run(&["mount", data.path().to_str().unwrap(), inside.to_str().unwrap()]);
    assert!(!refused.status.success() && !inside.exists(), "a mount inside its own source is refused");
    let dirs = tempfile::tempdir().unwrap();
    let ws = dirs.path().join("data");
    let source = data.path().to_str().unwrap();
    sandfs.mount(&["mount", source, ws.to_str().unwrap()], &ws);
    assert!(!sandfs.try_run(&["serve", "--mcp", "data"]).status.success(), "one writer at a time");

    let mut rows = std::fs::read_to_string(ws.join("raw/rows.csv")).unwrap();
    rows.push_str("2,20\n");
    std::fs::write(ws.join("raw/rows.csv"), &rows).unwrap();
    std::fs::create_dir(ws.join("out")).unwrap();
    std::fs::write(ws.join("out/summary.txt"), "2 rows\n").unwrap();
    std::fs::remove_dir_all(ws.join("raw")).unwrap();

    assert_eq!(sandfs.stdout(&["changes", "data"]), "A out/summary.txt\nD raw/rows.csv\n");
    assert_eq!(std::fs::read_to_string(data.path().join("raw/rows.csv")).unwrap(), "id,value\n1,10\n");
    assert!(!data.path().join("out").exists());

    // A fork whose mount fails is removed, so the same command can be retried.
    let blocked = dirs.path().join("file");
    std::fs::write(&blocked, "").unwrap();
    let failed = sandfs.try_run(&["fork", "data", blocked.join("copy").to_str().unwrap()]);
    assert!(!failed.status.success());
    assert!(!sandfs.stdout(&["list"]).contains("copy"), "the failed fork is gone");

    // A crashed mount process leaves a disconnected mount that `unmount DIR` detaches.
    let mount_json = sandfs.home.path().join("workspaces/data/mount.json");
    let info: serde_json::Value = serde_json::from_slice(&std::fs::read(&mount_json).unwrap()).unwrap();
    let pid = info["pid"].as_u64().unwrap().to_string();
    assert!(Command::new("kill").args(["-9", &pid]).status().unwrap().success());
    while Path::new(&format!("/proc/{pid}")).exists() {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    sandfs.unmount(&ws);
    sandfs.mount(&["mount", "data", ws.to_str().unwrap()], &ws);
    assert_eq!(std::fs::read_to_string(ws.join("out/summary.txt")).unwrap(), "2 rows\n", "saved before the crash");
    sandfs.unmount(&ws);
}

#[test]
fn rereading_a_changed_file_never_returns_cached_bytes() {
    if !fuse_available() {
        return;
    }
    let folder = tempfile::tempdir().unwrap();
    std::fs::write(folder.path().join("data.csv"), "one\n").unwrap();
    let mut sandfs = Sandfs::new();
    let dirs = tempfile::tempdir().unwrap();
    let ws = dirs.path().join("ws");
    sandfs.mount(&["mount", folder.path().to_str().unwrap(), ws.to_str().unwrap()], &ws);

    let file = ws.join("data.csv");
    // Read twice so the second open may keep the kernel's cached pages, then change the
    // file in ways that keep its size, which a stale cache would not notice.
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "one\n");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "one\n");
    std::fs::write(&file, "two\n").unwrap();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "two\n");
    std::fs::write(ws.join("other.csv"), "six\n").unwrap();
    assert_eq!(std::fs::read_to_string(ws.join("other.csv")).unwrap(), "six\n");
    std::fs::rename(ws.join("other.csv"), &file).unwrap();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "six\n");
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut handle = std::fs::OpenOptions::new().write(true).open(&file).unwrap();
        handle.seek(SeekFrom::Start(1)).unwrap();
        handle.write_all(b"ea").unwrap();
    }
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "sea\n");
    sandfs.unmount(&ws);
}
