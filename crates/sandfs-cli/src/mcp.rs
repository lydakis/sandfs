//! `sandfs serve --mcp`: hand a saved workspace to an agent as MCP file tools.
//!
//! The tools come from `sandfs-mcp`; this only opens the workspace and saves what the
//! agent changes, so a later `mount` or `changes` sees it. The source's files are
//! under `/mnt`, as in the mount.

use std::io::{BufRead, Write};

use anyhow::{Result, bail};
use sandfs_core::VPath;
use sandfs_mcp::Server;
use serde_json::Value;

use crate::persist::Snapshot;
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
    let (mut saved, restored) = (opened.saved, opened.restored);
    let mut server = Server::new(opened.ws);
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let reply = server.handle(&line);
        // Save after every tool call, before answering, so a reply means it is on disk.
        if is_tool_call(&line) {
            let snapshot =
                Snapshot { ws: server.workspace().fork(), times: restored.times.clone(), dirs: restored.dirs.clone() };
            saved.save(&snapshot, &root, true)?;
        }
        if let Some(reply) = reply {
            writeln!(output, "{reply}")?;
            output.flush()?;
        }
    }
    Ok(())
}

fn is_tool_call(line: &str) -> bool {
    serde_json::from_str::<Value>(line).is_ok_and(|request| request["method"] == "tools/call")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

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
}
