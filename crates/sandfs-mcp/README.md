# sandfs-mcp

An MCP server that gives an agent a [sandfs](https://crates.io/crates/sandfs-core) workspace as nine file tools instead of a bash tool: `read`, `list`, `glob`, `grep`, `write`, `edit`, `move`, `delete` and `changes`. Mounts stay read-only, every write lands in a copy-on-write overlay, and `changes` lists the files the agent added, modified or deleted.

Install the binary with `cargo install sandfs-mcp` and point an MCP client at it. Each `--dir MOUNT=PATH` mounts a host folder, read lazily and never written to (mount one nothing else is changing while the server runs). `--read-only PATH` lets the agent read PATH but change nothing in it, even in its own copy; the server's instructions name such paths, and changes there fail with "read-only":

```json
{ "mcpServers": { "sandfs": {
    "command": "sandfs-mcp",
    "args": ["--dir", "/data=./datasets", "--dir", "/notes=./notes", "--read-only", "/data"]
} } }
```

A harness that builds its own workspace, with any sources, embeds the server and collects the changes when the session ends. To hand an agent a fork of a checkpoint, `Server::new(checkpoint.fork()).changes_since(checkpoint)` makes its `changes` tool show only its own work:

```rust,no_run
use std::io;
use std::sync::Arc;

use sandfs_core::{MemorySource, Workspace};
use sandfs_mcp::Server;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut workspace = Workspace::new();
    workspace.mount("/brief", Arc::new(MemorySource::from_files([("task.md", "Summarise the data\n")])?))?;
    let mut server = Server::new(workspace);
    server.serve(io::stdin().lock(), io::stdout())?;
    for change in server.into_workspace().changes()? {
        eprintln!("{:?} {}", change.kind, change.path); // stdout carries the MCP messages
    }
    Ok(())
}
```

The transport is MCP's stdio framing, one JSON-RPC 2.0 message per line, and only the tools capability is offered. See the [repository](https://github.com/lydakis/sandfs) for the design.

License: MIT
