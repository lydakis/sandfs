//! The `sandfs` binary without a mount, as on macOS and Windows.

use std::io::Write;
use std::process::{Command, Output, Stdio};

fn sandfs(home: &std::path::Path, args: &[&str], input: &str) -> Output {
    sandfs_in(home, &std::env::current_dir().unwrap(), args, input)
}

fn sandfs_in(home: &std::path::Path, dir: &std::path::Path, args: &[&str], input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_sandfs"))
        .current_dir(dir)
        .env("SANDFS_HOME", home)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
    child.wait_with_output().unwrap()
}

fn stdout(home: &std::path::Path, args: &[&str]) -> String {
    let output = sandfs(home, args, "");
    assert!(output.status.success(), "sandfs {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn serve_creates_a_workspace_over_a_source() {
    let (home, data) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let folder = data.path().join("reports");
    std::fs::create_dir(&folder).unwrap();
    std::fs::write(folder.join("q3.txt"), "draft\n").unwrap();
    let source = folder.to_str().unwrap();

    let write = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"write","arguments":{"path":"/mnt/q4.txt","content":"next\n"}}}"#;
    let served = sandfs(home.path(), &["serve", "--mcp", source], &format!("{write}\n"));
    assert!(served.status.success(), "{}", String::from_utf8_lossy(&served.stderr));
    assert!(String::from_utf8_lossy(&served.stdout).contains(r#""isError":false"#));

    // Named after the folder, and served again by that name.
    assert!(stdout(home.path(), &["list"]).starts_with("reports\t"));
    assert_eq!(stdout(home.path(), &["changes", "reports"]), "A q4.txt\n");
    let again = sandfs(home.path(), &["serve", "--mcp", "reports"], "");
    assert!(again.status.success(), "{}", String::from_utf8_lossy(&again.stderr));
    assert!(!folder.join("q4.txt").exists(), "the source folder is untouched");

    let named = sandfs(home.path(), &["serve", "--mcp", source, "--name", "copy"], "");
    assert!(named.status.success(), "{}", String::from_utf8_lossy(&named.stderr));
    assert_eq!(stdout(home.path(), &["changes", "copy"]), "");

    // Two workspaces over one source compare directly.
    let write = write.replace("q4.txt", "q5.txt");
    let other = sandfs(home.path(), &["serve", "--mcp", "copy"], &format!("{write}\n"));
    assert!(other.status.success(), "{}", String::from_utf8_lossy(&other.stderr));
    assert_eq!(stdout(home.path(), &["changes", "reports", "--since", "copy"]), "A q4.txt\nD q5.txt\n");
    let elsewhere = sandfs(home.path(), &["serve", "--mcp", data.path().to_str().unwrap(), "--name", "data"], "");
    assert!(elsewhere.status.success(), "{}", String::from_utf8_lossy(&elsewhere.stderr));
    let mismatched = sandfs(home.path(), &["changes", "reports", "--since", "data"], "");
    assert!(String::from_utf8_lossy(&mismatched.stderr).contains("different sources"));
}

#[test]
fn served_sources_are_named_after_their_folder() {
    let (home, data) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let folder = data.path().join("photos");
    std::fs::create_dir(&folder).unwrap();
    let here = sandfs_in(home.path(), &folder, &["serve", "--mcp", "."], "");
    assert!(here.status.success(), "{}", String::from_utf8_lossy(&here.stderr));

    let repo = data.path().join("notes");
    std::fs::create_dir(&repo).unwrap();
    std::fs::write(repo.join("a.txt"), "a\n").unwrap();
    for args in
        [&["init", "-q"][..], &["add", "a.txt"], &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "a"]]
    {
        assert!(Command::new("git").current_dir(&repo).args(args).status().unwrap().success());
    }
    let source = format!("git:{}@HEAD", repo.display());
    let versioned = sandfs(home.path(), &["serve", "--mcp", &source], "");
    assert!(versioned.status.success(), "{}", String::from_utf8_lossy(&versioned.stderr));

    let names: Vec<String> =
        stdout(home.path(), &["list"]).lines().map(|line| line.split('\t').next().unwrap().to_owned()).collect();
    assert_eq!(names, ["notes", "photos"]);
}

#[test]
fn a_reader_that_stops_early_is_not_an_error() {
    let (home, data) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    for name in ["a", "b", "c"] {
        let served = sandfs(home.path(), &["serve", "--mcp", data.path().to_str().unwrap(), "--name", name], "");
        assert!(served.status.success(), "{}", String::from_utf8_lossy(&served.stderr));
    }
    // Like `sandfs list | head -0`: the reader is gone before the first line.
    let mut child = Command::new(env!("CARGO_BIN_EXE_sandfs"))
        .env("SANDFS_HOME", home.path())
        .arg("list")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(output.stderr.is_empty(), "{}", String::from_utf8_lossy(&output.stderr));
}

#[test]
fn a_url_is_a_repository_without_git_prefix() {
    // The local copy is named after the URL's path, which can't hold a hidden folder.
    let home = tempfile::tempdir().unwrap();
    let data = tempfile::Builder::new().prefix("data").tempdir().unwrap();
    let repo = data.path().join("notes");
    std::fs::create_dir(&repo).unwrap();
    std::fs::write(repo.join("a.txt"), "a\n").unwrap();
    for args in
        [&["init", "-q"][..], &["add", "a.txt"], &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "a"]]
    {
        assert!(Command::new("git").current_dir(&repo).args(args).status().unwrap().success());
    }
    let url = format!("file://{}", repo.display());
    let served = sandfs(home.path(), &["serve", "--mcp", &url, "--name", "notes"], "");
    assert!(served.status.success(), "{}", String::from_utf8_lossy(&served.stderr));
    assert_eq!(stdout(home.path(), &["list"]), format!("notes\t{url}\t-\n"));
}

#[test]
fn clean_drops_downloaded_files_and_unused_copies() {
    let home = tempfile::tempdir().unwrap();
    let data = tempfile::Builder::new().prefix("data").tempdir().unwrap();
    let mut urls = Vec::new();
    for name in ["notes", "photos"] {
        let repo = data.path().join(name);
        std::fs::create_dir(&repo).unwrap();
        std::fs::write(repo.join("a.txt"), format!("{name}\n")).unwrap();
        for args in [
            &["init", "-q"][..],
            &["add", "a.txt"],
            &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "a"],
        ] {
            assert!(Command::new("git").current_dir(&repo).args(args).status().unwrap().success());
        }
        urls.push(format!("file://{}", repo.display()));
    }
    let read =
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"read","arguments":{"path":"/mnt/a.txt"}}}"#;
    for (url, name) in urls.iter().zip(["notes", "photos"]) {
        let served = sandfs(home.path(), &["serve", "--mcp", url, "--name", name], &format!("{read}\n"));
        assert!(served.status.success(), "{}", String::from_utf8_lossy(&served.stderr));
    }
    assert!(stdout(home.path(), &["rm", "photos"]).is_empty());

    let copy = |name: &str| {
        let copies = home.path().join("repos");
        // Named after the URL's path, which on Windows is one name with the drive's.
        let named = |path: &std::path::PathBuf| path.to_string_lossy().ends_with(&format!("{name}.git"));
        let found = walkdir(&copies).into_iter().find(named);
        found.unwrap_or_else(|| copies.join("missing"))
    };
    let blobs = |copy: &std::path::Path| {
        let out = Command::new("git")
            .arg("-C")
            .arg(copy)
            .args(["cat-file", "--batch-all-objects", "--batch-check=%(objecttype)"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).lines().filter(|kind| *kind == "blob").count()
    };
    let notes = copy("notes");
    assert!(blobs(&notes) > 0, "reading fetched the file");
    assert!(copy("photos").exists());

    let cleaned = stdout(home.path(), &["clean"]);
    assert!(cleaned.contains("notes.git: ") && cleaned.contains(" freed"), "{cleaned}");
    assert!(cleaned.contains("photos.git: deleted"), "{cleaned}");
    assert!(!copy("photos").exists(), "no workspace uses it");
    assert_eq!(blobs(&notes), 0, "the downloaded file is gone");

    // The workspace still opens, and the file comes back from the repository.
    let served = sandfs(home.path(), &["serve", "--mcp", "notes"], &format!("{read}\n"));
    assert!(served.status.success(), "{}", String::from_utf8_lossy(&served.stderr));
    assert!(
        String::from_utf8_lossy(&served.stdout).contains("notes\\n"),
        "{}",
        String::from_utf8_lossy(&served.stdout)
    );
}

/// Every folder under `root`.
fn walkdir(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let mut folders = vec![root.to_path_buf()];
    while let Some(folder) = folders.pop() {
        for entry in std::fs::read_dir(&folder).into_iter().flatten().flatten() {
            if entry.path().is_dir() {
                folders.push(entry.path());
                found.push(entry.path());
            }
        }
    }
    found
}

fn call(id: u32, tool: &str, arguments: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"{tool}","arguments":{arguments}}}}}"#
    ) + "\n"
}

#[test]
fn changes_copy_to_writes_only_the_changes_asked_for() {
    let (home, data, out) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let folder = data.path().join("books");
    std::fs::create_dir_all(folder.join("2025")).unwrap();
    std::fs::write(folder.join("budget.csv"), "jan,1200\n").unwrap();
    std::fs::write(folder.join("2025/q1.csv"), "q1\n").unwrap();
    std::fs::write(folder.join("2025/q2.csv"), "q2\n").unwrap();
    let source = folder.to_str().unwrap();

    let session = call(1, "write", r#"{"path":"/mnt/budget.csv","content":"jan,1250\n"}"#)
        + &call(2, "write", r#"{"path":"/mnt/notes/summary.md","content":"up 4%\n"}"#)
        + &call(3, "delete", r#"{"path":"/mnt/2025/q1.csv"}"#)
        + &call(4, "delete", r#"{"path":"/mnt/2025/q2.csv"}"#);
    let served = sandfs(home.path(), &["serve", "--mcp", source], &session);
    assert!(served.status.success(), "{}", String::from_utf8_lossy(&served.stderr));

    // A folder holding the original, as a person keeps one.
    let kept = out.path().join("books");
    std::fs::create_dir_all(kept.join("2025")).unwrap();
    for name in ["budget.csv", "2025/q1.csv", "2025/q2.csv"] {
        std::fs::copy(folder.join(name), kept.join(name)).unwrap();
    }
    let dir = kept.to_str().unwrap();

    // Only the summary first: one file, nothing else touched.
    assert_eq!(stdout(home.path(), &["changes", "books", "notes", "--copy-to", dir]), "A notes/summary.md\n");
    assert_eq!(std::fs::read_to_string(kept.join("notes/summary.md")).unwrap(), "up 4%\n");
    assert_eq!(std::fs::read_to_string(kept.join("budget.csv")).unwrap(), "jan,1200\n");
    assert!(kept.join("2025/q1.csv").exists());

    // Then everything. The workspace still has the emptied folder, so the copy keeps it.
    assert_eq!(
        stdout(home.path(), &["changes", "books", "--copy-to", dir]),
        "D 2025/q1.csv\nD 2025/q2.csv\nM budget.csv\nA notes/summary.md\n"
    );
    assert_eq!(std::fs::read_to_string(kept.join("budget.csv")).unwrap(), "jan,1250\n");
    assert_eq!(std::fs::read_dir(kept.join("2025")).unwrap().count(), 0);
    assert_eq!(std::fs::read_to_string(folder.join("budget.csv")).unwrap(), "jan,1200\n", "the source is untouched");
}

#[test]
fn changes_copy_to_removes_folders_a_deletion_emptied_even_when_its_files_are_gone() {
    let (home, data, out) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let folder = data.path().join("books");
    std::fs::create_dir_all(folder.join("2025")).unwrap();
    std::fs::write(folder.join("2025/q1.csv"), "q1\n").unwrap();
    let session = call(1, "delete", r#"{"path":"/mnt/2025/q1.csv"}"#) + &call(2, "delete", r#"{"path":"/mnt/2025"}"#);
    let served = sandfs(home.path(), &["serve", "--mcp", folder.to_str().unwrap()], &session);
    assert!(served.status.success(), "{}", String::from_utf8_lossy(&served.stderr));

    // The file is already gone from the copy, but its folder is left.
    std::fs::create_dir(out.path().join("2025")).unwrap();
    assert_eq!(
        stdout(home.path(), &["changes", "books", "--copy-to", out.path().to_str().unwrap()]),
        "D 2025/q1.csv\n"
    );
    assert_eq!(std::fs::read_dir(out.path()).unwrap().count(), 0, "neither 2025 nor the copy's own folder is left");
}

#[cfg(unix)]
#[test]
fn changes_copy_to_never_writes_through_a_link() {
    let (home, data, out, elsewhere) = (
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    );
    let folder = data.path().join("books");
    std::fs::create_dir(&folder).unwrap();
    let session = call(1, "write", r#"{"path":"/mnt/notes/summary.md","content":"up 4%\n"}"#);
    let served = sandfs(home.path(), &["serve", "--mcp", folder.to_str().unwrap()], &session);
    assert!(served.status.success(), "{}", String::from_utf8_lossy(&served.stderr));

    std::os::unix::fs::symlink(elsewhere.path(), out.path().join("notes")).unwrap();
    let copied = sandfs(home.path(), &["changes", "books", "--copy-to", out.path().to_str().unwrap()], "");
    assert!(!copied.status.success());
    assert!(
        String::from_utf8_lossy(&copied.stderr).contains("is a link"),
        "{}",
        String::from_utf8_lossy(&copied.stderr)
    );
    assert!(!elsewhere.path().join("summary.md").exists());
}

#[test]
fn changes_copy_to_leaves_out_git_folders_in_any_spelling() {
    let (home, data, out) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let folder = data.path().join("books");
    std::fs::create_dir(&folder).unwrap();
    // A Mac or Windows disk would read both as the `.git` of the folder copied into.
    let session = call(1, "write", r#"{"path":"/mnt/.GIT/config","content":"[core]\n\tfsmonitor = evil\n"}"#)
        + &call(2, "write", r#"{"path":"/mnt/GIT~1/hooks/post-checkout","content":"evil\n"}"#)
        + &call(3, "write", r#"{"path":"/mnt/notes.md","content":"fine\n"}"#);
    let served = sandfs(home.path(), &["serve", "--mcp", folder.to_str().unwrap()], &session);
    assert!(served.status.success(), "{}", String::from_utf8_lossy(&served.stderr));

    let copied = sandfs(home.path(), &["changes", "books", "--copy-to", out.path().to_str().unwrap()], "");
    assert!(copied.status.success(), "{}", String::from_utf8_lossy(&copied.stderr));
    assert!(String::from_utf8_lossy(&copied.stderr).contains("left out 2 changes in .git"));
    let mut names: Vec<_> =
        std::fs::read_dir(out.path()).unwrap().map(|entry| entry.unwrap().file_name().into_string().unwrap()).collect();
    names.sort();
    assert_eq!(names, ["notes.md"]);
}

#[test]
fn changes_refuses_paths_outside_the_workspace() {
    let (home, data) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let folder = data.path().join("books");
    std::fs::create_dir(&folder).unwrap();
    let served = sandfs(home.path(), &["serve", "--mcp", folder.to_str().unwrap()], "");
    assert!(served.status.success(), "{}", String::from_utf8_lossy(&served.stderr));
    let outside = sandfs(home.path(), &["changes", "books", ".."], "");
    assert!(!outside.status.success());
    assert!(String::from_utf8_lossy(&outside.stderr).contains("outside the workspace"));
}

#[test]
fn served_workspaces_keep_read_only_paths() {
    let (home, data) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let folder = data.path().join("books");
    std::fs::create_dir_all(folder.join("contracts")).unwrap();
    std::fs::write(folder.join("contracts/lease.md"), "12 months\n").unwrap();

    let session = call(1, "write", r#"{"path":"/mnt/contracts/lease.md","content":"6 months\n"}"#)
        + &call(2, "write", r#"{"path":"/mnt/notes.md","content":"fine\n"}"#);
    let served =
        sandfs(home.path(), &["serve", "--mcp", folder.to_str().unwrap(), "--read-only", "./contracts/"], &session);
    assert!(served.status.success(), "{}", String::from_utf8_lossy(&served.stderr));
    let replies = String::from_utf8_lossy(&served.stdout);
    assert!(replies.contains("/mnt/contracts/lease.md: read-only"), "{replies}");
    assert_eq!(stdout(home.path(), &["changes", "books"]), "A notes.md\n");
}

#[test]
fn read_only_paths_outside_the_source_are_refused() {
    let (home, data) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let folder = data.path().join("books");
    std::fs::create_dir_all(&folder).unwrap();
    let served = sandfs(home.path(), &["serve", "--mcp", folder.to_str().unwrap(), "--read-only", "../contracts"], "");
    assert!(!served.status.success());
    assert!(String::from_utf8_lossy(&served.stderr).contains("--read-only ../contracts is outside the source"));
    assert!(!home.path().join("workspaces/books").exists(), "no workspace was made");
}

/// Run the MCP tools `calls` (name and arguments) against the workspace or source `what`.
fn mcp(home: &std::path::Path, what: &str, calls: &[(&str, serde_json::Value)]) -> String {
    let input: String = calls
        .iter()
        .enumerate()
        .map(|(id, (name, arguments))| {
            let call = serde_json::json!({"jsonrpc": "2.0", "id": id + 1, "method": "tools/call",
                "params": {"name": name, "arguments": arguments}});
            format!("{call}\n")
        })
        .collect();
    let served = sandfs(home, &["serve", "--mcp", what], &input);
    assert!(served.status.success(), "{}", String::from_utf8_lossy(&served.stderr));
    let out = String::from_utf8(served.stdout).unwrap();
    assert!(!out.contains(r#""isError":true"#), "{out}");
    out
}

fn commit_all(repo: &std::path::Path, message: &str) {
    for args in [&["add", "-A"][..], &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", message]] {
        assert!(Command::new("git").current_dir(repo).args(args).status().unwrap().success());
    }
}

#[test]
fn update_moves_a_workspace_onto_a_newer_commit() {
    let home = tempfile::tempdir().unwrap();
    let data = tempfile::Builder::new().prefix("data").tempdir().unwrap();
    let repo = data.path().join("site");
    std::fs::create_dir(&repo).unwrap();
    for (name, text) in [("both.txt", "b0\n"), ("up.txt", "u0\n"), ("same.txt", "s0\n"), ("gone.txt", "g\n")] {
        std::fs::write(repo.join(name), text).unwrap();
    }
    assert!(Command::new("git").current_dir(&repo).args(["init", "-q", "-b", "main"]).status().unwrap().success());
    commit_all(&repo, "first");
    let url = format!("file://{}", repo.display());
    let served = sandfs(home.path(), &["serve", "--mcp", &url, "--name", "site"], "");
    assert!(served.status.success(), "{}", String::from_utf8_lossy(&served.stderr));
    mcp(
        home.path(),
        "site",
        &[
            ("write", serde_json::json!({"path": "/mnt/mine.txt", "content": "m\n"})),
            ("write", serde_json::json!({"path": "/mnt/both.txt", "content": "mine\n"})),
            ("write", serde_json::json!({"path": "/mnt/same.txt", "content": "s1\n"})),
            ("delete", serde_json::json!({"path": "/mnt/gone.txt"})),
        ],
    );
    let fork = sandfs(home.path(), &["fork", "site", "--name", "before"], "");
    assert!(fork.status.success(), "{}", String::from_utf8_lossy(&fork.stderr));

    // Upstream: one file changed on both sides, one the same way, one only there.
    std::fs::write(repo.join("both.txt"), "theirs\n").unwrap();
    std::fs::write(repo.join("up.txt"), "u1\n").unwrap();
    std::fs::write(repo.join("same.txt"), "s1\n").unwrap();
    std::fs::remove_file(repo.join("gone.txt")).unwrap();
    std::fs::write(repo.join("new.txt"), "n\n").unwrap();
    commit_all(&repo, "second");

    let moved = sandfs(home.path(), &["update", "site"], "");
    assert!(moved.status.success(), "{}", String::from_utf8_lossy(&moved.stderr));
    assert_eq!(String::from_utf8_lossy(&moved.stdout), "C both.txt\n");
    let said = String::from_utf8_lossy(&moved.stderr);
    assert!(said.contains("5 files changed there") && said.contains("2 of its changes matched"), "{said}");

    // Only the workspace's own changes are left, over the newer commit's files.
    assert_eq!(stdout(home.path(), &["changes", "site"]), "M both.txt\nA mine.txt\n");
    let read = mcp(
        home.path(),
        "site",
        &[
            ("read", serde_json::json!({"path": "/mnt/up.txt"})),
            ("read", serde_json::json!({"path": "/mnt/new.txt"})),
            ("read", serde_json::json!({"path": "/mnt/both.txt"})),
        ],
    );
    assert!(read.contains("u1") && read.contains(r#"n\n"#) && read.contains("mine"), "{read}");
    // Files the newer commit changed report the time of the move, for build tools.
    let record = std::fs::read_to_string(home.path().join("workspaces/site/workspace.json")).unwrap();
    let record: serde_json::Value = serde_json::from_str(&record).unwrap();
    let updated: Vec<&String> = record["updated"].as_object().unwrap().keys().collect();
    assert_eq!(updated, ["both.txt", "new.txt", "same.txt", "up.txt"]);

    // The fork stays on the commit it had.
    assert_eq!(stdout(home.path(), &["changes", "before"]), "M both.txt\nD gone.txt\nA mine.txt\nM same.txt\n");
    // Nothing newer: nothing moves, and nothing is reported again.
    let again = sandfs(home.path(), &["update", "site"], "");
    assert!(again.status.success(), "{}", String::from_utf8_lossy(&again.stderr));
    assert_eq!(String::from_utf8_lossy(&again.stdout), "");
    assert_eq!(stdout(home.path(), &["changes", "site"]), "M both.txt\nA mine.txt\n");

    // Another repository is refused.
    let other = data.path().join("other");
    std::fs::create_dir(&other).unwrap();
    std::fs::write(other.join("a.txt"), "a\n").unwrap();
    assert!(Command::new("git").current_dir(&other).args(["init", "-q"]).status().unwrap().success());
    commit_all(&other, "a");
    let refused = sandfs(home.path(), &["update", "site", &format!("git:{}", other.display())], "");
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("another repository"));
}

#[test]
fn update_reads_a_relative_source_where_it_was_made_and_refuses_what_it_cannot_lay_over() {
    let home = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let repo = data.path().join("site");
    std::fs::create_dir_all(repo.join("dir")).unwrap();
    std::fs::write(repo.join("dir/file.txt"), "f0\n").unwrap();
    std::fs::write(repo.join("other.txt"), "o0\n").unwrap();
    assert!(Command::new("git").current_dir(&repo).args(["init", "-q", "-b", "main"]).status().unwrap().success());
    commit_all(&repo, "first");
    let input = format!(
        "{}\n",
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "write", "arguments": {"path": "/mnt/dir/file.txt", "content": "mine\n"}}})
    );
    let served = sandfs_in(home.path(), data.path(), &["serve", "--mcp", "git:site@main", "--name", "site"], &input);
    assert!(served.status.success(), "{}", String::from_utf8_lossy(&served.stderr));

    // Run from elsewhere, the update still finds the repository it was made from.
    std::fs::write(repo.join("other.txt"), "o1\n").unwrap();
    commit_all(&repo, "second");
    let moved = sandfs_in(home.path(), home.path(), &["update", "site"], "");
    assert!(moved.status.success(), "{}", String::from_utf8_lossy(&moved.stderr));
    assert_eq!(stdout(home.path(), &["changes", "site"]), "M dir/file.txt\n");

    // A file where the folder holding the workspace's change was is refused, and the
    // workspace stays as it was. A save names the folders above a change; one that
    // didn't, as a save may, couldn't be laid over the file at all.
    let index = home.path().join("workspaces/site/changes.json");
    let mut saved: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&index).unwrap()).unwrap();
    assert!(saved["entries"].as_object_mut().unwrap().remove("dir").is_some());
    std::fs::write(&index, saved.to_string()).unwrap();
    std::fs::remove_dir_all(repo.join("dir")).unwrap();
    std::fs::write(repo.join("dir"), "now a file\n").unwrap();
    commit_all(&repo, "third");
    let refused = sandfs(home.path(), &["update", "site"], "");
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("dir is a file in the newer source"), "{refused:?}");
    assert_eq!(stdout(home.path(), &["changes", "site"]), "M dir/file.txt\n");
}

#[test]
fn update_moves_a_workspace_onto_another_folder() {
    let (home, data) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (v1, v2) = (data.path().join("v1"), data.path().join("v2"));
    for (dir, survey, notes) in [(&v1, "old\n", "n\n"), (&v2, "new\n", "n\n")] {
        std::fs::create_dir_all(dir.join("raw")).unwrap();
        std::fs::write(dir.join("survey.csv"), survey).unwrap();
        std::fs::write(dir.join("notes.txt"), notes).unwrap();
        std::fs::write(dir.join("raw/a.csv"), "a\n").unwrap();
    }
    std::fs::write(v2.join("raw/b.csv"), "b\n").unwrap();
    let served = sandfs(home.path(), &["serve", "--mcp", v1.to_str().unwrap(), "--name", "data"], "");
    assert!(served.status.success(), "{}", String::from_utf8_lossy(&served.stderr));
    mcp(
        home.path(),
        "data",
        &[
            ("write", serde_json::json!({"path": "/mnt/survey.csv", "content": "mine\n"})),
            ("write", serde_json::json!({"path": "/mnt/notes.txt", "content": "edited\n"})),
            ("delete", serde_json::json!({"path": "/mnt/raw", "recursive": true})),
        ],
    );

    let same = sandfs(home.path(), &["update", "data"], "");
    assert!(!same.status.success());
    assert!(String::from_utf8_lossy(&same.stderr).contains("nothing newer"));

    let moved = sandfs(home.path(), &["update", "data", v2.to_str().unwrap()], "");
    assert!(moved.status.success(), "{}", String::from_utf8_lossy(&moved.stderr));
    // The folder deleted here got a new file there.
    assert_eq!(String::from_utf8_lossy(&moved.stdout), "C raw\nC survey.csv\n");
    assert_eq!(stdout(home.path(), &["changes", "data"]), "M notes.txt\nD raw/a.csv\nD raw/b.csv\nM survey.csv\n");
    assert!(stdout(home.path(), &["list"]).starts_with(&format!("data\t{}\t", v2.display())));
}

/// Git's output in `dir`, which must succeed.
#[cfg(unix)]
fn git_in(dir: &std::path::Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).unwrap()
}

#[cfg(unix)]
#[test]
fn a_fork_of_a_clone_can_be_kept_as_a_real_folder() {
    use std::os::unix::fs::PermissionsExt;
    let (home, data) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let home = home.path();
    let repo = data.path().join("repo");
    std::fs::create_dir_all(repo.join("docs")).unwrap();
    git_in(data.path(), &["init", "-q", "-b", "main", "repo"]);
    std::fs::write(repo.join("README.md"), "hello\n").unwrap();
    std::fs::write(repo.join("docs/guide.md"), "guide\n").unwrap();
    std::fs::write(repo.join("run.sh"), "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(repo.join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(repo.join(".gitignore"), "*.log\n").unwrap();
    commit_all(&repo, "first");
    // Uncommitted work comes along; ignored files don't.
    std::fs::write(repo.join("README.md"), "hello, edited\n").unwrap();
    std::fs::write(repo.join("draft.txt"), "draft\n").unwrap();
    std::fs::write(repo.join("build.log"), "ignored\n").unwrap();
    let before = git_in(&repo, &["status", "--porcelain"]);

    let task = data.path().join("task");
    let at = task.to_str().unwrap();
    let made = stdout(home, &["fork", repo.to_str().unwrap(), at, "--branch", "task/one", "--real-folder"]);
    let task = task.canonicalize().unwrap();
    assert_eq!(made, format!("task kept at {}\n", task.display()));
    assert!(!std::fs::read_to_string("/proc/mounts").unwrap_or_default().contains(at), "not a mount");
    assert_eq!(std::fs::read_to_string(task.join("README.md")).unwrap(), "hello, edited\n");
    assert_eq!(std::fs::metadata(task.join("run.sh")).unwrap().permissions().mode() & 0o111, 0o111);
    assert!(!task.join("build.log").exists(), "ignored files stay out");
    assert_eq!(
        git_in(&task, &["status", "--porcelain", "-b"]),
        format!("## task/one\n{before}").replace("?? build.log\n", "")
    );
    assert_eq!(stdout(home, &["changes", at]), "M README.md\nA draft.txt\n");
    assert_eq!(
        stdout(home, &["list"]),
        format!("task\tgit:{}\t{}\n", repo.canonicalize().unwrap().display(), task.display())
    );

    // Work in the folder shows as the workspace's changes; a file changed back doesn't.
    std::fs::write(task.join("notes.txt"), "notes\n").unwrap();
    std::fs::remove_file(task.join("docs/guide.md")).unwrap();
    std::os::unix::fs::symlink("README.md", task.join("readme")).unwrap();
    std::fs::set_permissions(task.join("run.sh"), std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
        stdout(home, &["changes", "task"]),
        "M README.md\nD docs/guide.md\nA draft.txt\nA notes.txt\nA readme\nM run.sh\n"
    );
    std::fs::write(task.join("README.md"), "hello\n").unwrap();
    assert_eq!(stdout(home, &["changes", "task", "docs", "README.md"]), "D docs/guide.md\n");

    // Its commits go back as a branch, and the clone's own work is left alone.
    git_in(&task, &["add", "-A"]);
    git_in(&task, &["commit", "-qm", "task"]);
    git_in(&task, &["push", "-q", "origin", "task/one"]);
    assert_eq!(git_in(&repo, &["log", "--format=%s", "task/one"]), "task\nfirst\n");
    assert_eq!(git_in(&repo, &["status", "--porcelain"]), before);

    // A fork of the folder, from a folder in it, is a folder too, with its files.
    let second = data.path().join("second");
    stdout(home, &["fork", task.join("docs").to_str().unwrap(), second.to_str().unwrap(), "--real-folder"]);
    assert_eq!(std::fs::read_to_string(second.join("notes.txt")).unwrap(), "notes\n");
    std::fs::write(second.join("more.txt"), "more\n").unwrap();
    assert_eq!(stdout(home, &["changes", "second", "--since", "task"]), "A more.txt\n");

    // Settings git runs programs from are shown apart.
    std::fs::create_dir(task.join(".git/hooks")).unwrap();
    std::fs::write(task.join(".git/hooks/pre-commit"), "#!/bin/sh\n").unwrap();
    let changed = sandfs(home, &["changes", "task"], "");
    assert!(String::from_utf8_lossy(&changed.stderr).contains(".git/hooks/pre-commit changed"));

    // Written back into the clone as files.
    stdout(home, &["changes", "task", "notes.txt", "--copy-to", repo.to_str().unwrap()]);
    assert_eq!(std::fs::read_to_string(repo.join("notes.txt")).unwrap(), "notes\n");

    // What only a mount does is refused; removing it takes the folder along when asked.
    let refused = |args: &[&str], message: &str| {
        let output = sandfs(home, args, "");
        assert!(!output.status.success(), "sandfs {args:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(message), "sandfs {args:?}: {stderr}");
    };
    let other = data.path().join("other");
    let other = other.to_str().unwrap();
    refused(&["mount", "task", other], "kept as the folder");
    refused(&["update", "task"], "kept as the folder");
    refused(&["unmount", at], "not mounted");
    refused(&["serve", "--mcp", "task"], "kept as the folder");
    refused(&["rm", "task"], "pass --unmount");
    refused(&["fork", repo.to_str().unwrap(), other, "--real-folder", "--read-only", "docs"], "needs a mount");
    refused(&["fork", repo.to_str().unwrap(), "--name", "snap", "--real-folder"], "give DIR");
    std::fs::create_dir(other).unwrap();
    std::fs::write(data.path().join("other/file"), "").unwrap();
    refused(&["fork", repo.to_str().unwrap(), other, "--real-folder"], "not empty");
    stdout(home, &["rm", "--unmount", at]);
    assert!(!task.exists());
    stdout(home, &["rm", "--unmount", "second"]);
    assert_eq!(stdout(home, &["list"]), "");
    assert!(std::fs::read_dir(data.path().join("other")).unwrap().count() == 1, "a folder it didn't make stays");
}
