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
