//! The `sandfs-mcp` binary speaking MCP over stdio, as a client would drive it.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use serde_json::{Value, json};

/// Send `requests` one per line and collect every reply line.
fn exchange(args: &[&str], requests: &[Value]) -> Vec<Value> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_sandfs-mcp"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    for request in requests {
        writeln!(stdin, "{request}").unwrap();
    }
    writeln!(stdin, "not json").unwrap();
    drop(stdin);
    let replies = BufReader::new(child.stdout.take().unwrap())
        .lines()
        .map(|line| serde_json::from_str(&line.unwrap()).unwrap())
        .collect();
    assert!(child.wait().unwrap().success());
    replies
}

#[test]
fn a_session_over_a_mounted_directory() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("hello.txt"), "hello\n").unwrap();
    std::fs::write(dir.path().join("dot.gif"), b"GIF89a\x01\x00").unwrap();
    let mount = format!("/work={}", dir.path().display());

    let replies = exchange(
        &["--dir", &mount],
        &[
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "test", "version": "0" } } }),
            json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
            json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {
                "name": "edit", "arguments": { "path": "/work/hello.txt", "edits": [{ "old": "hello", "new": "bye" }] } } }),
            json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {
                "name": "read", "arguments": { "path": "/work/missing" } } }),
            json!({ "jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": { "name": "bash", "arguments": {} } }),
            json!({ "jsonrpc": "2.0", "id": "six", "method": "resources/list" }),
            json!({ "jsonrpc": "2.0", "id": 7, "method": "ping" }),
            json!({ "jsonrpc": "2.0", "method": 1 }),
            json!({ "jsonrpc": "2.0", "id": 9, "method": "tools/call", "params": {
                "name": "read", "arguments": { "path": "/work/dot.gif" } } }),
        ],
    );
    // Every request plus the trailing unparseable line; only the notification gets no reply.
    assert_eq!(replies.len(), 10, "{replies:#?}");

    let init = &replies[0]["result"];
    assert_eq!(init["protocolVersion"], "2025-06-18");
    assert_eq!(init["serverInfo"]["name"], "sandfs");
    assert!(init["instructions"].as_str().unwrap().contains("/work/"));

    let names: Vec<&str> =
        replies[1]["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["read", "list", "glob", "grep", "write", "edit", "move", "delete", "diff"]);

    assert_eq!(replies[2]["result"]["isError"], false);
    assert_eq!(replies[2]["result"]["content"][0]["text"], "Edited /work/hello.txt, version 1");
    // The host file is untouched: the edit lives in the workspace.
    assert_eq!(std::fs::read_to_string(dir.path().join("hello.txt")).unwrap(), "hello\n");

    assert_eq!(replies[3]["result"]["isError"], true);
    assert_eq!(replies[3]["result"]["content"][0]["text"], "/work/missing: no such file or directory");
    assert_eq!(replies[4]["error"]["code"], -32602);
    assert_eq!(replies[5]["id"], "six");
    assert_eq!(replies[5]["error"]["code"], -32601);
    assert_eq!(replies[6]["result"], json!({}));
    assert_eq!(replies[6]["id"], 7);
    assert_eq!(replies[7]["error"]["code"], -32600, "a request without an id is still validated");
    assert_eq!(replies[7]["id"], Value::Null);
    let gif = &replies[8]["result"]["content"];
    assert_eq!(gif.as_array().unwrap().len(), 1, "non-text files are described, never sent as bytes");
    assert_eq!(gif[0]["text"], "/work/dot.gif: GIF image, 8 bytes, version 0; not text, so not shown");
    assert_eq!(replies[9]["error"]["code"], -32700);
}

#[test]
fn unparseable_lines_get_a_parse_error() {
    let replies = exchange(&[], &[]);
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0]["error"]["code"], -32700);
    assert_eq!(replies[0]["id"], Value::Null);
}

#[test]
fn bad_arguments_exit_with_usage() {
    let output = Command::new(env!("CARGO_BIN_EXE_sandfs-mcp")).args(["--dir", "nonsense"]).output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("expected MOUNT=PATH"));
}
