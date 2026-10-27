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
        // Another test's child can hold the script open for writing for a
        // moment between its fork and exec, so a busy file is tried again.
        let run = || Command::new(main.join("run.sh")).output();
        let out = (0..50)
            .map(|_| run())
            .find(|out| {
                let busy = matches!(out, Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy);
                if busy {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                !busy
            })
            .unwrap_or_else(run)
            .unwrap();
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

#[test]
fn a_process_on_its_way_out_leaves_a_new_home_at_the_same_path_alone() {
    if !fuse_available() {
        return;
    }
    let data = tempfile::tempdir().unwrap();
    std::fs::write(data.path().join("rows.csv"), "id\n").unwrap();
    let mut sandfs = Sandfs::new();
    let dirs = tempfile::tempdir().unwrap();
    let [one, two] = ["one", "two"].map(|name| dirs.path().join(name));
    sandfs.mount(&["mount", data.path().to_str().unwrap(), one.to_str().unwrap()], &one);
    let old = server_pid(&sandfs, "one");

    // A script that cleans up by deleting the whole home, then starts over at the same
    // path, while the last process still lingers after its last unmount.
    sandfs.unmount(&one);
    std::fs::remove_dir_all(sandfs.home.path()).unwrap();
    std::fs::create_dir(sandfs.home.path()).unwrap();
    sandfs.mount(&["mount", data.path().to_str().unwrap(), one.to_str().unwrap()], &one);
    let new = server_pid(&sandfs, "one");
    assert_ne!(new, old);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while alive(&old) {
        assert!(std::time::Instant::now() < deadline, "the old process stayed");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    // The new process still answers once the old one has gone.
    sandfs.mount(&["fork", "one", two.to_str().unwrap()], &two);
    assert_eq!(server_pid(&sandfs, "two"), new);
    assert_eq!(std::fs::read_to_string(two.join("rows.csv")).unwrap(), "id\n");
    sandfs.unmount(&two);
    sandfs.unmount(&one);
}

/// The parent, process group and session of `pid`, from /proc.
#[cfg(target_os = "linux")]
fn lineage(pid: u32) -> (u32, u32, u32) {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    // After the command name, which may itself hold spaces and parentheses.
    let fields: Vec<u32> =
        stat[stat.rfind(')').unwrap() + 2..].split(' ').skip(1).take(3).map(|field| field.parse().unwrap()).collect();
    (fields[0], fields[1], fields[2])
}

#[test]
#[cfg(target_os = "linux")]
fn the_mount_process_shares_nothing_with_whoever_started_it() {
    if !fuse_available() {
        return;
    }
    let data = tempfile::tempdir().unwrap();
    std::fs::write(data.path().join("notes.txt"), "notes\n").unwrap();
    let mut sandfs = Sandfs::new();
    let dirs = tempfile::tempdir().unwrap();
    let work = dirs.path().join("work");
    // Job runners find a job's processes by their process group or their descent from
    // the job; neither may reach the process that serves every job's mounts. It keeps
    // what git needs to fetch, such as a token for a credential helper, but not a
    // caller's repository.
    let output = Command::new(env!("CARGO_BIN_EXE_sandfs"))
        .env("SANDFS_HOME", sandfs.home.path())
        .env("A_FETCH_TOKEN", "secret")
        .env("GIT_DIR", "/nonexistent")
        .args(["mount", data.path().to_str().unwrap(), work.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    sandfs.mounts.push(work.clone());
    let pid: u32 = server_pid(&sandfs, "work").parse().unwrap();

    let environ = std::fs::read(format!("/proc/{pid}/environ")).unwrap();
    let names: Vec<&[u8]> = environ.split(|&b| b == 0).filter_map(|entry| entry.split(|&b| b == b'=').next()).collect();
    assert!(names.contains(&&b"SANDFS_HOME"[..]) && names.contains(&&b"PATH"[..]));
    assert!(names.contains(&&b"A_FETCH_TOKEN"[..]) && !names.contains(&&b"GIT_DIR"[..]));

    let me = std::process::id();
    let (_, my_group, my_session) = lineage(me);
    let (_, group, session) = lineage(pid);
    assert_ne!(group, my_group);
    assert_ne!(session, my_session);
    let mut ancestor = pid;
    while ancestor > 1 {
        ancestor = lineage(ancestor).0;
        assert_ne!(ancestor, me, "the mount process descends from the test");
    }
    assert_eq!(std::fs::read_to_string(work.join("notes.txt")).unwrap(), "notes\n");
    sandfs.unmount(&work);
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
fn git_runs_quietly_in_a_worktree_added_from_a_git_mount() {
    if !fuse_available() {
        return;
    }
    let remote = tempfile::Builder::new().prefix("sandfs-remote").tempdir().unwrap();
    let remote = remote.path();
    git(remote, &["init", "-q"]);
    git(remote, &["config", "uploadpack.allowFilter", "true"]);
    std::fs::write(remote.join("README.md"), "hello\n").unwrap();
    git(remote, &["add", "-A"]);
    git(remote, &["commit", "-q", "-m", "first"]);

    let mut sandfs = Sandfs::new();
    let dirs = tempfile::tempdir().unwrap();
    let [work, tree] = ["work", "tree"].map(|name| dirs.path().join(name));
    let source = format!("git:file://{}", remote.display());
    sandfs.mount(&["mount", &source, work.to_str().unwrap()], &work);

    // The worktree is a plain folder that shares the mount's config, file monitor
    // included, so git runs the monitor where no workspace is mounted.
    git_out(&work, &["worktree", "add", "-q", "-b", "side", tree.to_str().unwrap()]);
    std::fs::write(tree.join("README.md"), "hello, worktree\n").unwrap();
    std::fs::write(tree.join("notes.txt"), "notes\n").unwrap();
    for dir in [&tree, &work] {
        let output = Command::new("git").arg("-C").arg(dir).args(["status", "--porcelain"]).output().unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stderr), "", "git in {}", dir.display());
        let expected = if dir == &tree { " M README.md\n?? notes.txt\n" } else { "" };
        assert_eq!(String::from_utf8_lossy(&output.stdout), expected, "git in {}", dir.display());
    }
    sandfs.unmount(&work);
}

#[test]
fn a_fork_of_a_clone_carries_uncommitted_files_and_pushes_back_on_its_own_branch() {
    if !fuse_available() {
        return;
    }
    use std::os::unix::fs::PermissionsExt;

    // Named so that the repository can also be given as a URL (see the remote test).
    let data = tempfile::Builder::new().prefix("sandfs-task").tempdir().unwrap();
    let repo = data.path().join("repo");
    std::fs::create_dir_all(repo.join("old")).unwrap();
    std::fs::create_dir_all(repo.join("docs")).unwrap();
    git(&repo, &["init", "-q"]);
    std::fs::write(repo.join("README.md"), "hello\n").unwrap();
    std::fs::write(repo.join("old/notes.md"), "old\n").unwrap();
    std::fs::write(repo.join("docs/guide"), "guide\n").unwrap();
    std::fs::create_dir_all(repo.join("lib")).unwrap();
    std::fs::write(repo.join("lib/one.rs"), "one\n").unwrap();
    std::fs::create_dir_all(repo.join("assets")).unwrap();
    std::fs::write(repo.join("assets/logo.txt"), "logo\n").unwrap();
    std::fs::write(repo.join("queue"), "queue\n").unwrap();
    std::fs::write(repo.join("run.sh"), "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(repo.join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(repo.join(".gitignore"), "*.log\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "first"]);

    // Uncommitted work of every kind: an edit, a staged new file, a new one, a link, a
    // deletion that empties a folder, a mode change, a folder in place of a file, a file
    // in place of a folder, a link in place of a folder, an ignored file, and a pipe,
    // which is left out.
    std::fs::write(repo.join("README.md"), "hello, edited\n").unwrap();
    std::fs::write(repo.join("staged.txt"), "staged\n").unwrap();
    git(&repo, &["add", "staged.txt"]);
    std::fs::write(repo.join("draft.txt"), "draft\n").unwrap();
    std::os::unix::fs::symlink("README.md", repo.join("readme")).unwrap();
    std::fs::remove_dir_all(repo.join("old")).unwrap();
    std::fs::set_permissions(repo.join("run.sh"), std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::remove_file(repo.join("docs/guide")).unwrap();
    std::fs::create_dir(repo.join("docs/guide")).unwrap();
    std::fs::write(repo.join("docs/guide/intro.md"), "intro\n").unwrap();
    std::fs::remove_dir_all(repo.join("lib")).unwrap();
    std::fs::write(repo.join("lib"), "lib\n").unwrap();
    std::fs::remove_dir_all(repo.join("assets")).unwrap();
    std::fs::create_dir(repo.join("art")).unwrap();
    std::fs::write(repo.join("art/logo.txt"), "new logo\n").unwrap();
    std::os::unix::fs::symlink("art", repo.join("assets")).unwrap();
    std::fs::write(repo.join("build.log"), "ignored\n").unwrap();
    std::fs::remove_file(repo.join("queue")).unwrap();
    assert!(Command::new("mkfifo").arg(repo.join("queue")).status().unwrap().success());
    let before = git_out(&repo, &["status", "--porcelain"]);

    let mut sandfs = Sandfs::new();
    let task = data.path().join("task");
    // From a folder inside the repository, as from `cd repo/docs; sandfs fork . DIR`.
    let docs = repo.join("docs");
    let args = ["fork", docs.to_str().unwrap(), task.to_str().unwrap(), "--branch", "task/one", "--read-only", "docs"];
    sandfs.mount(&args, &task);

    // The copy looks like the folder to git, on its own branch, though nothing is staged in it.
    assert_eq!(git_out(&task, &["status", "--porcelain", "-b"]).lines().next(), Some("## task/one"));
    let unstaged = |status: &str| {
        let mut lines: Vec<String> =
            status.lines().filter(|line| !line.ends_with(" queue")).map(|line| line.replace("A  ", "?? ")).collect();
        lines.sort();
        lines
    };
    assert_eq!(unstaged(&git_out(&task, &["status", "--porcelain"])), unstaged(&before));
    assert!(before.contains("A  staged.txt\n"), "{before}");
    assert_eq!(std::fs::read_to_string(task.join("docs/guide/intro.md")).unwrap(), "intro\n");
    assert!(std::fs::write(task.join("docs/guide/intro.md"), "changed\n").is_err(), "docs stays read-only");
    assert_eq!(std::fs::read_to_string(task.join("lib")).unwrap(), "lib\n");
    assert_eq!(std::fs::read_link(task.join("assets")).unwrap(), Path::new("art"));
    assert_eq!(std::fs::read_to_string(task.join("queue")).unwrap(), "queue\n", "the pipe was left out");
    assert_eq!(std::fs::read_link(task.join("readme")).unwrap(), Path::new("README.md"));
    assert!(!task.join("old").exists(), "a folder git emptied is gone");
    assert!(!task.join("build.log").exists(), "ignored files stay out");
    assert_eq!(std::fs::metadata(task.join("run.sh")).unwrap().permissions().mode() & 0o111, 0);
    assert_eq!(
        sandfs.stdout(&["changes", "task"]),
        "M README.md\nA art/logo.txt\nA assets\nD assets/logo.txt\nD docs/guide\nA docs/guide/intro.md\nA draft.txt\nA lib\nD lib/one.rs\nD old/notes.md\nA readme\nM run.sh\nA staged.txt\n"
    );

    // Later edits in the folder stay out of the copy, and the copy's commits come back
    // as a branch, leaving the folder's own work alone.
    std::fs::write(repo.join("README.md"), "hello, later\n").unwrap();
    assert_eq!(std::fs::read_to_string(task.join("README.md")).unwrap(), "hello, edited\n");
    git_out(&task, &["add", "-A"]);
    git_out(&task, &["commit", "-q", "-m", "task"]);
    git_out(&task, &["push", "-q", "origin", "task/one"]);
    assert_eq!(git_out(&repo, &["show", "task/one:README.md"]), "hello, edited\n");
    assert_eq!(git_out(&repo, &["log", "--format=%s", "task/one"]), "task\nfirst\n");
    std::fs::write(repo.join("README.md"), "hello, edited\n").unwrap();
    assert_eq!(git_out(&repo, &["status", "--porcelain"]), before);

    // A fork of the copy, made from a folder in its mount, starts a branch of its own
    // at the copy's commit, with the copy's files and what it has staged.
    std::fs::write(task.join("draft.txt"), "draft, staged\n").unwrap();
    git_out(&task, &["add", "draft.txt"]);
    let second = data.path().join("second");
    let art = task.join("art");
    let args = ["fork", art.to_str().unwrap(), second.to_str().unwrap(), "--branch", "task/two"];
    sandfs.mount(&args, &second);
    assert_eq!(git_out(&second, &["status", "--porcelain", "-b"]), "## task/two\nM  draft.txt\n");
    assert_eq!(git_out(&second, &["log", "--format=%s"]), "task\nfirst\n");
    assert_eq!(
        git_out(&task, &["status", "--porcelain", "-b"]),
        "## task/one\nM  draft.txt\n",
        "the copy is as it was"
    );
    assert!(std::fs::write(second.join("docs/guide/intro.md"), "changed\n").is_err(), "forks keep read-only paths");

    // Copied back as files instead: what the folder held when the copy was made, its
    // uncommitted edits included, is replaced, but a file edited there since is not.
    std::fs::write(task.join("draft.txt"), "draft, finished\n").unwrap();
    std::fs::write(task.join("README.md"), "hello, from the task\n").unwrap();
    std::fs::write(repo.join("draft.txt"), "draft, mine\n").unwrap();
    let output = sandfs.try_run(&["changes", "task", "--copy-to", repo.to_str().unwrap()]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success() && stderr.contains("would lose that: draft.txt;"), "{stderr}");
    assert_eq!(std::fs::read_to_string(repo.join("README.md")).unwrap(), "hello, edited\n", "nothing was written");
    std::fs::write(repo.join("draft.txt"), "draft\n").unwrap();
    sandfs.run(&["changes", "task", "--copy-to", repo.to_str().unwrap()]);
    assert_eq!(std::fs::read_to_string(repo.join("README.md")).unwrap(), "hello, from the task\n");
    assert_eq!(std::fs::read_to_string(repo.join("draft.txt")).unwrap(), "draft, finished\n");
    std::fs::write(repo.join("draft.txt"), "draft, mine again\n").unwrap();
    sandfs.run(&["changes", "task", "draft.txt", "--copy-to", repo.to_str().unwrap(), "--force"]);
    assert_eq!(std::fs::read_to_string(repo.join("draft.txt")).unwrap(), "draft, finished\n");

    // Refused before anything is made: a branch the repository or a copy has, a name git
    // won't take, a remote repository's branch, a folder source, a plain folder's branch, and a
    // workspace that exists.
    let refused = |sandfs: &Sandfs, args: &[&str], message: &str| {
        let output = sandfs.try_run(args);
        assert!(!output.status.success(), "sandfs {args:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(message), "sandfs {args:?}: {stderr}");
    };
    let other = data.path().join("other");
    let other = other.to_str().unwrap();
    let repo_path = repo.to_str().unwrap();
    refused(&sandfs, &["fork", repo_path, other, "--branch", "task/one"], "already has a branch task/one");
    refused(&sandfs, &["fork", "second", other, "--branch", "task/two"], "already has a branch task/two");
    refused(&sandfs, &["fork", repo_path, other, "--branch", "bad..name"], "not a name git takes");
    sandfs.run(&["fork", "second", "--name", "keep"]);
    let url = format!("git:file://{}", repo.display());
    let remote = data.path().join("remote");
    sandfs.mount(&["mount", &url, remote.to_str().unwrap()], &remote);
    refused(&sandfs, &["fork", "remote", other, "--branch", "task/one"], "already has a branch task/one");
    // A partial clone's pushes go to the remote it borrows from, which is what counts.
    git(&repo, &["config", "uploadpack.allowFilter", "true"]);
    let partial = data.path().join("partial");
    git(data.path(), &["clone", "-q", "--filter=blob:none", &format!("file://{}", repo.display()), "partial"]);
    refused(
        &sandfs,
        &["fork", partial.to_str().unwrap(), other, "--branch", "task/one"],
        "already has a branch task/one",
    );
    // A path is taken as it is, never as a source with an @revision.
    git(data.path(), &["clone", "-q", repo.to_str().unwrap(), "clone@work"]);
    sandfs.run(&["fork", data.path().join("clone@work").to_str().unwrap(), "--name", "at"]);
    let plain = data.path().join("plain");
    std::fs::create_dir(&plain).unwrap();
    let folder = data.path().join("folder");
    sandfs.mount(&["mount", plain.to_str().unwrap(), folder.to_str().unwrap()], &folder);
    refused(&sandfs, &["fork", "folder", other, "--branch", "two"], "is a folder");
    refused(&sandfs, &["fork", plain.to_str().unwrap(), other, "--branch", "two"], "needs a git repository");
    refused(&sandfs, &["fork", repo_path, other, "--name", "task"], "already exists");
    assert_eq!(sandfs.stdout(&["list"]).lines().count(), 6, "no workspace was left behind");
    for dir in [&second, &remote, &folder] {
        sandfs.unmount(dir);
    }

    // Removing a mounted copy unmounts it first only when asked.
    refused(&sandfs, &["rm", "task"], "pass --unmount");
    sandfs.run(&["rm", "--unmount", task.to_str().unwrap()]);
    sandfs.mounts.clear();
    assert!(!sandfs.stdout(&["list"]).lines().any(|line| line.starts_with("task\t")));
}

/// Whether files clone without copying their contents in `dir`: APFS on macOS, btrfs or
/// XFS on Linux. With `SANDFS_REQUIRE_REFLINK` set, as CI does for a run on btrfs, they
/// must.
fn clones_files(dir: &Path) -> bool {
    let clones = cfg!(target_os = "macos") || {
        std::fs::write(dir.join("clone-check"), "check\n").unwrap();
        let copy =
            Command::new("cp").current_dir(dir).args(["--reflink=always", "clone-check", "clone-check-copy"]).output();
        copy.is_ok_and(|out| out.status.success())
    };
    assert!(clones || std::env::var_os("SANDFS_REQUIRE_REFLINK").is_none(), "{} can't clone files", dir.display());
    clones
}

#[test]
fn a_fork_of_a_clone_is_a_real_folder_where_the_disk_clones_files() {
    if !fuse_available() {
        return;
    }
    let data = tempfile::tempdir().unwrap();
    let repo = data.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    std::fs::write(repo.join("README.md"), "hello\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "first"]);
    std::fs::write(repo.join("README.md"), "hello, edited\n").unwrap();

    let mut sandfs = Sandfs::new();
    let task = data.path().join("task");
    let made = sandfs.stdout(&["fork", repo.to_str().unwrap(), task.to_str().unwrap()]);
    let task = task.canonicalize().unwrap();
    if clones_files(data.path()) {
        assert_eq!(made, format!("task kept at {}\n", task.display()));
        assert_eq!(mounts_at(&task), 0, "a folder, not a mount");
    } else {
        sandfs.mounts.push(task.clone());
        assert_eq!(made, format!("task mounted at {}\n", task.display()));
        assert_eq!(mounts_at(&task), 1);
    }
    assert_eq!(std::fs::read_to_string(task.join("README.md")).unwrap(), "hello, edited\n");
    assert_eq!(sandfs.stdout(&["changes", "task"]), "M README.md\n");
    assert!(
        !std::fs::read_dir(data.path())
            .unwrap()
            .any(|entry| { entry.unwrap().file_name().to_string_lossy().starts_with(".sandfs-clone-check") })
    );

    // Asked for, a mount is a mount anywhere.
    let mounted = data.path().join("mounted");
    sandfs.mount(&["fork", repo.to_str().unwrap(), mounted.to_str().unwrap(), "--mount"], &mounted);
    assert_eq!(mounts_at(&mounted.canonicalize().unwrap()), 1);
}

#[test]
fn a_fork_of_a_plain_folder_keeps_it_as_it_was() {
    use std::os::unix::fs::PermissionsExt;

    if !fuse_available() {
        return;
    }
    let mut sandfs = Sandfs::new();
    let data = tempfile::tempdir().unwrap();
    let folder = data.path().join("notes");
    std::fs::create_dir_all(folder.join("drafts")).unwrap();
    std::fs::write(folder.join("todo.md"), "a\n").unwrap();
    std::fs::write(folder.join("drafts/plan.md"), "plan\n").unwrap();
    std::fs::write(folder.join("run.sh"), "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(folder.join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("todo.md", folder.join("todo")).unwrap();
    let copies_dir = sandfs.home.path().join("copies");
    // The copies themselves, not the files that hold one for a mount.
    let copies = || {
        let entries = std::fs::read_dir(&copies_dir).map(|entries| entries.flatten().collect::<Vec<_>>());
        entries
            .unwrap_or_default()
            .iter()
            .filter(|entry| entry.path().extension().is_none_or(|ext| ext != "held"))
            .count()
    };

    let task = data.path().join("task");
    let output = sandfs.try_run(&["fork", folder.to_str().unwrap(), task.to_str().unwrap()]);
    // Where a fork of the folder copies files the disk can't clone.
    let mut allow_copy = vec![];
    if !output.status.success() {
        // A disk that can't clone files, such as ext4, refuses, leaving nothing behind,
        // unless told to copy them instead; the rest then runs on copies.
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("needs the folder and sandfs's home on one disk that clones files"), "{stderr}");
        assert!(stderr.contains("--allow-copy"), "{stderr}");
        if cfg!(target_os = "macos") {
            panic!("APFS clones files: {stderr}");
        }
        assert!(!sandfs.stdout(&["list"]).contains("task"));
        assert_eq!(copies(), 0);
        allow_copy.push("--allow-copy");
        sandfs.run(&[&["fork", folder.to_str().unwrap(), task.to_str().unwrap()], &allow_copy[..]].concat());
    }
    sandfs.mounts.push(task.clone());
    let secs = |path: &Path| {
        std::fs::metadata(path).unwrap().modified().unwrap().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
    };

    // The fork is the folder as it was, times and modes included; later edits there stay out.
    assert_eq!(std::fs::read_to_string(task.join("todo.md")).unwrap(), "a\n");
    assert_eq!(secs(&task.join("todo.md")), secs(&folder.join("todo.md")));
    assert_ne!(std::fs::metadata(task.join("run.sh")).unwrap().permissions().mode() & 0o111, 0);
    assert_eq!(std::fs::read_link(task.join("todo")).unwrap(), Path::new("todo.md"));
    std::fs::write(folder.join("todo.md"), "a, mine\n").unwrap();
    std::fs::write(folder.join("drafts/plan.md"), "plan, mine\n").unwrap();
    assert_eq!(std::fs::read_to_string(task.join("todo.md")).unwrap(), "a\n");
    assert_eq!(std::fs::read_to_string(task.join("drafts/plan.md")).unwrap(), "plan\n");

    // Its work goes back with `changes --copy-to`, which won't lose an edit made in the
    // folder since.
    std::fs::write(task.join("drafts/plan.md"), "plan, done\n").unwrap();
    std::fs::write(task.join("new.md"), "new\n").unwrap();
    assert_eq!(sandfs.stdout(&["changes", "task"]), "M drafts/plan.md\nA new.md\n");
    let output = sandfs.try_run(&["changes", "task", "--copy-to", folder.to_str().unwrap()]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success() && stderr.contains("would lose that: drafts/plan.md;"), "{stderr}");
    assert!(!folder.join("new.md").exists(), "nothing was written");
    std::fs::write(folder.join("drafts/plan.md"), "plan\n").unwrap();
    sandfs.run(&["changes", "task", "--copy-to", folder.to_str().unwrap()]);
    assert_eq!(std::fs::read_to_string(folder.join("drafts/plan.md")).unwrap(), "plan, done\n");
    assert_eq!(std::fs::read_to_string(folder.join("new.md")).unwrap(), "new\n");
    assert_eq!(std::fs::read_to_string(folder.join("todo.md")).unwrap(), "a, mine\n", "unchanged in the fork");

    // A snapshot of the fork shares its copy, which goes with the last workspace over it,
    // however the home is named.
    sandfs.run(&["fork", "task", "--name", "snap"]);
    let linked = data.path().join("home");
    std::os::unix::fs::symlink(sandfs.home.path(), &linked).unwrap();
    let rm = Command::new(env!("CARGO_BIN_EXE_sandfs"))
        .env("SANDFS_HOME", &linked)
        .args(["rm", "--unmount", "task"])
        .output()
        .unwrap();
    assert!(rm.status.success(), "{}", String::from_utf8_lossy(&rm.stderr));
    sandfs.mounts.clear();
    assert_eq!(copies(), 1);
    sandfs.mount(&["mount", "snap", task.to_str().unwrap()], &task);
    assert_eq!(std::fs::read_to_string(task.join("todo.md")).unwrap(), "a\n");
    sandfs.run(&["rm", "--unmount", "snap"]);
    sandfs.mounts.clear();
    assert_eq!(copies(), 0);

    // Moving a fork of the folder takes a new copy of the folder as it is now, keeping
    // the fork's own changes, and the old copy goes.
    sandfs.run(&[&["fork", folder.to_str().unwrap(), task.to_str().unwrap()], &allow_copy[..]].concat());
    sandfs.mounts.push(task.clone());
    std::fs::write(task.join("mine.md"), "mine\n").unwrap();
    sandfs.unmount(&task);
    std::fs::write(folder.join("todo.md"), "b\n").unwrap();
    std::fs::write(folder.join("gone.md"), "going\n").unwrap();
    sandfs.run(&["update", "task"]);
    assert_eq!(copies(), 1);
    std::fs::write(folder.join("todo.md"), "c\n").unwrap();
    std::fs::remove_file(folder.join("gone.md")).unwrap();
    sandfs.mount(&["mount", "task", task.to_str().unwrap()], &task);
    assert_eq!(std::fs::read_to_string(task.join("todo.md")).unwrap(), "b\n");
    assert_eq!(std::fs::read_to_string(task.join("mine.md")).unwrap(), "mine\n");
    assert_eq!(sandfs.stdout(&["changes", "task"]), "A mine.md\n");
    // Mounted, it moves in place onto another new copy. The old one stays while the
    // mount may read files of it left open, until the mount ends.
    let held = std::fs::File::open(task.join("gone.md")).unwrap();
    sandfs.run(&["update", "task"]);
    assert_eq!(copies(), 2);
    assert_eq!(std::fs::read_to_string(task.join("todo.md")).unwrap(), "c\n");
    assert!(!task.join("gone.md").exists());
    let mut was = String::new();
    std::io::Read::read_to_string(&mut &held, &mut was).unwrap();
    assert_eq!(was, "going\n");
    drop(held);
    assert_eq!(sandfs.stdout(&["changes", "task"]), "A mine.md\n");
    sandfs.run(&["rm", "--unmount", "task"]);
    sandfs.mounts.clear();
    assert_eq!(copies(), 0);
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
    std::fs::create_dir(data.path().join("pkg")).unwrap();
    for name in ["go.mod", "go.sum", "pkg/a.go"] {
        std::fs::write(data.path().join(name), "module x\n").unwrap();
        std::fs::File::options().write(true).open(data.path().join(name)).unwrap().set_modified(then).unwrap();
    }
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

    // A move or a second name keeps the time, after the move is saved and remounted too.
    std::fs::rename(ws.join("go.sum"), ws.join("go.sum.old")).unwrap();
    std::fs::rename(ws.join("pkg"), ws.join("lib")).unwrap();
    let mut moved = vec!["go.sum.old", "lib/a.go", "go.mod"];
    // The NFS server under the macOS mount has no hard links.
    if cfg!(target_os = "linux") {
        std::fs::hard_link(ws.join("go.mod"), ws.join("go.mod.link")).unwrap();
        moved.push("go.mod.link");
    }
    for &name in &moved {
        assert_eq!(modified(&ws.join(name)), then, "{name}");
    }
    sandfs.unmount(&ws);
    sandfs.mount(&["mount", "ws", ws.to_str().unwrap()], &ws);
    for name in moved {
        assert_eq!(modified(&ws.join(name)), then, "{name} after a remount");
    }
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

/// Files written and closed keep their contents when the kernel, short of memory,
/// forgets them before their writes settle. Forgetting is forced by dropping the
/// kernel's caches, which only root may do; elsewhere this has nothing to test.
#[cfg(target_os = "linux")]
#[test]
fn written_files_survive_the_kernel_forgetting_them() {
    if !fuse_available() || std::fs::OpenOptions::new().write(true).open("/proc/sys/vm/drop_caches").is_err() {
        return;
    }
    let folder = tempfile::tempdir().unwrap();
    let mut sandfs = Sandfs::new();
    let dirs = tempfile::tempdir().unwrap();
    let ws = dirs.path().join("ws");
    sandfs.mount(&["mount", folder.path().to_str().unwrap(), ws.to_str().unwrap()], &ws);
    for i in 0..20 {
        let content = format!("version {i}\n").repeat(100);
        std::fs::write(ws.join("index.lock"), &content).unwrap();
        std::fs::rename(ws.join("index.lock"), ws.join("index")).unwrap();
        std::fs::write("/proc/sys/vm/drop_caches", "2\n").unwrap();
        assert_eq!(std::fs::read_to_string(ws.join("index")).unwrap(), content, "round {i}");
    }
    sandfs.unmount(&ws);
}

/// The kernel keeps each folder's listing once read, so every change made through the
/// mount must show in the next listing of every folder it touched, including folders
/// too big for one page and listings abandoned halfway.
#[test]
fn kept_listings_follow_every_change() {
    if !fuse_available() {
        return;
    }
    let folder = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(folder.path().join("a")).unwrap();
    std::fs::create_dir_all(folder.path().join("b")).unwrap();
    std::fs::write(folder.path().join("a/one.txt"), "1\n").unwrap();
    std::fs::create_dir(folder.path().join("big")).unwrap();
    for i in 0..1000 {
        std::fs::write(folder.path().join(format!("big/file-with-a-long-name-{i:04}.txt")), "x").unwrap();
    }
    let mut sandfs = Sandfs::new();
    let dirs = tempfile::tempdir().unwrap();
    let ws = dirs.path().join("ws");
    sandfs.mount(&["mount", folder.path().to_str().unwrap(), ws.to_str().unwrap()], &ws);

    let names = |dir: &str| {
        let mut names: Vec<String> = std::fs::read_dir(ws.join(dir))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    };
    // Each check lists both folders twice: the first listing after a change must be
    // fresh, and the second comes from what the kernel kept.
    let check = |a: &[&str], b: &[&str]| {
        for _ in 0..2 {
            assert_eq!(names("a"), a);
            assert_eq!(names("b"), b);
        }
    };
    check(&["one.txt"], &[]);
    std::fs::write(ws.join("a/two.txt"), "2\n").unwrap();
    check(&["one.txt", "two.txt"], &[]);
    std::fs::create_dir(ws.join("a/sub")).unwrap();
    check(&["one.txt", "sub", "two.txt"], &[]);
    std::fs::rename(ws.join("a/two.txt"), ws.join("a/three.txt")).unwrap();
    check(&["one.txt", "sub", "three.txt"], &[]);
    std::fs::rename(ws.join("a/three.txt"), ws.join("b/three.txt")).unwrap();
    check(&["one.txt", "sub"], &["three.txt"]);
    // The Mac mount can't make hard links (#93); a copy adds the same entry there.
    if cfg!(target_os = "linux") {
        std::fs::hard_link(ws.join("a/one.txt"), ws.join("b/linked.txt")).unwrap();
    } else {
        std::fs::copy(ws.join("a/one.txt"), ws.join("b/linked.txt")).unwrap();
    }
    check(&["one.txt", "sub"], &["linked.txt", "three.txt"]);
    std::os::unix::fs::symlink("one.txt", ws.join("a/pointer")).unwrap();
    check(&["one.txt", "pointer", "sub"], &["linked.txt", "three.txt"]);
    std::fs::rename(ws.join("a/sub"), ws.join("b/sub")).unwrap();
    check(&["one.txt", "pointer"], &["linked.txt", "sub", "three.txt"]);
    std::fs::remove_file(ws.join("a/one.txt")).unwrap();
    std::fs::remove_dir(ws.join("b/sub")).unwrap();
    check(&["pointer"], &["linked.txt", "three.txt"]);

    // A big folder, listed in several pages, then a listing given up after a few
    // entries, then whole listings again, around a change.
    let big: Vec<String> = (0..1000).map(|i| format!("file-with-a-long-name-{i:04}.txt")).collect();
    assert_eq!(names("big"), big);
    assert_eq!(std::fs::read_dir(ws.join("big")).unwrap().take(3).count(), 3);
    assert_eq!(names("big"), big);
    std::fs::remove_file(ws.join("big/file-with-a-long-name-0500.txt")).unwrap();
    let fewer: Vec<String> = big.iter().filter(|name| !name.ends_with("0500.txt")).cloned().collect();
    assert_eq!(names("big"), fewer);
    assert_eq!(names("big"), fewer);
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
fn sandfs_never_runs_what_a_mounts_git_settings_name() {
    if !fuse_available() {
        return;
    }
    // Remote local copies are named after the URL, which can't have parts starting with '.'.
    let data = tempfile::Builder::new().prefix("sandfs-settings").tempdir().unwrap();
    let remote = data.path().join("remote");
    std::fs::create_dir(&remote).unwrap();
    git(&remote, &["init", "-q"]);
    git(&remote, &["config", "uploadpack.allowFilter", "true"]);
    std::fs::write(remote.join("notes.txt"), "notes\n").unwrap();
    git(&remote, &["add", "."]);
    git(&remote, &["commit", "-q", "-m", "first"]);
    let url = format!("file://{}", remote.display());
    let mut sandfs = Sandfs::new();
    let work = data.path().join("work");
    sandfs.mount(&["mount", &format!("git:{url}"), work.to_str().unwrap()], &work);

    // What a program writing to the mount could put in its `.git/config`: the remote
    // rewritten to a program that leaves a marker.
    let marker = data.path().join("ran");
    git(&work, &["config", "protocol.ext.allow", "always"]);
    let program = format!("ext::sh -c touch% {}% >&2;false", marker.display());
    git(&work, &["config", &format!("url.{program}.insteadOf"), &url]);

    // Started in the mount, sandfs asks the server for the branch from no repository.
    let other = data.path().join("other");
    let output = Command::new(env!("CARGO_BIN_EXE_sandfs"))
        .current_dir(&work)
        .env("SANDFS_HOME", sandfs.home.path())
        .args(["mount", &format!("git:{url}"), other.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    sandfs.mounts.push(other.clone());
    assert!(!marker.exists(), "sandfs ran git with the mount's settings");

    // Nor does it take the mount's repository as a source: a fork copies it.
    let output =
        sandfs.try_run(&["mount", &format!("git:{}", work.display()), data.path().join("x").to_str().unwrap()]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("sandfs fork work DIR"));
    assert!(!marker.exists());
    sandfs.unmount(&other);
    sandfs.unmount(&work);
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

#[test]
fn changes_copy_to_brings_files_links_and_modes_out_of_a_mount() {
    if !fuse_available() {
        return;
    }
    use std::os::unix::fs::PermissionsExt;

    let mut sandfs = Sandfs::new();
    let (data, out) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let source = data.path().join("site");
    std::fs::create_dir_all(source.join("old")).unwrap();
    std::fs::write(source.join("old/page.html"), "old\n").unwrap();
    let dir = data.path().join("ws");
    sandfs.mount(&["mount", source.to_str().unwrap(), dir.to_str().unwrap()], &dir);

    std::fs::write(dir.join("build.sh"), "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(dir.join("build.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("build.sh", dir.join("run")).unwrap();
    std::fs::remove_dir_all(dir.join("old")).unwrap();
    std::fs::write(dir.join("old"), "a file where a folder was\n").unwrap();

    let kept = out.path().join("site");
    std::fs::create_dir_all(kept.join("old")).unwrap();
    std::fs::write(kept.join("old/page.html"), "old\n").unwrap();
    let listed = sandfs.stdout(&["changes", "ws", "--copy-to", kept.to_str().unwrap()]);
    assert_eq!(listed, "A build.sh\nA old\nD old/page.html\nA run\n");
    assert_eq!(std::fs::read_to_string(kept.join("old")).unwrap(), "a file where a folder was\n");
    assert_ne!(std::fs::metadata(kept.join("build.sh")).unwrap().permissions().mode() & 0o111, 0);
    assert_eq!(std::fs::read_link(kept.join("run")).unwrap(), Path::new("build.sh"));
    assert!(source.join("old/page.html").exists(), "the source is untouched");
}

#[test]
fn changes_points_out_git_settings_that_run_programs() {
    if !fuse_available() {
        return;
    }
    let mut sandfs = Sandfs::new();
    let data = tempfile::tempdir().unwrap();
    let repo = data.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    std::fs::write(repo.join("notes.md"), "draft\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "first"]);
    let dir = data.path().join("ws");
    sandfs.mount(&["mount", &format!("git:{}", repo.display()), dir.to_str().unwrap()], &dir);

    // An ordinary commit changes no settings, so there is nothing to point out.
    std::fs::write(dir.join("notes.md"), "final\n").unwrap();
    git(&dir, &["commit", "-qam", "second"]);
    let quiet = sandfs.run(&["changes", "ws"]);
    assert_eq!(String::from_utf8_lossy(&quiet.stdout), "M notes.md\n");
    assert_eq!(String::from_utf8_lossy(&quiet.stderr), "");

    let config = std::fs::read_to_string(dir.join(".git/config")).unwrap();
    std::fs::write(dir.join(".git/config"), config + "[core]\n\tpager = touch /tmp/owned\n").unwrap();
    std::fs::create_dir_all(dir.join(".git/hooks")).unwrap();
    std::fs::write(dir.join(".git/hooks/pre-commit"), "#!/bin/sh\n").unwrap();
    let loud = sandfs.run(&["changes", "ws"]);
    assert_eq!(String::from_utf8_lossy(&loud.stdout), "M notes.md\n", "the listing itself still leaves .git out");
    let warned = String::from_utf8_lossy(&loud.stderr);
    assert!(warned.contains(".git/config changed") && warned.contains(".git/hooks/pre-commit changed"), "{warned}");
}

#[test]
fn read_only_paths_can_be_read_but_not_changed_through_a_mount() {
    if !fuse_available() {
        return;
    }
    use std::os::unix::fs::PermissionsExt;

    let mut sandfs = Sandfs::new();
    let data = tempfile::tempdir().unwrap();
    let source = data.path().join("books");
    std::fs::create_dir_all(source.join("contracts")).unwrap();
    std::fs::write(source.join("contracts/lease.md"), "12 months\n").unwrap();
    std::fs::write(source.join("budget.csv"), "jan\n").unwrap();
    let dir = data.path().join("ws");
    let (src, at) = (source.to_str().unwrap(), dir.to_str().unwrap());
    sandfs.mount(&["mount", src, at, "--read-only", "contracts"], &dir);

    let lease = dir.join("contracts/lease.md");
    assert_eq!(std::fs::read_to_string(&lease).unwrap(), "12 months\n");
    assert_eq!(std::fs::metadata(&lease).unwrap().permissions().mode() & 0o222, 0, "shown without write bits");
    // EROFS is 30 on Linux and macOS alike.
    let erofs = |result: std::io::Result<()>| assert_eq!(result.unwrap_err().raw_os_error(), Some(30));
    erofs(std::fs::write(&lease, "6 months\n"));
    erofs(std::fs::OpenOptions::new().append(true).open(&lease).and_then(|mut f| {
        use std::io::Write;
        f.write_all(b"more\n")
    }));
    erofs(std::fs::remove_file(&lease));
    erofs(std::fs::write(dir.join("contracts/new.md"), "x"));
    erofs(std::fs::rename(dir.join("budget.csv"), dir.join("contracts/budget.csv")));
    erofs(std::fs::remove_dir_all(dir.join("contracts")));
    std::fs::write(dir.join("budget.csv"), "feb\n").unwrap();
    assert_eq!(sandfs.stdout(&["changes", "ws"]), "M budget.csv\n");

    // Forks keep it, and it can't be added to a workspace that exists.
    let fork = data.path().join("fork");
    sandfs.mount(&["fork", at, fork.to_str().unwrap()], &fork);
    erofs(std::fs::write(fork.join("contracts/lease.md"), "6 months\n"));
    sandfs.unmount(&dir);
    let again = sandfs.try_run(&["mount", "ws", at, "--read-only", "budget.csv"]);
    assert!(String::from_utf8_lossy(&again.stderr).contains("applies when a workspace is created"));
    assert_eq!(std::fs::read_to_string(source.join("contracts/lease.md")).unwrap(), "12 months\n");
}

/// A kept listing names each entry's inode. When the kernel forgets entries, a later
/// lookup gives them new inodes, and the listing must follow. Forgetting is forced by
/// dropping the kernel's caches, which only root may do; elsewhere this has nothing to
/// test.
#[cfg(target_os = "linux")]
#[test]
fn kept_listings_name_the_inodes_stat_reports() {
    use std::os::unix::fs::MetadataExt;
    if !fuse_available() || std::fs::OpenOptions::new().write(true).open("/proc/sys/vm/drop_caches").is_err() {
        return;
    }
    let folder = tempfile::tempdir().unwrap();
    for i in 0..3 {
        std::fs::write(folder.path().join(format!("f{i}")), "x").unwrap();
    }
    let mut sandfs = Sandfs::new();
    let dirs = tempfile::tempdir().unwrap();
    let ws = dirs.path().join("ws");
    sandfs.mount(&["mount", folder.path().to_str().unwrap(), ws.to_str().unwrap()], &ws);
    let inodes = || -> Vec<(u64, u64)> {
        let mut inodes: Vec<(u64, u64)> = std::fs::read_dir(&ws)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (std::os::unix::fs::DirEntryExt::ino(&entry), entry.path().metadata().unwrap().ino())
            })
            .collect();
        inodes.sort();
        inodes
    };
    let before = inodes();
    assert_eq!(inodes(), before, "the second listing is the kept one");
    std::fs::write("/proc/sys/vm/drop_caches", "2\n").unwrap();
    // The kernel's forgets and the notices that follow them arrive on their own time.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let after = inodes();
        if after.iter().all(|(listed, stat)| listed == stat) {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "listings name old inodes: {after:?}");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    sandfs.unmount(&ws);
}

/// Whether `sandfs shell` can confine a program here: macOS has its sandbox built in,
/// Linux needs bubblewrap.
fn confinement_available() -> bool {
    let available = cfg!(target_os = "macos") || Path::new("/usr/bin/bwrap").exists();
    if !available {
        assert!(std::env::var_os("SANDFS_REQUIRE_FUSE").is_none(), "bubblewrap is required but missing");
        eprintln!("skipping: bubblewrap is not installed");
    }
    available
}

#[test]
fn git_works_in_a_shell_on_a_git_workspace() {
    if !fuse_available() || !confinement_available() {
        return;
    }
    let mut sandfs = Sandfs::new();
    let data = tempfile::tempdir().unwrap();
    let repo = data.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    std::fs::write(repo.join("notes.md"), "draft\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "first"]);
    let dir = data.path().join("ws");
    sandfs.mount(&["mount", &format!("git:{}", repo.display()), dir.to_str().unwrap()], &dir);

    // The repository is hidden, but its objects, which the mount's .git reads, are not.
    let script = r#"
        cat "$REPO/notes.md" 2>/dev/null && exit 10
        echo final > notes.md
        git log --format=%s && git status --short || exit 11
        git -c user.name=t -c user.email=t@example.com commit -qam second || exit 12
        git log --format=%s
    "#;
    let run = sandfs.stdout(&["shell", "ws", "--", "sh", "-c", &script.replace("$REPO", repo.to_str().unwrap())]);
    assert_eq!(run, "first\n M notes.md\nsecond\nfirst\n");
    assert_eq!(sandfs.stdout(&["changes", "ws"]), "M notes.md\n");
}

#[test]
fn a_shell_sees_its_mount_and_nothing_else() {
    if !fuse_available() || !confinement_available() {
        return;
    }
    let mut sandfs = Sandfs::new();
    let data = tempfile::tempdir().unwrap();
    let source = data.path().join("books");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("budget.csv"), "jan\n").unwrap();
    std::fs::write(data.path().join("secret.txt"), "key\n").unwrap();
    let dir = data.path().join("ws");
    sandfs.mount(&["mount", source.to_str().unwrap(), dir.to_str().unwrap()], &dir);

    let script = r#"
        cat budget.csv && echo feb > budget.csv && echo hello > notes.md || exit 10
        cat "$OUTSIDE/secret.txt" 2>/dev/null && exit 11
        ls "$OUTSIDE/books" 2>/dev/null && exit 12
        case "$HOME" in "$PWD"*) exit 13;; esac
        echo cache > "$HOME/.cache" || exit 14
        [ -z "$SANDFS_TEST_TOKEN" ] || exit 15
        if [ -e /proc/net/dev ]; then
            [ "$(grep -c : /proc/net/dev)" = 1 ] || exit 16
        fi
        exit 7
    "#;
    let outside = data.path().canonicalize().unwrap();
    let run = Command::new(env!("CARGO_BIN_EXE_sandfs"))
        .env("SANDFS_HOME", sandfs.home.path())
        .env("SANDFS_TEST_TOKEN", "abc")
        .args(["shell", "ws", "--", "sh", "-c", &script.replace("$OUTSIDE", outside.to_str().unwrap())])
        .output()
        .unwrap();
    assert_eq!(run.status.code(), Some(7), "stderr: {}", String::from_utf8_lossy(&run.stderr));
    assert_eq!(String::from_utf8_lossy(&run.stdout), "jan\n");
    assert_eq!(sandfs.stdout(&["changes", "ws"]), "M budget.csv\nA notes.md\n");
    assert_eq!(std::fs::read_to_string(source.join("budget.csv")).unwrap(), "jan\n");

    // The mount's folder works as well as its name, and only --network reaches a server
    // on this machine.
    let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let connect = format!("exec 3<>/dev/tcp/127.0.0.1/{} || exit 9", server.local_addr().unwrap().port());
    let dir_arg = dir.to_str().unwrap();
    assert_eq!(sandfs.try_run(&["shell", dir_arg, "--", "/bin/bash", "-c", &connect]).status.code(), Some(9));
    sandfs.run(&["shell", dir_arg, "--network", "--", "/bin/bash", "-c", &connect]);
    // HOME would land in the workspace with a temporary folder inside the mount.
    let inside = Command::new(env!("CARGO_BIN_EXE_sandfs"))
        .env("SANDFS_HOME", sandfs.home.path())
        .env("TMPDIR", &dir)
        .args(["shell", "ws", "--", "true"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&inside.stderr).contains("set TMPDIR to another folder"));
    sandfs.unmount(&dir);
    let unmounted = sandfs.try_run(&["shell", "ws", "--", "true"]);
    assert!(String::from_utf8_lossy(&unmounted.stderr).contains("ws is not mounted"));
}

#[test]
fn update_moves_a_git_workspace_onto_a_newer_commit() {
    if !fuse_available() {
        return;
    }
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q"]);
    for (name, text) in [("a.txt", "a0\n"), ("b.txt", "b0\n"), ("keep.txt", "k\n")] {
        std::fs::write(repo.path().join(name), text).unwrap();
    }
    git(repo.path(), &["add", "-A"]);
    git(repo.path(), &["commit", "-q", "-m", "first"]);

    let mut sandfs = Sandfs::new();
    let dirs = tempfile::tempdir().unwrap();
    let main = dirs.path().join("main");
    let source = format!("git:{}", repo.path().display());
    sandfs.mount(&["mount", &source, main.to_str().unwrap()], &main);
    std::fs::write(main.join("a.txt"), "mine\n").unwrap();
    assert_eq!(git_out(&main, &["status", "--porcelain"]), " M a.txt\n");

    // A pull in the workspace brings upstream files into its own changes.
    std::fs::write(repo.path().join("b.txt"), "b1\n").unwrap();
    git(repo.path(), &["commit", "-q", "-am", "second"]);
    git_out(&main, &["pull", "-q", "--ff-only", "origin", "main"]);
    // Upstream moves on again, the pulled file among the rest.
    std::fs::write(repo.path().join("b.txt"), "b2\n").unwrap();
    std::fs::write(repo.path().join("c.txt"), "c\n").unwrap();
    git(repo.path(), &["add", "b.txt", "c.txt"]);
    git(repo.path(), &["commit", "-q", "-m", "third"]);
    let third = git_out(repo.path(), &["rev-parse", "HEAD"]);

    sandfs.unmount(&main);
    // Seconds apart, so the times files report tell the move from the mount.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let moved = sandfs.run(&["update", "main"]);
    assert_eq!(String::from_utf8_lossy(&moved.stdout), "", "nothing changed on both sides");
    assert!(String::from_utf8_lossy(&moved.stderr).contains("1 of its changes matched"));
    assert_eq!(sandfs.stdout(&["changes", "main"]), "M a.txt\n");

    sandfs.mount(&["mount", "main", main.to_str().unwrap()], &main);
    assert_eq!(git_out(&main, &["rev-parse", "HEAD"]), third);
    assert_eq!(git_out(&main, &["status", "--porcelain"]), " M a.txt\n");
    assert_eq!(std::fs::read_to_string(main.join("c.txt")).unwrap(), "c\n");
    assert_eq!(std::fs::read_to_string(main.join("b.txt")).unwrap(), "b2\n", "what was pulled isn't the workspace's");
    let mtime = |name: &str| std::fs::metadata(main.join(name)).unwrap().modified().unwrap();
    assert!(mtime("c.txt") > mtime("keep.txt"), "files the move changed look changed to build tools");
    assert_eq!(mtime("b.txt"), mtime("c.txt"));
}

#[test]
fn update_moves_a_mounted_workspace_in_place() {
    if !fuse_available() {
        return;
    }
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q"]);
    std::fs::create_dir_all(repo.path().join("sub")).unwrap();
    std::fs::create_dir_all(repo.path().join("old")).unwrap();
    for (name, text) in
        [("a.txt", "a0\n"), ("b.txt", "b0\n"), ("gone.txt", "gone\n"), ("sub/s.txt", "s0\n"), ("old/x.txt", "x\n")]
    {
        std::fs::write(repo.path().join(name), text).unwrap();
    }
    git(repo.path(), &["add", "-A"]);
    git(repo.path(), &["commit", "-q", "-m", "first"]);

    let mut sandfs = Sandfs::new();
    let dirs = tempfile::tempdir().unwrap();
    let main = dirs.path().join("main");
    let source = format!("git:{}", repo.path().display());
    sandfs.mount(&["mount", &source, main.to_str().unwrap()], &main);
    std::fs::write(main.join("a.txt"), "mine\n").unwrap();
    // A shell sits in a folder of the mount, a file stays open, and the kernel holds
    // what was looked at: names, sizes, contents, a name found missing, listings.
    let mut shell = Command::new("sleep").arg("60").current_dir(main.join("sub")).spawn().unwrap();
    let held = std::fs::File::open(main.join("gone.txt")).unwrap();
    assert_eq!(std::fs::read_to_string(main.join("b.txt")).unwrap(), "b0\n");
    assert_eq!(std::fs::read_to_string(main.join("sub/s.txt")).unwrap(), "s0\n");
    assert_eq!(std::fs::read_to_string(main.join("old/x.txt")).unwrap(), "x\n");
    assert!(!main.join("c.txt").exists());
    let names = |dir: &Path| {
        let mut names: Vec<String> =
            std::fs::read_dir(dir).unwrap().map(|entry| entry.unwrap().file_name().into_string().unwrap()).collect();
        names.sort();
        names
    };
    assert_eq!(names(&main), [".git", "a.txt", "b.txt", "gone.txt", "old", "sub"]);
    assert_eq!(git_out(&main, &["status", "--porcelain"]), " M a.txt\n");

    std::fs::write(repo.path().join("b.txt"), "b1, longer\n").unwrap();
    std::fs::write(repo.path().join("c.txt"), "c\n").unwrap();
    std::fs::write(repo.path().join("sub/s.txt"), "s1\n").unwrap();
    std::fs::remove_file(repo.path().join("gone.txt")).unwrap();
    std::fs::remove_dir_all(repo.path().join("old")).unwrap();
    std::fs::write(repo.path().join("old"), "a file now\n").unwrap();
    git(repo.path(), &["add", "-A"]);
    git(repo.path(), &["commit", "-q", "-m", "second"]);
    let second = git_out(repo.path(), &["rev-parse", "HEAD"]);

    let moved = sandfs.run(&["update", "main"]);
    assert_eq!(String::from_utf8_lossy(&moved.stdout), "", "nothing changed on both sides");
    assert!(String::from_utf8_lossy(&moved.stderr).contains("moved main"));

    assert_eq!(std::fs::read_to_string(main.join("b.txt")).unwrap(), "b1, longer\n");
    assert_eq!(std::fs::read_to_string(main.join("c.txt")).unwrap(), "c\n");
    assert_eq!(std::fs::read_to_string(main.join("sub/s.txt")).unwrap(), "s1\n");
    assert_eq!(std::fs::read_to_string(main.join("old")).unwrap(), "a file now\n");
    assert!(!main.join("gone.txt").exists());
    assert_eq!(names(&main), [".git", "a.txt", "b.txt", "c.txt", "old", "sub"]);
    assert_eq!(std::fs::read_to_string(main.join("a.txt")).unwrap(), "mine\n", "the workspace's own change stays");
    assert_eq!(git_out(&main, &["rev-parse", "HEAD"]), second);
    assert_eq!(git_out(&main, &["status", "--porcelain"]), " M a.txt\n");
    assert_eq!(sandfs.stdout(&["changes", "main"]), "M a.txt\n");
    // The shell is still where it was, and a file left open reads as it was.
    let inside = Command::new("cat").arg("s.txt").current_dir(main.join("sub")).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&inside.stdout), "s1\n");
    #[cfg(target_os = "linux")]
    {
        let cwd = std::fs::read_link(format!("/proc/{}/cwd", shell.id())).unwrap();
        assert_eq!(cwd, main.join("sub").canonicalize().unwrap(), "the shell's folder is still there");
    }
    let mut was = String::new();
    std::io::Read::read_to_string(&mut &held, &mut was).unwrap();
    assert_eq!(was, "gone\n");
    drop(held);
    shell.kill().unwrap();
    shell.wait().unwrap();

    // Written on, it saves as ever, and comes back the same once mounted again.
    std::fs::write(main.join("c.txt"), "c, mine\n").unwrap();
    sandfs.unmount(&main);
    assert_eq!(sandfs.stdout(&["changes", "main"]), "M a.txt\nM c.txt\n");
    sandfs.mount(&["mount", "main", main.to_str().unwrap()], &main);
    assert_eq!(std::fs::read_to_string(main.join("b.txt")).unwrap(), "b1, longer\n");
    assert_eq!(git_out(&main, &["status", "--porcelain"]), " M a.txt\n M c.txt\n");
    sandfs.unmount(&main);
}

#[test]
fn update_moves_a_mounted_folder_workspace_onto_another_folder() {
    if !fuse_available() {
        return;
    }
    let old = tempfile::tempdir().unwrap();
    let new = tempfile::tempdir().unwrap();
    for (dir, files) in [
        (old.path(), &[("same.txt", "same\n"), ("f.txt", "f0\n"), ("gone.txt", "gone\n")][..]),
        (new.path(), &[("same.txt", "same\n"), ("f.txt", "f1, longer\n"), ("new.txt", "new\n")][..]),
    ] {
        for (name, text) in files {
            std::fs::write(dir.join(name), text).unwrap();
        }
    }
    let mut sandfs = Sandfs::new();
    let dirs = tempfile::tempdir().unwrap();
    let ws = dirs.path().join("ws");
    sandfs.mount(&["mount", old.path().to_str().unwrap(), ws.to_str().unwrap(), "--name", "ws"], &ws);
    std::fs::write(ws.join("mine.txt"), "mine\n").unwrap();
    assert_eq!(std::fs::read_to_string(ws.join("f.txt")).unwrap(), "f0\n");
    assert!(!ws.join("new.txt").exists());

    sandfs.run(&["update", "ws", new.path().to_str().unwrap()]);
    assert_eq!(std::fs::read_to_string(ws.join("f.txt")).unwrap(), "f1, longer\n");
    assert_eq!(std::fs::read_to_string(ws.join("new.txt")).unwrap(), "new\n");
    assert_eq!(std::fs::read_to_string(ws.join("mine.txt")).unwrap(), "mine\n");
    assert!(!ws.join("gone.txt").exists());
    assert_eq!(sandfs.stdout(&["changes", "ws"]), "A mine.txt\n");
    sandfs.unmount(&ws);
}

#[test]
fn update_follows_a_pull_in_a_mount_of_a_url() {
    if !fuse_available() {
        return;
    }
    let data = tempfile::Builder::new().prefix("data").tempdir().unwrap();
    let repo = data.path().join("site");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    for (name, text) in [("a.txt", "a0\n"), ("b.txt", "b0\n")] {
        std::fs::write(repo.join(name), text).unwrap();
    }
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "first"]);

    let mut sandfs = Sandfs::new();
    let dirs = tempfile::tempdir().unwrap();
    let main = dirs.path().join("main");
    let url = format!("file://{}", repo.display());
    sandfs.mount(&["mount", &url, main.to_str().unwrap(), "--name", "main"], &main);
    std::fs::write(main.join("a.txt"), "mine\n").unwrap();

    // A pull in the mount, its branches then packed as `git gc` would.
    std::fs::write(repo.join("b.txt"), "b1\n").unwrap();
    git(&repo, &["commit", "-q", "-am", "second"]);
    git_out(&main, &["pull", "-q", "--ff-only", "origin", "main"]);
    git_out(&main, &["pack-refs", "--all"]);
    // Upstream undoes the pulled change and moves on; the local copy of the URL has
    // only the commits mounted, so where the pulled one is comes from fetching more.
    std::fs::write(repo.join("b.txt"), "b0\n").unwrap();
    std::fs::write(repo.join("c.txt"), "c\n").unwrap();
    git(&repo, &["add", "b.txt", "c.txt"]);
    git(&repo, &["commit", "-q", "-m", "third"]);
    let third = git_out(&repo, &["rev-parse", "HEAD"]);

    sandfs.unmount(&main);
    let moved = sandfs.run(&["update", "main"]);
    assert_eq!(String::from_utf8_lossy(&moved.stdout), "", "nothing changed on both sides");
    assert_eq!(sandfs.stdout(&["changes", "main"]), "M a.txt\n");

    sandfs.mount(&["mount", "main", main.to_str().unwrap()], &main);
    assert_eq!(git_out(&main, &["rev-parse", "HEAD"]), third);
    assert_eq!(git_out(&main, &["status", "--porcelain"]), " M a.txt\n");
    assert_eq!(std::fs::read_to_string(main.join("b.txt")).unwrap(), "b0\n", "upstream undid what was pulled");
    // It changed back, so build tools see it as changed, as the files the move brought.
    let mtime = |name: &str| std::fs::metadata(main.join(name)).unwrap().modified().unwrap();
    assert_eq!(mtime("b.txt"), mtime("c.txt"));
    sandfs.unmount(&main);
}

#[test]
fn update_takes_as_pulled_only_what_head_checked_out() {
    if !fuse_available() {
        return;
    }
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q"]);
    std::fs::write(repo.path().join("x.txt"), "a\n").unwrap();
    git(repo.path(), &["add", "-A"]);
    git(repo.path(), &["commit", "-q", "-m", "first"]);

    let mut sandfs = Sandfs::new();
    let dirs = tempfile::tempdir().unwrap();
    let (one, two) = (dirs.path().join("one"), dirs.path().join("two"));
    let source = format!("git:{}", repo.path().display());
    sandfs.mount(&["mount", &source, one.to_str().unwrap()], &one);
    sandfs.mount(&["mount", &source, two.to_str().unwrap()], &two);

    // Upstream changes x.txt and adds a folder.
    std::fs::write(repo.path().join("x.txt"), "b\n").unwrap();
    std::fs::create_dir(repo.path().join("dir")).unwrap();
    std::fs::write(repo.path().join("dir/f"), "f\n").unwrap();
    git(repo.path(), &["add", "-A"]);
    git(repo.path(), &["commit", "-q", "-m", "second"]);
    // One fetches it to a branch it doesn't check out, and makes x.txt the same by hand.
    // A branch of its own, packed, is reset to the mounted commit since.
    git_out(&one, &["fetch", "-q", "origin", "main"]);
    git_out(&one, &["branch", "other", "FETCH_HEAD"]);
    std::fs::write(one.join("x.txt"), "b\n").unwrap();
    git_out(&one, &["checkout", "-q", "-b", "own"]);
    git_out(&one, &["commit", "-q", "--allow-empty", "-m", "own"]);
    git_out(&one, &["pack-refs", "--all"]);
    git_out(&one, &["checkout", "-q", "main"]);
    git_out(&one, &["branch", "-f", "own", "main"]);
    // Two pulls it.
    git_out(&two, &["pull", "-q", "--ff-only", "origin", "main"]);
    // Upstream changes x.txt again, and makes the folder a file.
    std::fs::write(repo.path().join("x.txt"), "c\n").unwrap();
    std::fs::remove_dir_all(repo.path().join("dir")).unwrap();
    std::fs::write(repo.path().join("dir"), "now a file\n").unwrap();
    git(repo.path(), &["add", "-A"]);
    git(repo.path(), &["commit", "-q", "-m", "third"]);
    sandfs.unmount(&one);
    sandfs.unmount(&two);

    // One's x.txt is its own edit, whatever another branch names.
    let moved = sandfs.run(&["update", "one"]);
    assert_eq!(String::from_utf8_lossy(&moved.stdout), "C x.txt\n");
    assert_eq!(sandfs.stdout(&["changes", "one"]), "M x.txt\n");
    // Everything two has over the old commit, the folder too, was pulled.
    let moved = sandfs.run(&["update", "two"]);
    assert_eq!(String::from_utf8_lossy(&moved.stdout), "", "nothing changed on both sides");
    assert_eq!(sandfs.stdout(&["changes", "two"]), "");
    sandfs.mount(&["mount", "two", two.to_str().unwrap()], &two);
    assert_eq!(std::fs::read_to_string(two.join("dir")).unwrap(), "now a file\n");
    assert_eq!(git_out(&two, &["status", "--porcelain"]), "");
    sandfs.unmount(&two);
}

/// Hard links need the FUSE mount; the NFS server under the macOS mount has no LINK.
#[cfg(target_os = "linux")]
#[test]
fn update_keeps_a_hard_link_the_newer_source_has_the_bytes_of() {
    if !fuse_available() {
        return;
    }
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q"]);
    std::fs::write(repo.path().join("a.txt"), "a0\n").unwrap();
    git(repo.path(), &["add", "-A"]);
    git(repo.path(), &["commit", "-q", "-m", "first"]);

    let mut sandfs = Sandfs::new();
    let dirs = tempfile::tempdir().unwrap();
    let main = dirs.path().join("main");
    let source = format!("git:{}", repo.path().display());
    sandfs.mount(&["mount", &source, main.to_str().unwrap()], &main);
    std::fs::hard_link(main.join("a.txt"), main.join("b.txt")).unwrap();
    sandfs.unmount(&main);

    // Upstream changes a.txt and adds a b.txt of its own with a.txt's old bytes.
    std::fs::write(repo.path().join("a.txt"), "a1\n").unwrap();
    std::fs::write(repo.path().join("b.txt"), "a0\n").unwrap();
    git(repo.path(), &["add", "-A"]);
    git(repo.path(), &["commit", "-q", "-m", "second"]);
    let refused = sandfs.try_run(&["update", "main"]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("hard link"), "{refused:?}");
}

/// A file whose contents come from a slow server: the first read waits for them, however
/// long the fetch takes, instead of failing.
#[test]
fn a_slow_first_read_waits_for_the_fetch() {
    if !fuse_available() {
        return;
    }
    let remote = tempfile::Builder::new().prefix("sandfs-remote").tempdir().unwrap();
    let remote = remote.path();
    git(remote, &["init", "-q"]);
    git(remote, &["config", "uploadpack.allowFilter", "true"]);
    std::fs::write(remote.join("README.md"), "hello\n").unwrap();
    git(remote, &["add", "-A"]);
    git(remote, &["commit", "-q", "-m", "first"]);

    // Every fetch from the remote takes 20 s, longer than an NFS client waits for an
    // answer before it tries again.
    let config = tempfile::tempdir().unwrap();
    let hook = config.path().join("slow");
    std::fs::write(&hook, "#!/bin/sh\nsleep 20\nexec \"$@\"\n").unwrap();
    std::fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let global = config.path().join("gitconfig");
    std::fs::write(&global, format!("[uploadpack]\n\tpackObjectsHook = {}\n", hook.display())).unwrap();

    let mut sandfs = Sandfs::new();
    let dirs = tempfile::tempdir().unwrap();
    let work = dirs.path().join("work");
    let source = format!("git:file://{}", remote.display());
    // The mount process keeps the environment of the command that starts it.
    let mounted = Command::new(env!("CARGO_BIN_EXE_sandfs"))
        .env("SANDFS_HOME", sandfs.home.path())
        .env("GIT_CONFIG_GLOBAL", &global)
        .args(["mount", &source, work.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(mounted.status.success(), "{}", String::from_utf8_lossy(&mounted.stderr));
    sandfs.mounts.push(work.clone());

    let started = std::time::Instant::now();
    assert_eq!(std::fs::read_to_string(work.join("README.md")).unwrap(), "hello\n");
    assert!(started.elapsed() >= std::time::Duration::from_secs(15), "the read fetched: {:?}", started.elapsed());
}
