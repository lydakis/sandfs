//! Behavior tests for the file tools, called directly on a server.

use std::sync::Arc;

use sandfs_core::{MemorySource, Workspace};
use sandfs_mcp::{MAX_OUTPUT, Server};
use serde_json::{Value, json};

const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";

fn server() -> Server {
    let source = MemorySource::from_files([
        ("README.md", b"# Q3 data\nhello world\n".to_vec()),
        ("sales/2026.csv", b"region,total\nnorth,120\nsouth,95\neast,130\n".to_vec()),
        ("sales/notes/todo.txt", b"TODO: check east\n".to_vec()),
        ("docs/guide.md", b"Hello again\n".to_vec()),
        ("chart.png", PNG.to_vec()),
    ])
    .unwrap();
    let mut ws = Workspace::new();
    ws.mount("/data", Arc::new(source)).unwrap();
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
fn read_numbers_text_lines_and_reports_the_version() {
    let mut s = server();
    assert_eq!(
        ok(&mut s, "read", json!({ "path": "/data/README.md" })),
        "     1\t# Q3 data\n     2\thello world\n[version 0 · lines 1-2 of 2]"
    );
    let page = ok(&mut s, "read", json!({ "path": "/data/sales/2026.csv", "offset": 2, "limit": "2" }));
    assert_eq!(page, "     2\tnorth,120\n     3\tsouth,95\n[version 0 · lines 2-3 of 4 · continue with offset=4]");
    assert!(fails(&mut s, "read", json!({ "path": "/data/README.md", "offset": 9 })).contains("past the end"));
}

#[test]
fn read_describes_other_files_without_their_bytes() {
    let mut s = server();
    assert_eq!(
        ok(&mut s, "read", json!({ "path": "/data/chart.png" })),
        "/data/chart.png: PNG image, 16 bytes, version 0; not text, so not shown"
    );
    ok(&mut s, "write", json!({ "path": "/report.pdf", "content": "JVBERi0xLjcKJeLjz9MK", "encoding": "base64" }));
    assert!(ok(&mut s, "read", json!({ "path": "/report.pdf" })).starts_with("/report.pdf: PDF document, 15 bytes"));
    ok(&mut s, "write", json!({ "path": "/blob", "content": "AAEC", "encoding": "base64" }));
    assert!(ok(&mut s, "read", json!({ "path": "/blob" })).starts_with("/blob: binary file, 3 bytes"));
}

#[test]
fn read_reports_bad_paths() {
    let mut s = server();
    assert_eq!(fails(&mut s, "read", json!({ "path": "/data/nope" })), "/data/nope: no such file or directory");
    assert!(fails(&mut s, "read", json!({ "path": "data/README.md" })).contains("paths are absolute"));
    assert!(fails(&mut s, "read", json!({})).contains("missing required argument `path`"));
    assert!(fails(&mut s, "read", json!({ "path": "/data/sales" })).contains("is a directory"));
}

#[test]
fn list_shows_kinds_sizes_and_depth() {
    let mut s = server();
    assert_eq!(ok(&mut s, "list", json!({})), "data/");
    assert_eq!(
        ok(&mut s, "list", json!({ "path": "/data" })),
        "README.md (22 bytes)\nchart.png (16 bytes)\ndocs/\nsales/"
    );
    let deep = ok(&mut s, "list", json!({ "path": "/data/sales", "depth": 3 }));
    assert_eq!(deep, "2026.csv (41 bytes)\nnotes/\nnotes/todo.txt (17 bytes)");
    assert_eq!(fails(&mut s, "list", json!({ "depth": 21 })), "depth must be at most 20");
}

#[test]
fn glob_matches_relative_and_absolute_patterns() {
    let mut s = server();
    assert_eq!(
        ok(&mut s, "glob", json!({ "pattern": "**/*.md", "path": "/data" })),
        "/data/README.md\n/data/docs/guide.md"
    );
    assert_eq!(ok(&mut s, "glob", json!({ "pattern": "*.csv", "path": "/data/sales" })), "/data/sales/2026.csv");
    assert_eq!(
        ok(&mut s, "glob", json!({ "pattern": "/data/sales/**" })),
        "/data/sales/2026.csv\n/data/sales/notes/todo.txt"
    );
    assert!(ok(&mut s, "glob", json!({ "pattern": "*.xlsx" })).starts_with("No files match"));
    assert!(fails(&mut s, "glob", json!({ "pattern": "[" })).contains("invalid glob"));
}

#[test]
fn grep_searches_text_files_with_context_and_filters() {
    let mut s = server();
    // The PNG is not text, so it is skipped.
    assert_eq!(
        ok(&mut s, "grep", json!({ "pattern": "hello", "ignore_case": true })),
        "/data/README.md:2:hello world\n/data/docs/guide.md:1:Hello again\n[2 matches in 2 files]"
    );
    assert_eq!(
        ok(&mut s, "grep", json!({ "pattern": "^south", "path": "/data/sales", "context": 1 })),
        "/data/sales/2026.csv-2-north,120\n/data/sales/2026.csv:3:south,95\n/data/sales/2026.csv-4-east,130\n\
         [1 matches in 1 file]"
    );
    assert_eq!(
        ok(&mut s, "grep", json!({ "pattern": "east", "glob": "*.txt" })),
        "/data/sales/notes/todo.txt:1:TODO: check east\n[1 matches in 1 file]"
    );
    let capped = ok(&mut s, "grep", json!({ "pattern": "[0-9]", "path": "/data/sales/2026.csv", "max": 1 }));
    assert_eq!(capped, "/data/sales/2026.csv:2:north,120\n[1 matches in 1 file; stopped at max=1]");
    assert!(ok(&mut s, "grep", json!({ "pattern": "zzz" })).starts_with("No matches"));
    assert!(fails(&mut s, "grep", json!({ "pattern": "(" })).contains("invalid regex"));
}

#[test]
fn write_takes_text_or_base64_and_checks_versions() {
    let mut s = server();
    assert_eq!(
        ok(&mut s, "write", json!({ "path": "/notes/a.txt", "content": "one\n" })),
        "Wrote /notes/a.txt (4 bytes), version 1"
    );
    let conflict = fails(&mut s, "write", json!({ "path": "/notes/a.txt", "content": "x", "expected_version": 0 }));
    assert!(conflict.contains("version mismatch (expected 0, found 1)"));
    ok(&mut s, "write", json!({ "path": "/copy.png", "content": "iVBORw0KGgo=", "encoding": "base64" }));
    assert_eq!(s.workspace().read("/copy.png").unwrap().as_ref(), &PNG[..8]);
    let bad = json!({ "path": "/x", "content": "!!", "encoding": "base64" });
    assert!(fails(&mut s, "write", bad).contains("not valid base64"));
    assert!(
        fails(&mut s, "write", json!({ "path": "/x", "content": "", "encoding": "hex" })).contains("unknown encoding")
    );
}

#[test]
fn edit_replaces_exact_text_atomically() {
    let mut s = server();
    let path = "/data/sales/2026.csv";
    let first = json!({ "path": path, "expected_version": 0, "edits": [{ "old": "south,95", "new": "south,97" }] });
    assert_eq!(ok(&mut s, "edit", first), "Edited /data/sales/2026.csv, version 1");

    let stale = json!({ "path": path, "expected_version": 0, "edits": [{ "old": "north", "new": "North" }] });
    assert!(fails(&mut s, "edit", stale).contains("version mismatch"));
    let ambiguous = json!({ "path": path, "edits": [{ "old": "0\n", "new": "1\n" }] });
    assert!(fails(&mut s, "edit", ambiguous).contains("matches 2 times"));
    // A failing second edit leaves the first unapplied.
    let partial =
        json!({ "path": path, "edits": [{ "old": "north", "new": "North" }, { "old": "west", "new": "West" }] });
    assert!(fails(&mut s, "edit", partial).contains("text not found"));
    assert!(s.workspace().read(path).unwrap().starts_with(b"region,total\nnorth"));

    let all = json!({ "path": path, "edits": [{ "old": ",", "new": ";", "replace_all": true }] });
    ok(&mut s, "edit", all);
    assert_eq!(s.workspace().read(path).unwrap().as_ref(), b"region;total\nnorth;120\nsouth;97\neast;130\n");
    let image = json!({ "path": "/data/chart.png", "edits": [{ "old": "PNG", "new": "JPG" }] });
    assert!(fails(&mut s, "edit", image).contains("not a text file"));
    assert!(fails(&mut s, "edit", json!({ "path": path, "edits": [] })).contains("empty"));
    assert!(fails(&mut s, "edit", json!({ "path": path, "edits": [{ "old": "", "new": "x" }] })).contains("empty"));
}

#[test]
fn move_and_delete_change_the_tree() {
    let mut s = server();
    assert_eq!(
        ok(&mut s, "move", json!({ "from": "/data/docs", "to": "/data/manual" })),
        "Moved /data/docs to /data/manual"
    );
    assert!(s.workspace().exists("/data/manual/guide.md"));
    let onto = json!({ "from": "/data/README.md", "to": "/data/chart.png" });
    assert!(fails(&mut s, "move", onto).contains("already exists"));
    assert!(fails(&mut s, "delete", json!({ "path": "/data/sales" })).contains("directory not empty"));
    ok(&mut s, "delete", json!({ "path": "/data/sales", "recursive": true }));
    assert!(!s.workspace().exists("/data/sales/2026.csv"));
    assert!(fails(&mut s, "delete", json!({ "path": "/data" })).contains("mount point"));
}

#[test]
fn changes_lists_changed_files_with_sizes() {
    let mut s = server();
    assert_eq!(ok(&mut s, "changes", json!({})), "No changes under /");
    ok(&mut s, "edit", json!({ "path": "/data/README.md", "edits": [{ "old": "world", "new": "sandfs" }] }));
    ok(&mut s, "write", json!({ "path": "/data/summary.csv", "content": "total,345\n" }));
    ok(&mut s, "delete", json!({ "path": "/data/docs/guide.md" }));
    assert_eq!(
        ok(&mut s, "changes", json!({})),
        "M /data/README.md (23 bytes)\nD /data/docs/guide.md\nA /data/summary.csv (10 bytes)\n\
         [A added, M modified, D deleted; read a file to see its contents]"
    );
    assert!(ok(&mut s, "changes", json!({ "path": "/data/docs" })).starts_with("D /data/docs/guide.md\n["));
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

    ok(&mut s, "write", json!({ "path": "/long.txt", "content": "y".repeat(5000) }));
    assert!(ok(&mut s, "read", json!({ "path": "/long.txt" })).contains("… [line cut at 2000 characters]"));
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
fn unknown_tools_and_bad_arguments() {
    let mut s = server();
    assert!(s.call_tool("bash", &json!({})).is_none());
    assert!(fails(&mut s, "read", json!(["/data/README.md"])).contains("must be a JSON object"));
    assert!(fails(&mut s, "read", json!({ "path": "/data/README.md", "limit": -1 })).contains("non-negative integer"));
    assert!(fails(&mut s, "delete", json!({ "path": "/x", "recursive": "yes" })).contains("true or false"));
}
