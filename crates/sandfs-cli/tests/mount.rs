//! The `sandfs` binary against a real mount: FUSE on Linux, NFS on macOS.
//!
//! Skipped where mounting is unavailable (on Linux no `/dev/fuse` or no `fusermount3`,
//! on macOS no `mount_nfs`), unless `SANDFS_REQUIRE_FUSE` is set, as CI does, in which
//! case that is a failure.
#![cfg(mount)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn fuse_available() -> bool {
    let available = if cfg!(target_os = "macos") {
        Path::new("/sbin/mount_nfs").exists()
    } else {
        let tool = Command::new("fusermount3").arg("--version").output().is_ok_and(|out| out.status.success());
        Path::new("/dev/fuse").exists() && tool
    };
    if !available {
        assert!(std::env::var_os("SANDFS_REQUIRE_FUSE").is_none(), "mounting is required but unavailable");
        eprintln!("skipping: mounting is not available here");
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
            let _ = if cfg!(target_os = "macos") {
                Command::new("umount").arg("-f").arg(dir).status()
            } else {
                Command::new("fusermount3").arg("-u").arg(dir).status()
            };
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
        std::fs::write(main.join("plain.sh"), "#!/bin/sh\necho ran\n").unwrap();
        let denied = Command::new(main.join("plain.sh")).output().unwrap_err();
        assert_eq!(denied.kind(), std::io::ErrorKind::PermissionDenied, "only executables run");
        std::fs::remove_file(main.join("plain.sh")).unwrap();
    }
    assert_eq!(sandfs.stdout(&["changes", "main"]), "M README.md\nA notes.txt\nA run.sh\nD src/lib.rs\n");
    assert_eq!(std::fs::read_to_string(repo.path().join("README.md")).unwrap(), "hello\n", "the repo is untouched");

    // A fork starts from the workspace's changes, writes to a file still open among
    // them, and then goes its own way.
    let mut draft = std::fs::File::create(main.join("draft.txt")).unwrap();
    std::io::Write::write_all(&mut draft, b"draft\n").unwrap();
    sandfs.mount(&["fork", main.to_str().unwrap(), fix.to_str().unwrap()], &fix);
    drop(draft);
    assert_eq!(std::fs::read_to_string(fix.join("notes.txt")).unwrap(), "notes\n");
    assert_eq!(std::fs::read_to_string(fix.join("draft.txt")).unwrap(), "draft\n");
    std::fs::write(fix.join("fix.txt"), "only in the fork\n").unwrap();
    assert!(!main.join("fix.txt").exists());
    assert!(sandfs.stdout(&["changes", fix.to_str().unwrap()]).contains("A fix.txt\n"));
    assert_eq!(sandfs.stdout(&["changes", "fix", "--since", "main"]), "A fix.txt\n", "only the fork's own work");

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

/// The process serving `name`'s mount, from its mount.json.
fn server_pid(sandfs: &Sandfs, name: &str) -> String {
    let info = std::fs::read(sandfs.home.path().join("workspaces").join(name).join("mount.json")).unwrap();
    let info: serde_json::Value = serde_json::from_slice(&info).unwrap();
    info["pid"].as_u64().unwrap().to_string()
}

fn alive(pid: &str) -> bool {
    Command::new("kill").args(["-0", pid]).stderr(std::process::Stdio::null()).status().unwrap().success()
}

#[test]
fn one_process_serves_every_mount_until_the_last_is_gone() {
    if !fuse_available() {
        return;
    }
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q"]);
    std::fs::write(repo.path().join("notes.txt"), "draft\n").unwrap();
    git(repo.path(), &["add", "-A"]);
    git(repo.path(), &["commit", "-q", "-m", "first"]);
    let folder = tempfile::tempdir().unwrap();
    std::fs::write(folder.path().join("rows.csv"), "id\n").unwrap();

    let mut sandfs = Sandfs::new();
    let dirs = tempfile::tempdir().unwrap();
    let [one, two, rows] = ["one", "two", "rows"].map(|name| dirs.path().join(name));
    let source = format!("git:{}", repo.path().display());
    sandfs.mount(&["mount", &source, one.to_str().unwrap()], &one);
    sandfs.mount(&["fork", "one", two.to_str().unwrap()], &two);
    sandfs.mount(&["mount", folder.path().to_str().unwrap(), rows.to_str().unwrap()], &rows);
    let pid = server_pid(&sandfs, "one");
    assert_eq!(server_pid(&sandfs, "two"), pid);
    assert_eq!(server_pid(&sandfs, "rows"), pid);

    // Copies of one commit share its git readers (here, reading one file at a time, one
    // `cat-file` for contents and one for sizes) but not their changes.
    std::fs::write(two.join("notes.txt"), "changed\n").unwrap();
    assert_eq!(std::fs::read_to_string(one.join("notes.txt")).unwrap(), "draft\n");
    assert_eq!(std::fs::read_to_string(two.join("notes.txt")).unwrap(), "changed\n");
    let readers = Command::new("pgrep").args(["-P", &pid, "-f", "cat-file"]).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&readers.stdout).lines().count(), 2);

    // The process stays while any mount is left, and exits after the last one.
    sandfs.unmount(&one);
    sandfs.unmount(&rows);
    assert!(alive(&pid));
    assert_eq!(std::fs::read_to_string(two.join("notes.txt")).unwrap(), "changed\n");
    sandfs.unmount(&two);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while alive(&pid) {
        assert!(std::time::Instant::now() < deadline, "the process outlived its mounts");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(sandfs.stdout(&["changes", "two"]), "M notes.txt\n");
    // A mount after that starts a new process.
    sandfs.mount(&["mount", "one", one.to_str().unwrap()], &one);
    assert_ne!(server_pid(&sandfs, "one"), pid);
    sandfs.unmount(&one);
}

/// Runs git in `dir` and returns what it printed.
fn git_out(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=test", "-c", "user.email=test@example.com", "-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn git_works_inside_a_remote_git_mount() {
    if !fuse_available() {
        return;
    }
    // Remote local copies are named after the URL, which can't have parts starting with '.'.
    let remote = tempfile::Builder::new().prefix("sandfs-remote").tempdir().unwrap();
    let remote = remote.path();
    git(remote, &["init", "-q"]);
    git(remote, &["config", "uploadpack.allowFilter", "true"]);
    std::fs::create_dir_all(remote.join("src")).unwrap();
    std::fs::create_dir_all(remote.join("docs")).unwrap();
    std::fs::write(remote.join("README.md"), "hello\n").unwrap();
    std::fs::write(remote.join("src/lib.rs"), "fn one() {}\n").unwrap();
    std::fs::write(remote.join("docs/.gitignore"), "*.tmp\n").unwrap();
    std::fs::write(remote.join("LICENSE"), "free\n").unwrap();
    std::os::unix::fs::symlink("../README.md", remote.join("docs/readme")).unwrap();
    git(remote, &["add", "-A"]);
    git(remote, &["commit", "-q", "-m", "first"]);

    let mut sandfs = Sandfs::new();
    let dirs = tempfile::tempdir().unwrap();
    let [work, copy] = ["work", "copy"].map(|name| dirs.path().join(name));
    let source = format!("git:file://{}", remote.display());
    sandfs.mount(&["mount", &source, work.to_str().unwrap()], &work);

    // A checkout on the remote's default branch, read without fetching file contents.
    let alternates = std::fs::read_to_string(work.join(".git/objects/info/alternates")).unwrap();
    let cache = Path::new(alternates.lines().next().unwrap()).parent().unwrap().to_path_buf();
    let unread = || {
        let missing = git_out(&cache, &["rev-list", "--objects", "--missing=print", "--all"]);
        ["README.md", "src/lib.rs"].map(|path| {
            missing.contains(&format!("?{}", git_out(remote, &["rev-parse", &format!("HEAD:{path}")]).trim()))
        })
    };
    assert_eq!(unread(), [true, true]);
    assert_eq!(std::fs::read_link(work.join("docs/readme")).unwrap(), Path::new("../README.md"));
    assert_eq!(git_out(&work, &["status", "--porcelain", "-b"]), "## main...origin/main\n");
    // (A Mac mount lists folders over NFS with every file's size, which fetches them; #83.)
    if cfg!(target_os = "linux") {
        assert_eq!(unread(), [true, true], "status reads no file");
    }

    // Edits, a new file and an ignored one, committed and pushed.
    std::fs::write(work.join("README.md"), "hello, sandfs\n").unwrap();
    std::fs::write(work.join("notes.txt"), "notes\n").unwrap();
    std::fs::write(work.join("docs/scratch.tmp"), "ignored\n").unwrap();
    std::fs::remove_file(work.join("docs/readme")).unwrap();
    std::os::unix::fs::symlink("../LICENSE", work.join("docs/readme")).unwrap();
    assert_eq!(git_out(&work, &["status", "--porcelain"]), " M README.md\n M docs/readme\n?? notes.txt\n");
    git_out(&work, &["add", "-A"]);
    git_out(&work, &["commit", "-q", "-m", "change"]);
    // Committing compared the files of the folders it changed, and fetched no others,
    // into sandfs's local copy rather than the workspace.
    if cfg!(target_os = "linux") {
        assert_eq!(unread(), [false, true], "src/lib.rs is still unread");
    }
    let fetched_here = std::fs::read_dir(work.join(".git/objects/pack")).unwrap().count();
    assert_eq!(fetched_here, 0, "the workspace holds no fetched files");
    assert_eq!(git_out(&work, &["status", "--porcelain", "-b"]), "## main...origin/main [ahead 1]\n");
    assert_eq!(git_out(&work, &["log", "--format=%s"]), "change\nfirst\n");
    assert_eq!(
        sandfs.stdout(&["changes", "work"]),
        "M README.md\nM docs/readme\nA docs/scratch.tmp\nA notes.txt\n",
        "not .git"
    );
    git_out(&work, &["push", "-q", "origin", "HEAD:refs/heads/from-mount"]);
    assert_eq!(git_out(remote, &["show", "from-mount:README.md"]), "hello, sandfs\n");
    assert_eq!(git_out(remote, &["show", "from-mount:docs/readme"]), "../LICENSE");
    assert_eq!(git_out(remote, &["log", "--format=%s", "main"]), "first\n", "only the push reached the remote");

    // A file changed, staged, then put back by something other than git differs from
    // what was staged, though sandfs no longer counts it as changed.
    std::fs::write(work.join("src/lib.rs"), "fn two() {}\n").unwrap();
    git_out(&work, &["add", "src/lib.rs"]);
    std::fs::write(work.join("src/lib.rs"), "fn one() {}\n").unwrap();
    assert_eq!(git_out(&work, &["status", "--porcelain"]), "MM src/lib.rs\n");
    git_out(&work, &["reset", "-q"]);
    assert_eq!(git_out(&work, &["status", "--porcelain"]), "");

    // A folder in place of a file git never saw change still shows the file deleted.
    std::fs::remove_file(work.join("LICENSE")).unwrap();
    std::fs::create_dir(work.join("LICENSE")).unwrap();
    assert_eq!(git_out(&work, &["status", "--porcelain"]), " D LICENSE\n");
    std::fs::remove_dir(work.join("LICENSE")).unwrap();
    std::fs::write(work.join("LICENSE"), "free\n").unwrap();
    assert_eq!(git_out(&work, &["status", "--porcelain"]), "");

    // A fork carries the commits and branches made so far, and goes its own way.
    git_out(&work, &["switch", "-q", "-c", "feature"]);
    sandfs.mount(&["fork", "work", copy.to_str().unwrap()], &copy);
    assert_eq!(git_out(&copy, &["status", "--porcelain", "-b"]), "## feature\n");
    std::fs::write(copy.join("fork.txt"), "fork\n").unwrap();
    git_out(&copy, &["add", "fork.txt"]);
    git_out(&copy, &["commit", "-q", "-m", "in the fork"]);
    assert_eq!(git_out(&work, &["log", "-1", "--format=%s"]), "change\n");
    assert_eq!(git_out(&work, &["status", "--porcelain"]), "");

    // And it all survives a remount.
    sandfs.unmount(&copy);
    sandfs.mount(&["mount", "copy", copy.to_str().unwrap()], &copy);
    assert_eq!(git_out(&copy, &["log", "-1", "--format=%s"]), "in the fork\n");
    assert_eq!(git_out(&copy, &["status", "--porcelain"]), "");
    sandfs.unmount(&copy);
    sandfs.unmount(&work);
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
    while Command::new("kill").args(["-0", &pid]).stderr(std::process::Stdio::null()).status().unwrap().success() {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    // The crash released the workspace too, so another process may take it over.
    sandfs.run(&["serve", "--mcp", "data"]);
    // Mounting it elsewhere would lose track of the dead mount, so it is refused.
    let elsewhere = dirs.path().join("elsewhere");
    let refused = sandfs.try_run(&["mount", "data", elsewhere.to_str().unwrap()]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("sandfs unmount data"), "says how to detach it");
    sandfs.unmount(&ws);
    sandfs.mount(&["mount", "data", ws.to_str().unwrap()], &ws);
    assert_eq!(std::fs::read_to_string(ws.join("out/summary.txt")).unwrap(), "2 rows\n", "saved before the crash");
    sandfs.unmount(&ws);
}

#[test]
fn files_keep_their_source_time_in_every_mount_and_fork() {
    if !fuse_available() {
        return;
    }
    let data = tempfile::tempdir().unwrap();
    let then = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
    std::fs::write(data.path().join("go.mod"), "module x\n").unwrap();
    std::fs::File::options().write(true).open(data.path().join("go.mod")).unwrap().set_modified(then).unwrap();
    let mut sandfs = Sandfs::new();
    let dirs = tempfile::tempdir().unwrap();
    let (ws, fork) = (dirs.path().join("ws"), dirs.path().join("fork"));
    sandfs.mount(&["mount", data.path().to_str().unwrap(), ws.to_str().unwrap(), "--name", "ws"], &ws);
    sandfs.mount(&["fork", "ws", fork.to_str().unwrap(), "--name", "fork"], &fork);

    // Build and test caches compare times: a file nobody changed must look unchanged.
    let modified = |path: &Path| std::fs::metadata(path).unwrap().modified().unwrap();
    assert_eq!(modified(&ws.join("go.mod")), then);
    assert_eq!(modified(&fork.join("go.mod")), then, "a fork shows the same time");
    std::fs::write(fork.join("go.mod"), "module y\n").unwrap();
    assert!(modified(&fork.join("go.mod")) > then, "a write is a change");
    assert_eq!(modified(&ws.join("go.mod")), then);
    sandfs.unmount(&fork);
    sandfs.unmount(&ws);
}

#[test]
fn symlinks_work_in_a_mount_and_survive_a_remount() {
    if !fuse_available() {
        return;
    }
    let data = tempfile::tempdir().unwrap();
    std::fs::create_dir(data.path().join("lib")).unwrap();
    std::fs::write(data.path().join("lib/site.txt"), "site\n").unwrap();
    std::os::unix::fs::symlink("lib", data.path().join("lib64")).unwrap();

    let mut sandfs = Sandfs::new();
    let dirs = tempfile::tempdir().unwrap();
    let ws = dirs.path().join("env");
    sandfs.mount(&["mount", data.path().to_str().unwrap(), ws.to_str().unwrap()], &ws);

    // The folder's own link, and new ones, resolve like on any filesystem.
    assert_eq!(std::fs::read_link(ws.join("lib64")).unwrap(), Path::new("lib"));
    assert_eq!(std::fs::read_to_string(ws.join("lib64/site.txt")).unwrap(), "site\n");
    std::os::unix::fs::symlink("lib/site.txt", ws.join("site")).unwrap();
    assert_eq!(std::fs::read_to_string(ws.join("site")).unwrap(), "site\n");
    assert!(std::fs::symlink_metadata(ws.join("site")).unwrap().file_type().is_symlink());
    std::fs::write(ws.join("site"), "written through the link\n").unwrap();
    assert_eq!(std::fs::read_to_string(ws.join("lib/site.txt")).unwrap(), "written through the link\n");
    std::os::unix::fs::symlink("missing", ws.join("dangling")).unwrap();
    std::fs::rename(ws.join("dangling"), ws.join("lib/dangling")).unwrap();

    // A Python virtual environment is made of links.
    let venv = Command::new("python3").args(["-m", "venv", "--without-pip"]).arg(ws.join(".venv")).output().unwrap();
    assert!(venv.status.success(), "venv: {}", String::from_utf8_lossy(&venv.stderr));
    let python = Command::new(ws.join(".venv/bin/python")).args(["-c", "import sys; print(sys.prefix)"]).output();
    // macOS reports temporary folders through their real path (/private/var).
    let prefix = std::fs::canonicalize(String::from_utf8(python.unwrap().stdout).unwrap().trim()).unwrap();
    assert_eq!(prefix, std::fs::canonicalize(ws.join(".venv")).unwrap());

    let changes = sandfs.stdout(&["changes", "env"]);
    assert!(changes.contains("A .venv/bin/python\n"), "{changes}");
    assert!(changes.contains("A lib/dangling\nM lib/site.txt\nA site\n"), "{changes}");
    assert!(std::fs::symlink_metadata(data.path().join("site")).is_err(), "the folder underneath is untouched");

    sandfs.unmount(&ws);
    sandfs.mount(&["mount", "env", ws.to_str().unwrap()], &ws);
    assert_eq!(std::fs::read_link(ws.join("site")).unwrap(), Path::new("lib/site.txt"));
    assert_eq!(std::fs::read_link(ws.join("lib/dangling")).unwrap(), Path::new("missing"));
    assert!(Command::new(ws.join(".venv/bin/python")).arg("-c").arg("pass").status().unwrap().success());
    sandfs.unmount(&ws);
}

/// Hard links need the FUSE mount; the NFS server under the macOS mount has no LINK.
#[cfg(target_os = "linux")]
#[test]
fn hard_links_work_in_a_mount_and_survive_a_remount() {
    use std::os::unix::fs::MetadataExt;
    if !fuse_available() {
        return;
    }
    let data = tempfile::tempdir().unwrap();
    std::fs::write(data.path().join("base.txt"), "base\n").unwrap();
    let mut sandfs = Sandfs::new();
    let dirs = tempfile::tempdir().unwrap();
    let ws = dirs.path().join("build");
    sandfs.mount(&["mount", data.path().to_str().unwrap(), ws.to_str().unwrap()], &ws);

    std::fs::create_dir(ws.join("deps")).unwrap();
    std::fs::write(ws.join("deps/lib-1a2b.rlib"), "v1").unwrap();
    std::fs::hard_link(ws.join("deps/lib-1a2b.rlib"), ws.join("lib.rlib")).unwrap();
    std::fs::hard_link(ws.join("base.txt"), ws.join("base-link.txt")).unwrap();
    let (a, b) =
        (std::fs::metadata(ws.join("deps/lib-1a2b.rlib")).unwrap(), std::fs::metadata(ws.join("lib.rlib")).unwrap());
    assert_eq!((a.ino(), a.nlink()), (b.ino(), 2));
    std::fs::write(ws.join("lib.rlib"), "v2").unwrap();
    assert_eq!(std::fs::read_to_string(ws.join("deps/lib-1a2b.rlib")).unwrap(), "v2");
    std::fs::remove_file(ws.join("deps/lib-1a2b.rlib")).unwrap();
    assert_eq!(std::fs::metadata(ws.join("lib.rlib")).unwrap().nlink(), 1);
    std::fs::hard_link(ws.join("lib.rlib"), ws.join("deps/lib-1a2b.rlib")).unwrap();

    assert_eq!(sandfs.stdout(&["changes", "build"]), "A base-link.txt\nA deps/lib-1a2b.rlib\nA lib.rlib\n");
    assert!(!data.path().join("lib.rlib").exists(), "the folder underneath is untouched");

    sandfs.unmount(&ws);
    sandfs.mount(&["mount", "build", ws.to_str().unwrap()], &ws);
    std::fs::write(ws.join("deps/lib-1a2b.rlib"), "v3").unwrap();
    assert_eq!(std::fs::read_to_string(ws.join("lib.rlib")).unwrap(), "v3", "still one file after a remount");
    assert_eq!(std::fs::metadata(ws.join("base-link.txt")).unwrap().nlink(), 2);
    sandfs.unmount(&ws);
}

#[test]
fn names_found_missing_appear_once_created() {
    if !fuse_available() {
        return;
    }
    let folder = tempfile::tempdir().unwrap();
    std::fs::create_dir(folder.path().join("lib")).unwrap();
    let mut sandfs = Sandfs::new();
    let dirs = tempfile::tempdir().unwrap();
    let ws = dirs.path().join("ws");
    sandfs.mount(&["mount", folder.path().to_str().unwrap(), ws.to_str().unwrap()], &ws);

    // A module loader probes names that aren't there; the kernel may remember each
    // absence, but every way of making the name must still show it at once.
    let probe = |name: &str| ws.join("lib").join(name);
    let gone = |name: &str| std::fs::metadata(probe(name)).unwrap_err().kind() == std::io::ErrorKind::NotFound;
    for name in ["index.js", "index.json", "index", "moved.js", "copy.js"] {
        assert!(gone(name) && gone(name));
    }
    std::fs::write(probe("index.js"), "one\n").unwrap();
    assert_eq!(std::fs::read_to_string(probe("index.js")).unwrap(), "one\n");
    std::fs::create_dir(probe("index")).unwrap();
    assert!(std::fs::metadata(probe("index")).unwrap().is_dir());
    std::fs::write(probe("first.js"), "two\n").unwrap();
    std::fs::rename(probe("first.js"), probe("moved.js")).unwrap();
    assert_eq!(std::fs::read_to_string(probe("moved.js")).unwrap(), "two\n");
    std::fs::copy(probe("moved.js"), probe("copy.js")).unwrap();
    assert_eq!(std::fs::read_to_string(probe("copy.js")).unwrap(), "two\n");
    // And a removed name is missing again.
    std::fs::remove_file(probe("index.js")).unwrap();
    assert!(gone("index.js"));
    assert!(gone("index.json"));
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

    // A program that writes a file, closes it and exits, with no fsync, has it saved
    // once the file goes quiet: a crash a second later keeps it.
    std::fs::write(ws.join("report.txt"), "done\n").unwrap();
    std::thread::sleep(std::time::Duration::from_secs(1));
    let mount_json = sandfs.home.path().join("workspaces/ws/mount.json");
    let info: serde_json::Value = serde_json::from_slice(&std::fs::read(&mount_json).unwrap()).unwrap();
    let pid = info["pid"].as_u64().unwrap().to_string();
    assert!(Command::new("kill").args(["-9", &pid]).status().unwrap().success());
    while Command::new("kill").args(["-0", &pid]).stderr(std::process::Stdio::null()).status().unwrap().success() {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    // Another process may get the dead server's pid. Reading the workspace then uses its
    // last save rather than asking that process to save.
    let mut reused = info.clone();
    reused["pid"] = std::process::id().into();
    std::fs::write(&mount_json, serde_json::to_vec(&reused).unwrap()).unwrap();
    assert_eq!(sandfs.stdout(&["changes", "ws"]), "M log.txt\nD old.txt\nA report.txt\n");
    sandfs.unmount(&ws);
    sandfs.mount(&["mount", "ws", ws.to_str().unwrap()], &ws);
    assert_eq!(std::fs::read_to_string(ws.join("report.txt")).unwrap(), "done\n", "saved before the crash");
    assert_eq!(std::fs::read_to_string(ws.join("log.txt")).unwrap(), "new\n");
    assert_eq!(sandfs.stdout(&["changes", "ws"]), "M log.txt\nD old.txt\nA report.txt\n");
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
    let dir = dir.to_str().unwrap();
    if cfg!(target_os = "macos") {
        let table = String::from_utf8(Command::new("mount").output().unwrap().stdout).unwrap();
        table.lines().filter(|line| line.contains(&format!(" on {dir} ("))).count()
    } else {
        let table = std::fs::read_to_string("/proc/self/mountinfo").unwrap();
        table.lines().filter(|line| line.split(' ').nth(4) == Some(dir)).count()
    }
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

/// A mount that fails leaves no workspace behind, so the same command can be retried.
#[cfg(target_os = "macos")]
#[test]
fn a_failed_mount_leaves_no_workspace_behind() {
    use std::os::unix::fs::MetadataExt;
    // A folder someone else owns, which macOS won't let this user mount on.
    let theirs = Path::new("/var/empty");
    let mine = nix::unistd::getuid().as_raw();
    if !fuse_available() || std::fs::metadata(theirs).map_or(true, |meta| meta.uid() == mine) || mine == 0 {
        return;
    }
    let folder = tempfile::tempdir().unwrap();
    let mut sandfs = Sandfs::new();
    let failed = sandfs.try_run(&["mount", "--name", "ws", folder.path().to_str().unwrap(), theirs.to_str().unwrap()]);
    assert!(!failed.status.success());
    assert!(!sandfs.stdout(&["list"]).contains("ws\t"), "the workspace is removed again");
    let dirs = tempfile::tempdir().unwrap();
    let ws = dirs.path().join("ws");
    sandfs.mount(&["mount", "--name", "ws", folder.path().to_str().unwrap(), ws.to_str().unwrap()], &ws);
    sandfs.unmount(&ws);
}

#[test]
fn a_fork_without_a_directory_is_a_snapshot() {
    if !fuse_available() {
        return;
    }
    let data = tempfile::tempdir().unwrap();
    std::fs::write(data.path().join("notes.txt"), "notes\n").unwrap();
    let mut sandfs = Sandfs::new();
    let dirs = tempfile::tempdir().unwrap();
    let (work, back) = (dirs.path().join("work"), dirs.path().join("back"));
    sandfs.mount(&["mount", data.path().to_str().unwrap(), work.to_str().unwrap()], &work);
    std::fs::write(work.join("draft.txt"), "first draft\n").unwrap();

    // Taken with nothing mounted, so it costs a save and a link per saved file.
    assert!(!sandfs.try_run(&["fork", "work"]).status.success(), "a snapshot needs a name");
    assert_eq!(sandfs.stdout(&["fork", "work", "--name", "turn-1"]), "turn-1 forked from work\n");
    assert!(sandfs.stdout(&["list"]).contains("turn-1\t"));
    assert_eq!(sandfs.stdout(&["changes", "turn-1"]), "A draft.txt\n");

    // The workspace goes on; the snapshot keeps the files as they were.
    std::fs::write(work.join("draft.txt"), "second draft, longer\n").unwrap();
    std::fs::remove_file(work.join("notes.txt")).unwrap();
    assert_eq!(sandfs.stdout(&["changes", "work"]), "A draft.txt\nD notes.txt\n");
    sandfs.mount(&["fork", "turn-1", back.to_str().unwrap()], &back);
    assert_eq!(std::fs::read_to_string(back.join("draft.txt")).unwrap(), "first draft\n");
    assert_eq!(std::fs::read_to_string(back.join("notes.txt")).unwrap(), "notes\n");
    sandfs.unmount(&back);
    sandfs.unmount(&work);
}

#[test]
fn saved_files_can_be_changed_moved_and_read_again() {
    if !fuse_available() {
        return;
    }
    let data = tempfile::tempdir().unwrap();
    let mut sandfs = Sandfs::new();
    let dirs = tempfile::tempdir().unwrap();
    let work = dirs.path().join("work");
    sandfs.mount(&["mount", data.path().to_str().unwrap(), work.to_str().unwrap()], &work);
    std::fs::create_dir(work.join("out")).unwrap();
    let big: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(work.join("out/big.bin"), &big).unwrap();
    std::fs::write(work.join("out/log.txt"), "one\n").unwrap();
    // `changes` saves first, so from here on both files are read from the saved copies.
    assert_eq!(sandfs.stdout(&["changes", "work"]), "A out/big.bin\nA out/log.txt\n");

    let mut log = std::fs::OpenOptions::new().append(true).open(work.join("out/log.txt")).unwrap();
    std::io::Write::write_all(&mut log, b"two\n").unwrap();
    drop(log);
    std::fs::rename(work.join("out"), work.join("done")).unwrap();
    std::fs::copy(work.join("done/big.bin"), work.join("copy.bin")).unwrap();
    assert_eq!(std::fs::read(work.join("done/big.bin")).unwrap(), big);
    assert_eq!(std::fs::read_to_string(work.join("done/log.txt")).unwrap(), "one\ntwo\n");

    sandfs.unmount(&work);
    sandfs.mount(&["mount", "work", work.to_str().unwrap()], &work);
    assert_eq!(std::fs::read(work.join("done/big.bin")).unwrap(), big);
    assert_eq!(std::fs::read(work.join("copy.bin")).unwrap(), big);
    assert_eq!(std::fs::read_to_string(work.join("done/log.txt")).unwrap(), "one\ntwo\n");
    assert_eq!(sandfs.stdout(&["changes", "work"]), "A copy.bin\nA done/big.bin\nA done/log.txt\n");
    sandfs.unmount(&work);
}
