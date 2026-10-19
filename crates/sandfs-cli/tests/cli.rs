//! The `sandfs` binary without a mount, as on macOS and Windows.

use std::io::Write;
use std::process::{Command, Output, Stdio};

fn sandfs(home: &std::path::Path, args: &[&str], input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_sandfs"))
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
}
