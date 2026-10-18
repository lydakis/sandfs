//! `sandfs serve --mcp`: hand a saved workspace to an agent as MCP file tools.
//!
//! The tools come from `sandfs-mcp`; this only opens the workspace and saves what the
//! agent changes, so a later `mount` or `changes` sees it. The source's files are
//! under `/mnt`, as in the mount.

use std::io::{BufRead, Write};

use std::time::SystemTime;

use anyhow::{Result, bail};
use sandfs_core::VPath;
use sandfs_mcp::Server;
use serde_json::{Value, json};

use crate::persist::{Restored, Snapshot};
use crate::store::{Home, ROOT};

/// Serve `name` over stdin and stdout until the client closes stdin.
pub fn serve(home: &Home, name: &str) -> Result<()> {
    serve_on(home, name, std::io::stdin().lock(), std::io::stdout().lock())
}

/// Serve `name`, reading requests from `input` and answering on `output`.
fn serve_on(home: &Home, name: &str, input: impl BufRead, mut output: impl Write) -> Result<()> {
    // A mount keeps its own copy in memory and saves over ours, so one writer at a time.
    if let Some(info) = home.mount_info(name) {
        bail!("{name} is mounted at {}; unmount it first", info.mountpoint.display());
    }
    let opened = home.open(name)?;
    let root = VPath::parse(ROOT)?;
    let (mut saved, Restored { mut times, mut dirs }) = (opened.saved, opened.restored);
    saved.collect_garbage();
    let mut server = Server::new(opened.ws);
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
        let Some(touched) = touched_paths(&request) else {
            if let Some(reply) = server.handle(&line) {
                writeln!(output, "{reply}")?;
                output.flush()?;
            }
            continue;
        };
        // Only changes inside the source's mount can be saved, so refuse the rest.
        let reply = match touched.iter().find(|path| !path.starts_with(&root) || **path == root) {
            Some(outside) => {
                tool_error(&request, &format!("{outside} is outside {ROOT}; only files under {ROOT} are kept"))
            }
            None => server.handle(&line),
        };
        if reply.as_deref().is_some_and(succeeded) {
            // Changes reports files only: record touched directories and deletions so
            // they are saved too, and give changed paths a new modification time.
            let ws = server.workspace();
            for path in &touched {
                match ws.stat(path.as_str()) {
                    Ok(meta) => {
                        times.insert(path.to_string(), SystemTime::now());
                        if meta.kind.is_dir() {
                            dirs.insert(path.to_string());
                        }
                    }
                    Err(_) => {
                        times.remove(path.as_str());
                        dirs.insert(path.to_string());
                    }
                }
            }
            // Save before answering, so a reply means the change is on disk.
            let snapshot = Snapshot { ws: ws.fork(), times: times.clone(), dirs: dirs.clone() };
            saved.save(&snapshot, &root, true)?;
        }
        if let Some(reply) = reply {
            writeln!(output, "{reply}")?;
            output.flush()?;
        }
    }
    Ok(())
}

/// The paths a tool call that changes files would change, or `None` for any other message.
fn touched_paths(request: &Value) -> Option<Vec<VPath>> {
    if request["method"] != "tools/call" {
        return None;
    }
    let params = &request["params"];
    let keys: &[&str] = match params["name"].as_str()? {
        "write" | "edit" | "delete" => &["path"],
        "move" => &["from", "to"],
        _ => return None,
    };
    // A malformed path is left to the tool to reject.
    Some(keys.iter().filter_map(|key| VPath::parse(params["arguments"][key].as_str()?).ok()).collect())
}

/// Whether a reply reports a tool call that succeeded.
fn succeeded(reply: &str) -> bool {
    serde_json::from_str::<Value>(reply).is_ok_and(|reply| reply["result"]["isError"] == false)
}

/// A tool error answering `request`, in the shape the MCP tools use.
fn tool_error(request: &Value, message: &str) -> Option<String> {
    let id = request.get("id")?;
    let result = json!({ "content": [{ "type": "text", "text": message }], "isError": true });
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_calls_are_saved_to_the_workspace() {
        let (state, data) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        std::fs::write(data.path().join("notes.txt"), "draft\n").unwrap();
        let home = Home::at(state.path());
        let record = home.resolve(data.path().to_str().unwrap()).unwrap();
        home.create("ws", &record).unwrap();

        let requests = [
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {
                "name": "write", "arguments": { "path": "/mnt/summary.txt", "content": "done\n" } } }),
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
                "name": "read", "arguments": { "path": "/mnt/notes.txt" } } }),
        ];
        let input: String = requests.iter().map(|r| format!("{r}\n")).collect();
        let mut output = Vec::new();
        serve_on(&home, "ws", input.as_bytes(), &mut output).unwrap();

        let replies: Vec<Value> =
            String::from_utf8(output).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(replies.len(), 2);
        assert!(replies[1]["result"]["content"][0]["text"].as_str().unwrap().contains("draft"));
        let reopened = home.open("ws").unwrap();
        assert_eq!(reopened.ws.read_text("/mnt/summary.txt").unwrap(), "done\n");
        assert!(!data.path().join("summary.txt").exists(), "the source folder is untouched");
    }

    fn call(id: u64, tool: &str, arguments: Value) -> String {
        let request = json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": { "name": tool, "arguments": arguments } });
        format!("{request}\n")
    }

    fn session(home: &Home, input: &str) -> Vec<Value> {
        let mut output = Vec::new();
        serve_on(home, "ws", input.as_bytes(), &mut output).unwrap();
        String::from_utf8(output).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    #[test]
    fn later_sessions_keep_earlier_changes_and_directory_removals() {
        let (state, data) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        std::fs::create_dir_all(data.path().join("old/empty")).unwrap();
        std::fs::write(data.path().join("old/rows.csv"), "1\n").unwrap();
        let home = Home::at(state.path());
        home.create("ws", &home.resolve(data.path().to_str().unwrap()).unwrap()).unwrap();

        session(&home, &call(1, "write", json!({ "path": "/mnt/a.txt", "content": "first\n" })));
        let replies = session(
            &home,
            &[
                call(1, "write", json!({ "path": "/mnt/b.txt", "content": "second\n" })),
                call(2, "delete", json!({ "path": "/mnt/old", "recursive": true })),
                call(3, "write", json!({ "path": "/outside.txt", "content": "x" })),
            ]
            .concat(),
        );
        assert_eq!(replies[1]["result"]["isError"], false);
        assert_eq!(replies[2]["result"]["isError"], true, "a change outside /mnt is refused, not lost");

        let reopened = home.open("ws").unwrap();
        assert_eq!(reopened.ws.read_text("/mnt/a.txt").unwrap(), "first\n", "an earlier object is not overwritten");
        assert_eq!(reopened.ws.read_text("/mnt/b.txt").unwrap(), "second\n");
        assert!(!reopened.ws.exists("/mnt/old"), "a removed directory stays removed");
        assert!(reopened.restored.times.contains_key("/mnt/b.txt"), "changed files get a modification time");
    }
}
