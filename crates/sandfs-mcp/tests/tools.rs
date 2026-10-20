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
    assert_eq!(fails(&mut s, "grep", json!({ "pattern": "x", "context": 21 })), "context must be at most 20");
}

#[test]
fn grep_shows_matches_past_the_line_cut_and_read_takes_huge_limits() {
    let mut s = server();
    let wide = format!("{}needle{}", "a".repeat(600), "b".repeat(600));
    ok(&mut s, "write", json!({ "path": "/data/wide.json", "content": wide }));
    let hit = ok(&mut s, "grep", json!({ "pattern": "needle", "path": "/data/wide.json" }));
    assert!(hit.starts_with("/data/wide.json:1:[560 characters cut] …aaaa"), "{hit}");
    assert!(hit.contains("needlebbb") && hit.contains("… [line cut]"), "{hit}");
    let page = ok(&mut s, "read", json!({ "path": "/data/sales/2026.csv", "offset": 2, "limit": u64::MAX }));
    assert!(page.ends_with("[version 0 · lines 2-4 of 4]"), "{page}");
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
    let cut = ok(&mut s, "read", json!({ "path": "/long.txt" }));
    assert!(cut.contains("… [line cut at 2000 characters; rest from byte_offset=2000]"), "{cut}");
}

#[test]
fn byte_reads_reach_the_rest_of_long_lines() {
    let mut s = server();
    // A wide record: 3000 two-byte characters, then a marker past the line cut.
    let record = format!("{}END,42\nnext\n", "é".repeat(3000));
    ok(&mut s, "write", json!({ "path": "/data/wide.csv", "content": record }));
    let page = ok(&mut s, "read", json!({ "path": "/data/wide.csv" }));
    assert!(page.contains("rest from byte_offset=4000]"), "{page}");

    let rest = ok(&mut s, "read", json!({ "path": "/data/wide.csv", "byte_offset": 4000 }));
    assert!(rest.starts_with(&"é".repeat(1000)), "{rest}");
    assert!(rest.ends_with("END,42\nnext\n\n[bytes 4000-6012 · end of file]"), "{rest}");
    // Text found by bytes is exact, so it can be edited.
    ok(&mut s, "edit", json!({ "path": "/data/wide.csv", "edits": [{ "old": "éEND,42", "new": "éEND,43" }] }));

    // A range that starts or ends inside a character moves onto its boundaries.
    let mid = ok(&mut s, "read", json!({ "path": "/data/wide.csv", "byte_offset": 1, "byte_length": 6 }));
    assert_eq!(mid, "éé\n[bytes 2-6 · continue with byte_offset=6]");

    // Mounted files are read by range too.
    let head = ok(&mut s, "read", json!({ "path": "/data/sales/2026.csv", "byte_length": "12" }));
    assert_eq!(head, "region,total\n[bytes 0-12 · continue with byte_offset=12]");

    let err = |s: &mut Server, args| fails(s, "read", args);
    assert!(err(&mut s, json!({ "path": "/data/README.md", "byte_offset": 22 })).contains("past the end"));
    // A binary file stays hidden even where a range of it happens to be ASCII.
    let png = json!({ "path": "/data/chart.png", "byte_offset": 12, "byte_length": 4 });
    assert_eq!(err(&mut s, png), "/data/chart.png: PNG image; not text, so not shown");
    assert!(err(&mut s, json!({ "path": "/data/sales", "byte_offset": 0 })).contains("is a directory"));
    let split = json!({ "path": "/data/wide.csv", "byte_offset": 0, "byte_length": 1 });
    assert!(err(&mut s, split).contains("hold no whole character"));
    let ascii = ok(&mut s, "read", json!({ "path": "/data/README.md", "byte_length": 1 }));
    assert_eq!(ascii, "#\n[bytes 0-1 · continue with byte_offset=1]");
    assert!(err(&mut s, json!({ "path": "/data/README.md", "byte_offset": 0, "limit": 1 })).contains("not both"));
    assert_eq!(
        err(&mut s, json!({ "path": "/data/README.md", "byte_length": 60001 })),
        "byte_length must be at most 60000"
    );
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

#[test]
fn grep_cut_short_by_context_is_not_reported_as_no_matches() {
    let mut s = server();
    let dir = (0..30).map(|i| format!("{i:0>100}")).collect::<Vec<_>>().join("/");
    let path = format!("/data/{dir}/wide.txt");
    let text = format!("{}needle\n", format!("{}\n", "x".repeat(500)).repeat(20));
    ok(&mut s, "write", json!({ "path": path, "content": text }));
    let out = ok(&mut s, "grep", json!({ "pattern": "needle", "path": "/data", "context": 20 }));
    assert!(!out.starts_with("No matches"), "{}", &out[out.len() - 200..]);
    assert!(out.ends_with("output cut short, narrow the path or glob]"), "{}", &out[out.len() - 200..]);
}

/// A source that reports no sizes and refuses whole-file reads, like a large remote
/// object that only supports range requests.
#[derive(Debug)]
struct RangeOnly(&'static [u8]);

impl sandfs_core::Source for RangeOnly {
    fn root(&self) -> sandfs_core::Result<sandfs_core::SourceKey> {
        Ok("".into())
    }
    fn read_dir(&self, _key: &str) -> sandfs_core::Result<sandfs_core::Listing> {
        let file = sandfs_core::Kind::File { executable: false };
        Ok([("big.log".to_owned(), sandfs_core::SourceEntry { kind: file, key: "big.log".into(), size: None })].into())
    }
    fn read_file(&self, _key: &str) -> sandfs_core::Result<bytes::Bytes> {
        panic!("a byte read must not load the whole file");
    }
    fn read_range(&self, _key: &str, offset: u64, len: u64) -> sandfs_core::Result<bytes::Bytes> {
        let start = (offset as usize).min(self.0.len());
        Ok(bytes::Bytes::from_static(&self.0[start..(start + len as usize).min(self.0.len())]))
    }
}

#[test]
fn byte_reads_never_load_the_whole_file() {
    let mut ws = Workspace::new();
    ws.mount("/remote", Arc::new(RangeOnly(b"line one\nline two\n"))).unwrap();
    let mut s = Server::new(ws);
    let out = ok(&mut s, "read", json!({ "path": "/remote/big.log", "byte_offset": 5, "byte_length": 3 }));
    assert_eq!(out, "one\n[bytes 5-8 · continue with byte_offset=8]");
    // A range that ends exactly at the end of the file says so.
    let exact = ok(&mut s, "read", json!({ "path": "/remote/big.log", "byte_offset": 14, "byte_length": 4 }));
    assert_eq!(exact, "two\n\n[bytes 14-18 · end of file]");
    let tail = ok(&mut s, "read", json!({ "path": "/remote/big.log", "byte_offset": 9 }));
    assert_eq!(tail, "line two\n\n[bytes 9-18 · end of file]");
}

#[test]
fn paths_with_control_characters_are_shown_quoted() {
    // A newline in a name would otherwise split one path over two lines.
    let source = MemorySource::from_files([
        ("odd\nname.txt", b"needle\n".to_vec()),
        ("plain.txt", b"needle\n".to_vec()),
        ("sub\ndir/f.txt", b"x".to_vec()),
        ("next\u{85}line.txt", b"x".to_vec()),
        ("para\u{2029}sep", b"x".to_vec()),
        ("\"odd\\nname.txt\"", b"x".to_vec()),
    ])
    .unwrap();
    let mut ws = Workspace::new();
    ws.mount("/d", Arc::new(source)).unwrap();
    let mut s = Server::new(ws);
    assert_eq!(
        ok(&mut s, "list", json!({ "path": "/d" })),
        concat!(
            r#""\"odd\\nname.txt\"" (1 bytes)"#,
            "\n",
            r#""next\u0085line.txt" (1 bytes)"#,
            "\n",
            r#""odd\nname.txt" (7 bytes)"#,
            "\n",
            r#""para\u2029sep" (1 bytes)"#,
            "\nplain.txt (7 bytes)\n",
            r#""sub\ndir/""#,
        )
    );
    assert_eq!(
        ok(&mut s, "glob", json!({ "pattern": "*e*.txt", "path": "/d" })),
        "\"/d/next\\u0085line.txt\"\n\"/d/odd\\nname.txt\""
    );
    assert!(
        ok(&mut s, "grep", json!({ "pattern": "needle", "path": "/d" }))
            .starts_with("\"/d/odd\\nname.txt\":1:needle\n")
    );
    ok(&mut s, "write", json!({ "path": "/d/odd\nname.txt", "content": "x" }));
    assert!(ok(&mut s, "changes", json!({})).starts_with("M \"/d/odd\\nname.txt\" (1 bytes)\n"));

    // Results that find nothing name the path the same way.
    ok(&mut s, "delete", json!({ "path": "/d/sub\ndir/f.txt" }));
    assert_eq!(ok(&mut s, "list", json!({ "path": "/d/sub\ndir" })), "\"/d/sub\\ndir\" is empty");
    assert_eq!(
        ok(&mut s, "glob", json!({ "pattern": "*", "path": "/d/sub\ndir" })),
        "No files match * under \"/d/sub\\ndir\""
    );
    assert_eq!(
        ok(&mut s, "grep", json!({ "pattern": "y", "path": "/d/sub\ndir" })),
        "No matches for y under \"/d/sub\\ndir\""
    );
    assert_eq!(ok(&mut s, "changes", json!({ "path": "/d/sub\ndir/x" })), "No changes under \"/d/sub\\ndir/x\"");
}

#[test]
fn links_are_shown_with_their_targets_and_never_followed() {
    let source = MemorySource::from_files([("lib/site.py", "x = 1\n")]).unwrap();
    let mut ws = Workspace::new();
    ws.mount("/env", Arc::new(source)).unwrap();
    ws.symlink("/env/lib64", "lib").unwrap();
    ws.symlink("/env/two\nlines", "lib").unwrap();
    let mut s = Server::new(ws);

    assert!(ok(&mut s, "list", json!({ "path": "/env" })).starts_with("lib/\nlib64 -> lib\n"));
    assert!(ok(&mut s, "read", json!({ "path": "/env/lib64" })).starts_with("/env/lib64: symlink to lib, version "));
    let paged = ok(&mut s, "read", json!({ "path": "/env/lib64", "byte_offset": 0, "byte_length": 2 }));
    assert!(paged.starts_with("/env/lib64: symlink to lib, version "), "{paged}");
    assert!(ok(&mut s, "grep", json!({ "pattern": "x", "path": "/env" })).contains("1 matches in 1 file"));
    assert_eq!(
        ok(&mut s, "grep", json!({ "pattern": "x", "path": "/env/lib64" })),
        "No matches for x under /env/lib64"
    );
    assert!(ok(&mut s, "changes", json!({})).starts_with("A /env/lib64 -> lib\n"));
    assert!(fails(&mut s, "write", json!({ "path": "/env/lib64", "content": "y" })).contains("symbolic link"));
    let shown = ok(&mut s, "read", json!({ "path": "/env/two\nlines" }));
    assert!(shown.starts_with("\"/env/two\\nlines\": symlink to lib"), "one line: {shown}");
}

/// A source that lists a link whose target can't be fetched.
#[derive(Debug)]
struct DeadLink;

impl sandfs_core::Source for DeadLink {
    fn root(&self) -> sandfs_core::Result<sandfs_core::SourceKey> {
        Ok("".into())
    }
    fn read_dir(&self, _key: &str) -> sandfs_core::Result<sandfs_core::Listing> {
        let link = sandfs_core::SourceEntry { kind: sandfs_core::Kind::Symlink, key: "link".into(), size: Some(3) };
        Ok([("link".to_owned(), link)].into())
    }
    fn read_file(&self, _key: &str) -> sandfs_core::Result<bytes::Bytes> {
        Err(sandfs_core::Error::Source("connection reset".to_owned()))
    }
}

#[test]
fn a_link_whose_target_cannot_be_fetched_says_why() {
    let mut ws = Workspace::new();
    ws.mount("/remote", Arc::new(DeadLink)).unwrap();
    let mut s = Server::new(ws);
    assert!(fails(&mut s, "read", json!({ "path": "/remote/link" })).contains("connection reset"));
    let bytes = json!({ "path": "/remote/link", "byte_offset": 0 });
    assert!(fails(&mut s, "read", bytes).contains("connection reset"));
}
