//! Behavior tests for the file tools, called directly on a server.

use std::sync::Arc;

use bytes::Bytes;
use sandfs_core::{Kind, MemorySource, Workspace};
use sandfs_mcp::{MAX_OUTPUT, Server};
use serde_json::{Value, json};

fn server() -> Server {
    let mut source = MemorySource::from_files([
        ("README.md", "# demo\nhello world\n"),
        ("src/lib.rs", "pub fn one() -> u32 {\n    1\n}\n\npub fn two() -> u32 {\n    2\n}\n"),
        ("src/util/mod.rs", "// TODO: helpers\n"),
        ("docs/guide.md", "Hello again\n"),
        ("data/cities.csv", "city,population\nAthens,664046\nOslo,709037\n"),
    ])
    .unwrap();
    source.insert("logo.png", Bytes::from_static(b"\x89PNG\0\0"), Kind::File { executable: false }).unwrap();
    source.insert("latest", Bytes::from_static(b"README.md"), Kind::Symlink).unwrap();
    let mut ws = Workspace::new();
    ws.mount("/repo", Arc::new(source)).unwrap();
    Server::new(ws)
}

/// Call a tool that must succeed and return its text.
fn ok(server: &mut Server, tool: &str, args: Value) -> String {
    let out = server.call_tool(tool, &args).expect("known tool");
    assert!(!out.is_error, "{tool} failed: {}", out.text);
    out.text
}

/// Call a tool that must fail and return its error text.
fn fails(server: &mut Server, tool: &str, args: Value) -> String {
    let out = server.call_tool(tool, &args).expect("known tool");
    assert!(out.is_error, "{tool} unexpectedly succeeded: {}", out.text);
    out.text
}

#[test]
fn read_numbers_lines_and_reports_the_version() {
    let mut s = server();
    assert_eq!(
        ok(&mut s, "read", json!({ "path": "/repo/README.md" })),
        "     1\t# demo\n     2\thello world\n[version 0 · lines 1-2 of 2]"
    );
    let page = ok(&mut s, "read", json!({ "path": "/repo/src/lib.rs", "offset": 2, "limit": "2" }));
    assert_eq!(page, "     2\t    1\n     3\t}\n[version 0 · lines 2-3 of 7 · continue with offset=4]");
    assert!(fails(&mut s, "read", json!({ "path": "/repo/README.md", "offset": 9 })).contains("past the end"));
}

#[test]
fn read_handles_binaries_symlinks_and_bad_paths() {
    let mut s = server();
    assert_eq!(
        ok(&mut s, "read", json!({ "path": "/repo/logo.png" })),
        "/repo/logo.png: binary file, 6 bytes, version 0"
    );
    assert!(ok(&mut s, "read", json!({ "path": "/repo/latest" })).contains("hello world"));
    assert_eq!(fails(&mut s, "read", json!({ "path": "/repo/nope" })), "/repo/nope: no such file or directory");
    assert!(fails(&mut s, "read", json!({ "path": "repo/README.md" })).contains("paths are absolute"));
    assert!(fails(&mut s, "read", json!({})).contains("missing required argument `path`"));
    assert!(fails(&mut s, "read", json!({ "path": "/repo/src" })).contains("is a directory"));
}

#[test]
fn list_shows_kinds_sizes_and_depth() {
    let mut s = server();
    assert_eq!(ok(&mut s, "list", json!({})), "repo/");
    assert_eq!(
        ok(&mut s, "list", json!({ "path": "/repo" })),
        "README.md (19 bytes)\ndata/\ndocs/\nlatest -> README.md\nlogo.png (6 bytes)\nsrc/"
    );
    let deep = ok(&mut s, "list", json!({ "path": "/repo/src", "depth": 3 }));
    assert_eq!(deep, "lib.rs (61 bytes)\nutil/\nutil/mod.rs (17 bytes)");
}

#[test]
fn glob_matches_relative_and_absolute_patterns() {
    let mut s = server();
    assert_eq!(
        ok(&mut s, "glob", json!({ "pattern": "**/*.md", "path": "/repo" })),
        "/repo/README.md\n/repo/docs/guide.md"
    );
    assert_eq!(ok(&mut s, "glob", json!({ "pattern": "*.rs", "path": "/repo/src" })), "/repo/src/lib.rs");
    assert_eq!(ok(&mut s, "glob", json!({ "pattern": "/repo/src/**" })), "/repo/src/lib.rs\n/repo/src/util/mod.rs");
    assert!(ok(&mut s, "glob", json!({ "pattern": "*.go" })).starts_with("No files match"));
    assert!(fails(&mut s, "glob", json!({ "pattern": "[" })).contains("invalid glob"));
}

#[test]
fn grep_finds_matches_with_context_and_filters() {
    let mut s = server();
    // The symlink to README.md and the binary file are skipped.
    assert_eq!(
        ok(&mut s, "grep", json!({ "pattern": "hello", "ignore_case": true })),
        "/repo/README.md:2:hello world\n/repo/docs/guide.md:1:Hello again\n[2 matches in 2 files]"
    );
    assert_eq!(
        ok(&mut s, "grep", json!({ "pattern": "fn two", "path": "/repo/src", "context": 1 })),
        "/repo/src/lib.rs-4-\n/repo/src/lib.rs:5:pub fn two() -> u32 {\n/repo/src/lib.rs-6-    2\n[1 matches in 1 file]"
    );
    assert_eq!(
        ok(&mut s, "grep", json!({ "pattern": "TODO", "glob": "*.rs" })),
        "/repo/src/util/mod.rs:1:// TODO: helpers\n[1 matches in 1 file]"
    );
    let capped = ok(&mut s, "grep", json!({ "pattern": "pub", "path": "/repo/src/lib.rs", "max": 1 }));
    assert_eq!(capped, "/repo/src/lib.rs:1:pub fn one() -> u32 {\n[1 matches in 1 file; stopped at max=1]");
    assert!(ok(&mut s, "grep", json!({ "pattern": "zzz" })).starts_with("No matches"));
    assert!(fails(&mut s, "grep", json!({ "pattern": "(" })).contains("invalid regex"));
}

#[test]
fn write_and_edit_use_versions() {
    let mut s = server();
    assert_eq!(
        ok(&mut s, "write", json!({ "path": "/notes/a.txt", "content": "one\n" })),
        "Wrote /notes/a.txt (4 bytes), version 1"
    );
    let conflict = fails(&mut s, "write", json!({ "path": "/notes/a.txt", "content": "x", "expected_version": 0 }));
    assert!(conflict.contains("version mismatch (expected 0, found 1)"));

    let edited = ok(
        &mut s,
        "edit",
        json!({ "path": "/repo/src/lib.rs", "expected_version": 0, "edits": [{ "old": "    2\n", "new": "    22\n" }] }),
    );
    assert!(edited.starts_with("Edited /repo/src/lib.rs, version 2\n"));
    assert!(edited.contains("-    2\n+    22\n"), "{edited}");

    let stale = json!({ "path": "/repo/src/lib.rs", "expected_version": 0, "edits": [{ "old": "1", "new": "3" }] });
    assert!(fails(&mut s, "edit", stale).contains("version mismatch"));
    let ambiguous = json!({ "path": "/repo/src/lib.rs", "edits": [{ "old": "pub fn", "new": "fn" }] });
    assert!(fails(&mut s, "edit", ambiguous).contains("matches 2 times"));
    let all = json!({ "path": "/repo/src/lib.rs", "edits": [{ "old": "pub fn", "new": "fn", "replace_all": true }] });
    ok(&mut s, "edit", all);
    assert!(!s.workspace().read_text("/repo/src/lib.rs").unwrap().contains("pub"));
    assert!(fails(&mut s, "edit", json!({ "path": "/repo/README.md", "edits": [] })).contains("empty"));
}

#[test]
fn move_and_delete_change_the_tree() {
    let mut s = server();
    assert_eq!(
        ok(&mut s, "move", json!({ "from": "/repo/docs", "to": "/repo/manual" })),
        "Moved /repo/docs to /repo/manual"
    );
    assert!(s.workspace().exists("/repo/manual/guide.md"));
    assert!(fails(&mut s, "delete", json!({ "path": "/repo/src" })).contains("directory not empty"));
    ok(&mut s, "delete", json!({ "path": "/repo/src", "recursive": true }));
    assert!(!s.workspace().exists("/repo/src/lib.rs"));
    assert!(fails(&mut s, "delete", json!({ "path": "/repo" })).contains("mount point"));
}

#[test]
fn diff_lists_changes_then_shows_a_unified_diff() {
    let mut s = server();
    assert_eq!(ok(&mut s, "diff", json!({})), "No changes under /");
    ok(&mut s, "edit", json!({ "path": "/repo/README.md", "edits": [{ "old": "world", "new": "sandfs" }] }));
    ok(&mut s, "write", json!({ "path": "/repo/NEW.md", "content": "new\n" }));
    ok(&mut s, "delete", json!({ "path": "/repo/docs/guide.md" }));
    ok(&mut s, "write", json!({ "path": "/repo/logo.png", "content": "not a png" }));

    let diff = ok(&mut s, "diff", json!({}));
    let expected_head = "A /repo/NEW.md\nM /repo/README.md\nD /repo/docs/guide.md\nM /repo/logo.png\n\n";
    assert!(diff.starts_with(expected_head), "{diff}");
    assert!(diff.contains("--- /dev/null\n+++ b/repo/NEW.md\n@@ -0,0 +1 @@\n+new\n"), "{diff}");
    assert!(diff.contains(
        "--- a/repo/README.md\n+++ b/repo/README.md\n@@ -1,2 +1,2 @@\n # demo\n-hello world\n+hello sandfs\n"
    ));
    assert!(diff.contains("--- a/repo/docs/guide.md\n+++ /dev/null\n@@ -1 +0,0 @@\n-Hello again\n"), "{diff}");
    assert!(diff.contains("Binary file /repo/logo.png differs\n"));

    let scoped = ok(&mut s, "diff", json!({ "path": "/repo/docs" }));
    assert!(scoped.starts_with("D /repo/docs/guide.md\n\n"), "{scoped}");
    assert!(!scoped.contains("README"));
}

#[test]
fn large_results_are_cut_with_a_note() {
    let mut s = server();
    let line = "x".repeat(99) + "\n";
    ok(&mut s, "write", json!({ "path": "/big.txt", "content": line.repeat(5000) }));

    let read = ok(&mut s, "read", json!({ "path": "/big.txt" }));
    assert!(read.len() <= MAX_OUTPUT);
    assert!(read.contains("of 5000 · continue with offset="), "{}", &read[read.len() - 100..]);

    let grep = ok(&mut s, "grep", json!({ "pattern": "x", "max": 10000 }));
    assert!(grep.len() <= MAX_OUTPUT);
    assert!(grep.ends_with("output cut short, narrow the path or glob]"));

    let diff = ok(&mut s, "diff", json!({}));
    assert!(diff.len() <= MAX_OUTPUT);
    assert!(diff.ends_with("[output cut at 64 KiB; narrow the request, e.g. with a path, offset or limit]"));

    ok(&mut s, "write", json!({ "path": "/long.txt", "content": "y".repeat(5000) }));
    assert!(ok(&mut s, "read", json!({ "path": "/long.txt" })).contains("… [line cut at 2000 characters]"));
}

#[test]
fn unknown_tools_and_bad_arguments() {
    let mut s = server();
    assert!(s.call_tool("bash", &json!({})).is_none());
    assert_eq!(
        ok(&mut s, "grep", json!({ "pattern": "^Oslo,", "glob": "*.csv" })),
        "/repo/data/cities.csv:3:Oslo,709037\n[1 matches in 1 file]"
    );
    assert!(fails(&mut s, "read", json!(["/repo/README.md"])).contains("must be a JSON object"));
    assert!(fails(&mut s, "read", json!({ "path": "/repo/README.md", "limit": -1 })).contains("non-negative integer"));
    assert!(fails(&mut s, "delete", json!({ "path": "/x", "recursive": "yes" })).contains("true or false"));
}

#[test]
fn read_notes_line_endings_that_edits_must_match() {
    let mut s = server();
    ok(&mut s, "write", json!({ "path": "/crlf.txt", "content": "a\r\nb" }));
    let read = ok(&mut s, "read", json!({ "path": "/crlf.txt" }));
    assert!(read.ends_with("CRLF line endings: use \\r\\n in edit text · no newline at end of file]"), "{read}");
    ok(&mut s, "edit", json!({ "path": "/crlf.txt", "edits": [{ "old": "a\r\nb", "new": "c\r\nd" }] }));
}

#[test]
fn diff_reports_kind_changes_and_skips_huge_patches() {
    let mut s = server();
    ok(&mut s, "delete", json!({ "path": "/repo/latest" }));
    ok(&mut s, "write", json!({ "path": "/repo/latest", "content": "now a file\n" }));
    ok(&mut s, "write", json!({ "path": "/huge.txt", "content": "z\n".repeat(600_000) }));
    let diff = ok(&mut s, "diff", json!({}));
    assert!(diff.starts_with("A /huge.txt\nM /repo/latest\n\n"), "{diff}");
    assert!(diff.contains("/huge.txt: too large to diff here; read it in pages\n"));
    assert!(diff.contains("/repo/latest: changed from a symlink to a file\n--- a/repo/latest\n"), "{diff}");
    assert!(diff.contains("-README.md\n") && diff.contains("+now a file\n"), "{diff}");
}

#[test]
fn grep_reports_unreadable_files_when_nothing_matches() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("gone.txt"), "x").unwrap();
    let mut ws = Workspace::new();
    ws.mount("/d", Arc::new(sandfs_core::DirSource::new(dir.path()).unwrap())).unwrap();
    ws.list("/d").unwrap();
    std::fs::remove_file(dir.path().join("gone.txt")).unwrap();
    let mut s = Server::new(ws);
    assert_eq!(
        ok(&mut s, "grep", json!({ "pattern": "x", "path": "/d" })),
        "No matches for x under /d (1 files could not be read)"
    );
}

#[test]
fn read_returns_images_as_images() {
    let mut s = server();
    let png = b"\x89PNG\r\n\x1a\nrest of the image";
    let mut ws = s.workspace().fork();
    ws.write("/chart.png", Bytes::from_static(png)).unwrap();
    s = Server::new(ws);
    let out = s.call_tool("read", &json!({ "path": "/chart.png" })).unwrap();
    assert!(!out.is_error);
    assert_eq!(out.text, "/chart.png: image/png image, 25 bytes, version 1");
    let image = out.image.expect("an image");
    assert_eq!((image.mime_type, image.data.as_ref()), ("image/png", &png[..]));
    // Files that only look a little like images stay binary.
    assert!(s.call_tool("read", &json!({ "path": "/repo/logo.png" })).unwrap().image.is_none());
}
