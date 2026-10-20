//! `sandfs serve --mcp`: hand a saved workspace to an agent as MCP file tools.
//!
//! The tools come from `sandfs-mcp`; this only opens the workspace and saves what the
//! agent changes, so a later `mount` or `changes` sees it. The source's files are
//! under `/mnt`, as in the mount.

use std::io::{BufRead, Write};

use std::collections::BTreeSet;
use std::time::SystemTime;

use anyhow::Result;
use sandfs_core::{VPath, Workspace};
use sandfs_mcp::Server;
use serde_json::{Value, json};

use crate::persist::{NotDurable, Restored, Snapshot};
use crate::store::{Base, Home, ROOT, subtree_keys};

/// Serve `name` over stdin and stdout until the client closes stdin.
pub fn serve(home: &Home, name: &str) -> Result<()> {
    serve_on(home, name, std::io::stdin().lock(), std::io::stdout().lock())
}

/// Serve `name`, reading requests from `input` and answering on `output`.
fn serve_on(home: &Home, name: &str, input: impl BufRead, mut output: impl Write) -> Result<()> {
    // A mount or another session keeps its own copy in memory and would save over
    // ours, so one writer at a time.
    let _writer = home.lock(name, &format!("an MCP session (process {})", std::process::id()))?;
    let opened = home.open(name)?;
    let root = VPath::parse(ROOT)?;
    let git_dir = matches!(opened.record.base, Base::Git { .. }).then(|| root.join(".git")).transpose()?;
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
        // The state before the call, to tell which entries it creates or removes, and
        // to go back to if its change cannot be saved.
        let before = (server.workspace().fork(), times.clone(), dirs.clone());
        let below_root = |p: &VPath| p.starts_with(&root) && *p != root;
        let existed: Vec<bool> = touched.iter().map(|p| server.workspace().exists(p.as_str())).collect();
        let missing: Vec<VPath> = touched
            .iter()
            .flat_map(VPath::ancestors)
            .filter(|p| below_root(p) && !server.workspace().exists(p.as_str()))
            .collect();
        // Only changes inside the source's mount can be saved, so refuse the rest. A git
        // mount's `.git` is git's to write: its config names commands git runs.
        let reply = if let Some(outside) = touched.iter().find(|path| !path.starts_with(&root) || **path == root) {
            tool_error(&request, &format!("{outside} is outside {ROOT}; only files under {ROOT} are kept"))
        } else if let Some(git) = touched.iter().find(|path| git_dir.as_ref().is_some_and(|dir| path.starts_with(dir)))
        {
            tool_error(&request, &format!("{git} is in the git folder, which only git changes"))
        } else {
            server.handle(&line)
        };
        if reply.as_deref().is_some_and(succeeded) {
            // Changes reports files only, so record the directories a call may have
            // created (parents, a moved subtree) and the paths it removed, and give
            // changed paths a new modification time.
            let ws = server.workspace();
            let now = SystemTime::now();
            // A directory changes when an entry in it appears or goes, as on a real filesystem.
            for path in missing.iter().filter(|p| ws.exists(p.as_str())) {
                times.insert(path.to_string(), now);
            }
            for (path, existed) in touched.iter().zip(existed) {
                if let Some(parent) = path.parent().filter(|p| existed != ws.exists(path.as_str()) && below_root(p)) {
                    times.insert(parent.to_string(), now);
                }
            }
            // Only `move` touches two paths; its files keep their times at the new place.
            if let [from, to] = touched.as_slice() {
                for old in subtree_keys(&times, from) {
                    if let Some(time) = times.remove(&old) {
                        times.insert(format!("{to}{}", &old[from.as_str().len()..]), time);
                    }
                }
            }
            for path in &touched {
                for parent in path.ancestors().into_iter().filter(below_root) {
                    dirs.insert(parent.to_string());
                }
                match ws.stat(path.as_str()) {
                    Ok(meta) => {
                        times.insert(path.to_string(), now);
                        if meta.kind.is_dir() {
                            add_subtree(ws, path, &mut dirs);
                        }
                    }
                    Err(_) => {
                        for gone in subtree_keys(&times, path) {
                            times.remove(&gone);
                        }
                        dirs.insert(path.to_string());
                    }
                }
            }
            // Save before answering, so a reply means the change is on disk. A change
            // that cannot be saved is undone and reported, so the agent never builds on
            // a change that a restart would lose.
            let snapshot = Snapshot { ws: ws.fork(), times: times.clone(), dirs: dirs.clone() };
            let result = saved.save(&snapshot, &root, true);
            // Saved files are read from disk from now on.
            for (path, version, stored) in saved.take_written() {
                let _ = server.workspace_mut().store(path.as_str(), version, stored);
            }
            if let Err(err) = result {
                // Past the point where the new index is in place, the change stays: a
                // restart would bring it back, so it can't be reported as undone.
                let message = if err.downcast_ref::<NotDurable>().is_some() {
                    format!("the change was made, but a crash could still lose it: {err:#}")
                } else {
                    let ws;
                    (ws, times, dirs) = before;
                    server = Server::new(ws);
                    format!("the change could not be saved, so it was undone: {err:#}")
                };
                if let Some(reply) = tool_error(&request, &message) {
                    writeln!(output, "{reply}")?;
                    output.flush()?;
                }
                continue;
            }
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

/// Add `dir` and every directory below it to `dirs`.
fn add_subtree(ws: &Workspace, dir: &VPath, dirs: &mut BTreeSet<String>) {
    dirs.insert(dir.to_string());
    for entry in ws.list(dir.as_str()).unwrap_or_default() {
        if let (true, Ok(child)) = (entry.kind.is_dir(), dir.join(&entry.name)) {
            add_subtree(ws, &child, dirs);
        }
    }
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
    fn an_agent_cannot_change_a_git_mounts_git_folder() {
        let (state, data) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git").arg("-C").arg(data.path()).args(args).output().unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        };
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(data.path().join("a.txt"), "a\n").unwrap();
        git(&["add", "."]);
        git(&["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false", "commit", "-qm", "a"]);
        let home = Home::at(state.path());
        home.create("ws", &home.resolve(&format!("git:{}", data.path().display())).unwrap()).unwrap();

        let input = [
            call(
                1,
                "edit",
                json!({ "path": "/mnt/.git/config", "old": "[core]", "new": "[core]\n\tfsmonitor = evil" }),
            ),
            call(2, "write", json!({ "path": "/mnt/.git/hooks/pre-commit", "content": "evil" })),
            call(3, "move", json!({ "from": "/mnt/a.txt", "to": "/mnt/.git/a.txt" })),
            call(4, "write", json!({ "path": "/mnt/b.txt", "content": "b" })),
        ]
        .concat();
        let replies = session(&home, &input);
        for reply in &replies[..3] {
            assert_eq!(reply["result"]["isError"], true, "{reply}");
            assert!(reply["result"]["content"][0]["text"].as_str().unwrap().contains("the git folder"), "{reply}");
        }
        assert_eq!(replies[3]["result"]["isError"], false);
        let ws = home.open("ws").unwrap().ws;
        assert!(!ws.read_text("/mnt/.git/config").unwrap().contains("evil"));
        assert!(!ws.exists("/mnt/.git/hooks/pre-commit") && ws.exists("/mnt/a.txt") && ws.exists("/mnt/b.txt"));
    }

    #[test]
    fn a_workspace_in_use_is_refused() {
        let (state, data) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let home = Home::at(state.path());
        home.create("ws", &home.resolve(data.path().to_str().unwrap()).unwrap()).unwrap();

        let mount = home.lock("ws", "the mount at /work (process 7)").unwrap();
        let input = call(1, "write", json!({ "path": "/mnt/a.txt", "content": "a" }));
        let refused = serve_on(&home, "ws", input.as_bytes(), Vec::new()).unwrap_err();
        assert_eq!(refused.to_string(), "ws is in use by the mount at /work (process 7)");
        drop(mount);
        assert!(!home.open("ws").unwrap().ws.exists("/mnt/a.txt"), "the refused session changed nothing");
        session(&home, &input);
        assert!(home.open("ws").unwrap().ws.exists("/mnt/a.txt"));
    }

    #[test]
    fn a_change_that_cannot_be_saved_is_undone() {
        let (state, data) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        std::fs::write(data.path().join("notes.txt"), "draft\n").unwrap();
        let home = Home::at(state.path());
        home.create("ws", &home.resolve(data.path().to_str().unwrap()).unwrap()).unwrap();
        session(&home, &call(1, "write", json!({ "path": "/mnt/kept.txt", "content": "kept" })));

        // A directory where the save writes its new index makes every save fail, as a
        // full disk would.
        let blocker = home.workspace_dir("ws").join("changes.json.tmp");
        std::fs::create_dir(&blocker).unwrap();
        let replies = session(
            &home,
            &[
                call(1, "write", json!({ "path": "/mnt/lost.txt", "content": "lost" })),
                call(2, "delete", json!({ "path": "/mnt/notes.txt" })),
                call(3, "read", json!({ "path": "/mnt/notes.txt" })),
                call(4, "list", json!({ "path": "/mnt" })),
            ]
            .concat(),
        );
        for reply in &replies[..2] {
            assert_eq!(reply["result"]["isError"], true);
            let text = reply["result"]["content"][0]["text"].as_str().unwrap();
            assert!(text.starts_with("the change could not be saved, so it was undone"), "{text}");
        }
        assert!(replies[2]["result"]["content"][0]["text"].as_str().unwrap().contains("draft"), "delete was undone");
        let listing = replies[3]["result"]["content"][0]["text"].as_str().unwrap();
        assert!(listing.contains("kept.txt") && !listing.contains("lost.txt"), "{listing}");

        std::fs::remove_dir(&blocker).unwrap();
        session(&home, &call(1, "write", json!({ "path": "/mnt/later.txt", "content": "later" })));
        let reopened = home.open("ws").unwrap();
        assert_eq!(reopened.ws.read_text("/mnt/kept.txt").unwrap(), "kept");
        assert_eq!(reopened.ws.read_text("/mnt/later.txt").unwrap(), "later");
        assert!(!reopened.ws.exists("/mnt/lost.txt"));
        assert!(reopened.ws.exists("/mnt/notes.txt"));
    }

    #[test]
    fn moved_files_keep_their_saved_times() {
        let (state, data) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let home = Home::at(state.path());
        home.create("ws", &home.resolve(data.path().to_str().unwrap()).unwrap()).unwrap();

        session(&home, &call(1, "write", json!({ "path": "/mnt/dir/f.txt", "content": "f" })));
        let written = home.open("ws").unwrap().restored.times["/mnt/dir/f.txt"];
        session(&home, &call(1, "move", json!({ "from": "/mnt/dir", "to": "/mnt/moved" })));

        let times = home.open("ws").unwrap().restored.times;
        assert_eq!(times.get("/mnt/moved/f.txt"), Some(&written));
        assert!(!times.contains_key("/mnt/dir/f.txt"));
    }

    #[test]
    fn folders_get_a_time_when_their_entries_change() {
        let (state, data) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        std::fs::create_dir(data.path().join("old")).unwrap();
        std::fs::write(data.path().join("old/a.txt"), "a").unwrap();
        let home = Home::at(state.path());
        home.create("ws", &home.resolve(data.path().to_str().unwrap()).unwrap()).unwrap();

        let calls = [
            call(1, "write", json!({ "path": "/mnt/new/deeper/f.txt", "content": "f" })),
            call(2, "delete", json!({ "path": "/mnt/old/a.txt" })),
        ];
        session(&home, &calls.concat());

        let times = home.open("ws").unwrap().restored.times;
        for dir in ["/mnt/new", "/mnt/new/deeper", "/mnt/old"] {
            assert!(times.contains_key(dir), "{dir} has a saved time");
        }
    }

    #[test]
    fn later_sessions_keep_earlier_changes_and_directory_removals() {
        let (state, data) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        std::fs::create_dir_all(data.path().join("old/empty")).unwrap();
        std::fs::create_dir_all(data.path().join("keep/inner")).unwrap();
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
                call(4, "move", json!({ "from": "/mnt/keep", "to": "/mnt/moved" })),
                call(5, "write", json!({ "path": "/mnt/deep/er/c.txt", "content": "c" })),
                call(6, "delete", json!({ "path": "/mnt/deep/er/c.txt" })),
            ]
            .concat(),
        );
        assert_eq!(replies[1]["result"]["isError"], false);
        assert_eq!(replies[2]["result"]["isError"], true, "a change outside /mnt is refused, not lost");

        let reopened = home.open("ws").unwrap();
        assert_eq!(reopened.ws.read_text("/mnt/a.txt").unwrap(), "first\n", "an earlier object is not overwritten");
        assert_eq!(reopened.ws.read_text("/mnt/b.txt").unwrap(), "second\n");
        assert!(!reopened.ws.exists("/mnt/old"), "a removed directory stays removed");
        assert!(reopened.ws.exists("/mnt/moved/inner"), "an empty directory moves with its parent");
        assert!(!reopened.ws.exists("/mnt/keep"));
        assert!(reopened.ws.exists("/mnt/deep/er"), "parents a write created are kept");
        assert!(reopened.restored.times.contains_key("/mnt/b.txt"), "changed files get a modification time");
    }
}
