//! The `sandfs` binary against a real FUSE mount.
//!
//! Skipped where FUSE is unavailable (no `/dev/fuse` or no `fusermount3`), unless
//! `SANDFS_REQUIRE_FUSE` is set, as CI does, in which case that is a failure.
#![cfg(fuse)]

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
    // The crash released the workspace too, so another process may take it over.
    sandfs.run(&["serve", "--mcp", "data"]);
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

#[test]
fn open_files_work_without_open_or_close_reaching_sandfs() {
    use std::io::{Read, Seek, SeekFrom, Write};
    if !fuse_available() {
        return;
    }
    let folder = tempfile::tempdir().unwrap();
    std::fs::write(folder.path().join("log.txt"), "base\n").unwrap();
    std::fs::write(folder.path().join("old.txt"), "old\n").unwrap();
    let mut sandfs = Sandfs::new();
    let dirs = tempfile::tempdir().unwrap();
    let ws = dirs.path().join("ws");
    sandfs.mount(&["mount", folder.path().to_str().unwrap(), ws.to_str().unwrap()], &ws);

    // A file written slowly through one handle is committed between writes, and the
    // writes after a commit carry on from it.
    let mut log = std::fs::OpenOptions::new().append(true).open(ws.join("log.txt")).unwrap();
    for line in ["one\n", "two\n", "three\n"] {
        log.write_all(line.as_bytes()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(400));
    }
    drop(log);
    assert_eq!(std::fs::read_to_string(ws.join("log.txt")).unwrap(), "base\none\ntwo\nthree\n");

    // A file removed while open still reads and writes through its handle.
    let mut old = std::fs::OpenOptions::new().read(true).write(true).open(ws.join("old.txt")).unwrap();
    std::fs::remove_file(ws.join("old.txt")).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(400));
    let mut text = String::new();
    old.read_to_string(&mut text).unwrap();
    assert_eq!(text, "old\n");
    old.write_all(b"more\n").unwrap();
    old.seek(SeekFrom::Start(0)).unwrap();
    text.clear();
    old.read_to_string(&mut text).unwrap();
    assert_eq!(text, "old\nmore\n");
    drop(old);
    assert!(!ws.join("old.txt").exists());

    // Opening with O_TRUNC empties the file, and the kernel drops its pages.
    std::fs::write(ws.join("log.txt"), "new\n").unwrap();
    assert_eq!(std::fs::read_to_string(ws.join("log.txt")).unwrap(), "new\n");
    assert_eq!(sandfs.stdout(&["changes", "ws"]), "M log.txt\nD old.txt\n");
    sandfs.unmount(&ws);
    assert_eq!(std::fs::read_to_string(folder.path().join("log.txt")).unwrap(), "base\n", "the folder is untouched");
}

#[test]
fn names_and_sizes_stay_fresh_within_the_cache_ttl() {
    if !fuse_available() {
        return;
    }
    let folder = tempfile::tempdir().unwrap();
    std::fs::create_dir(folder.path().join("in")).unwrap();
    std::fs::write(folder.path().join("in/a.txt"), "first\n").unwrap();
    let mut sandfs = Sandfs::new();
    let dirs = tempfile::tempdir().unwrap();
    let ws = dirs.path().join("ws");
    sandfs.mount(&["mount", folder.path().to_str().unwrap(), ws.to_str().unwrap()], &ws);

    // The kernel keeps names and attributes for a minute, so every change below is
    // checked right away through the cached entries the reads before it created.
    let a = ws.join("in/a.txt");
    assert_eq!(std::fs::metadata(&a).unwrap().len(), 6);
    assert_eq!(std::fs::read_to_string(&a).unwrap(), "first\n");

    std::fs::write(&a, "a longer line\n").unwrap();
    assert_eq!(std::fs::metadata(&a).unwrap().len(), 14, "size after a write");
    std::fs::OpenOptions::new().write(true).truncate(true).open(&a).unwrap();
    assert_eq!(std::fs::metadata(&a).unwrap().len(), 0, "size after truncating on open");
    assert_eq!(std::fs::read_to_string(&a).unwrap(), "");
    std::fs::File::options().write(true).open(&a).unwrap().set_len(3).unwrap();
    assert_eq!(std::fs::read(&a).unwrap(), [0, 0, 0], "contents after extending");

    std::fs::remove_file(&a).unwrap();
    assert!(!a.exists(), "a deleted name is gone");
    std::fs::write(&a, "again\n").unwrap();
    assert_eq!(std::fs::read_to_string(&a).unwrap(), "again\n", "a recreated name has the new file");

    std::fs::rename(ws.join("in"), ws.join("out")).unwrap();
    assert!(!a.exists(), "the old path of a moved folder is gone");
    assert_eq!(std::fs::read_to_string(ws.join("out/a.txt")).unwrap(), "again\n");
    let mut names: Vec<_> =
        std::fs::read_dir(&ws).unwrap().map(|entry| entry.unwrap().file_name().into_string().unwrap()).collect();
    names.sort();
    assert_eq!(names, ["out"], "listings follow the move");
    sandfs.unmount(&ws);
}

/// Start `sandfs args...` without waiting for it.
fn spawn(sandfs: &Sandfs, args: &[&str]) -> std::process::Child {
    Command::new(env!("CARGO_BIN_EXE_sandfs"))
        .env("SANDFS_HOME", sandfs.home.path())
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap()
}

/// How many filesystems are mounted at `dir`.
fn mounts_at(dir: &Path) -> usize {
    let table = std::fs::read_to_string("/proc/self/mountinfo").unwrap();
    table.lines().filter(|line| line.split(' ').nth(4) == Some(dir.to_str().unwrap())).count()
}

#[test]
fn one_process_serves_a_workspace_at_a_time() {
    if !fuse_available() {
        return;
    }
    let data = tempfile::tempdir().unwrap();
    std::fs::write(data.path().join("notes.txt"), "draft\n").unwrap();
    let mut sandfs = Sandfs::new();
    let dirs = tempfile::tempdir().unwrap();
    let (one, two) = (dirs.path().join("one"), dirs.path().join("two"));
    sandfs.mount(&["mount", data.path().to_str().unwrap(), one.to_str().unwrap(), "--name", "ws"], &one);
    sandfs.unmount(&one);

    // An MCP session holds the workspace until its client goes away.
    let mut agent = spawn(&sandfs, &["serve", "--mcp", "ws"]);
    let holder = sandfs.home.path().join("workspaces/ws/holder");
    while !std::fs::read_to_string(&holder).unwrap_or_default().contains("MCP") {
        assert!(agent.try_wait().unwrap().is_none(), "the session exited early");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    for args in [&["mount", "ws", one.to_str().unwrap()][..], &["serve", "--mcp", "ws"], &["rm", "ws"]] {
        let refused = sandfs.try_run(args);
        let stderr = String::from_utf8_lossy(&refused.stderr);
        assert!(!refused.status.success() && stderr.contains("ws is in use by an MCP session"), "{args:?}: {stderr}");
    }
    assert_eq!(mounts_at(&one.canonicalize().unwrap()), 0);
    drop(agent.stdin.take());
    assert!(agent.wait().unwrap().success());

    // Two mounts of one workspace started together: one serves it, the other is refused.
    let racers = [&one, &two].map(|dir| spawn(&sandfs, &["mount", "ws", dir.to_str().unwrap()]));
    let won: Vec<bool> = racers.into_iter().map(|racer| racer.wait_with_output().unwrap().status.success()).collect();
    assert_eq!(won.iter().filter(|w| **w).count(), 1, "{won:?}");
    let winner = if won[0] { &one } else { &two };
    sandfs.mounts.push(winner.clone());
    assert_eq!(std::fs::read_to_string(winner.join("notes.txt")).unwrap(), "draft\n");
    sandfs.unmount(winner);

    // Two workspaces mounted on one directory together: only one mount is made.
    sandfs.run(&["fork", "ws", two.to_str().unwrap(), "--name", "other"]);
    sandfs.mounts.push(two.clone());
    sandfs.unmount(&two);
    let racers = ["ws", "other"].map(|name| spawn(&sandfs, &["mount", name, one.to_str().unwrap()]));
    let won: Vec<bool> = racers.into_iter().map(|racer| racer.wait_with_output().unwrap().status.success()).collect();
    assert_eq!(won.iter().filter(|w| **w).count(), 1, "{won:?}");
    sandfs.mounts.push(one.clone());
    assert_eq!(mounts_at(&one.canonicalize().unwrap()), 1);
    sandfs.unmount(&one);
}

#[test]
fn a_failed_final_save_fails_unmount() {
    if !fuse_available() {
        return;
    }
    let data = tempfile::tempdir().unwrap();
    let mut sandfs = Sandfs::new();
    let dirs = tempfile::tempdir().unwrap();
    let ws = dirs.path().join("ws");
    sandfs.mount(&["mount", data.path().to_str().unwrap(), ws.to_str().unwrap()], &ws);
    std::fs::write(ws.join("saved.txt"), "saved\n").unwrap();
    assert_eq!(sandfs.stdout(&["changes", "ws"]), "A saved.txt\n");

    // A directory where the save writes its new index makes every save fail, as a full
    // disk would.
    let blocker = sandfs.home.path().join("workspaces/ws/changes.json.tmp");
    std::fs::create_dir(&blocker).unwrap();
    std::fs::write(ws.join("lost.txt"), "lost\n").unwrap();
    let failed = sandfs.try_run(&["unmount", ws.to_str().unwrap()]);
    let stderr = String::from_utf8_lossy(&failed.stderr);
    assert!(!failed.status.success() && stderr.contains("saving the last changes failed"), "{stderr}");
    assert!(stderr.contains("changes.json"), "the reason is reported: {stderr}");
    sandfs.mounts.clear();

    std::fs::remove_dir(&blocker).unwrap();
    sandfs.mount(&["mount", "ws", ws.to_str().unwrap()], &ws);
    assert_eq!(std::fs::read_to_string(ws.join("saved.txt")).unwrap(), "saved\n");
    assert!(!ws.join("lost.txt").exists());
    sandfs.unmount(&ws);
}
